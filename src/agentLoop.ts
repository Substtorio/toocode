// Agent 的对话循环
//
// 一轮「对话」不一定是发一次请求就完了：
//
//   用户说话 → 发请求 → 模型说「我要看 a.txt」→ 我们读给它 → 再发请求
//            → 它可能还要看别的 → … → 它终于给出回答
//
// 这个「要工具 → 执行 → 喂回去」的循环就是 agent 的核心。
//
// ★★ 必须设轮数上限：模型犯轴的时候会不停要工具（尤其是小模型），
//    没有上限就是无限循环 + 持续烧钱

import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { createTurnCollector, createSseParser, type ChatTurn } from "./agentStream";
import { AGENT_TOOLS, parseToolArguments, runAgentTool } from "./agentTools";

/** 一轮对话里最多来回几次。够了就停下并告诉用户 */
const MAX_TURNS = 8;

/** 单次请求的兜底超时 —— 和 Rust 侧那个对齐，防止 `chat-end` 永远不来 */
const REQUEST_TIMEOUT_MS = 300_000;

export interface ChatMessage {
  role: "system" | "user" | "assistant" | "tool";
  /**
   * ⚠ 对 `assistant` 消息，带 `tool_calls` 时 content 可以是 `null` ——
   *   服务端对这种情况有要求，不能传空字符串
   */
  content: string | null;
  /** 只在 assistant 消息上出现 */
  tool_calls?: Array<{
    id: string;
    type: "function";
    function: { name: string; arguments: string };
  }>;
  /** 只在 tool 消息上出现 —— 要对应上发起的那个 tool_call */
  tool_call_id?: string;
}

export interface AgentConfig {
  /** 接口根地址，如 `https://api.deepseek.com/v1` */
  baseUrl: string;
  model: string;
}

export interface AgentCallbacks {
  /** 正文增量 */
  onText?(text: string): void;
  /** 思考过程增量（推理模型才有） */
  onReasoning?(text: string): void;
  onToolStart?(name: string, args: string): void;
  onToolEnd?(name: string, result: string): void;
  /** 循环结束。reason 说明为什么停 */
  onDone?(reason: "answered" | "max-turns" | "aborted"): void;
}

let requestSeq = 0;

/** 发一次请求，把流式响应解析完 */
async function chatOnce(
  config: AgentConfig,
  messages: ChatMessage[],
  callbacks: AgentCallbacks,
  signal: AbortSignal,
): Promise<ChatTurn> {
  const requestId = `chat-${++requestSeq}`;
  const parser = createSseParser();
  const collector = createTurnCollector();

  return new Promise<ChatTurn>((resolve, reject) => {
    let settled = false;
    let timer: number | undefined;

    const cleanup = () => {
      if (timer !== undefined) window.clearTimeout(timer);

      // ★ 每个退订单独包一层：**退订失败不该拖累后面的事**。
      //   最要命的是 finish() 的顺序是「先 cleanup、再 action」——
      //   一旦 cleanup 抛异常，那个 resolve / reject 就永远不会执行，
      //   Promise 永远挂着，界面卡在「思考中」，而且什么都不报。
      //   退订本身失败了无所谓：进程退出时监听器自然就没了
      for (const off of [offDelta, offEnd]) {
        try {
          off?.();
        } catch {
          // 故意吞掉：这里没有任何有意义的补救动作
        }
      }

      signal.removeEventListener("abort", onAbort);
    };

    const finish = (action: () => void) => {
      if (settled) return;
      settled = true;
      cleanup();
      action();
    };

    const onAbort = () => {
      finish(() => reject(new Error("已取消")));
    };

    // ⚠ 这两个是**异步**注册的，但下面要等它们都挂上才 invoke ——
    //   否则响应很快时 `chat-end` 可能在监听器注册之前就发出来了，
    //   然后就永远等不到（和终端那个「必须先 listen 再 spawn」是同一个坑）
    let offDelta: (() => void) | undefined;
    let offEnd: (() => void) | undefined;

    const ready = (async () => {
      offEnd = await listen<{ requestId: string; error: string | null }>("chat-end", (event) => {
        // 可能同时开着多个会话，只认自己那一份
        if (event.payload.requestId !== requestId) return;
        finish(() => {
          if (event.payload.error) reject(new Error(event.payload.error));
          else resolve(collector.finish());
        });
      });

      offDelta = await listen<{ requestId: string; text: string }>("chat-delta", (event) => {
        if (event.payload.requestId !== requestId) return;

        // ★ 一个事件块里可能是半行、也可能好几行 —— 缓冲交给解析器
        for (const chunk of parser.push(event.payload.text)) {
          if (chunk.content) callbacks.onText?.(chunk.content);
          if (chunk.reasoning) callbacks.onReasoning?.(chunk.reasoning);
          collector.feed(chunk);
        }
      });
    })();

    timer = window.setTimeout(() => {
      finish(() => reject(new Error("请求超时")));
    }, REQUEST_TIMEOUT_MS);

    if (signal.aborted) {
      onAbort();
      return;
    }
    signal.addEventListener("abort", onAbort);

    // 先挂监听，再发请求
    void ready
      .then(() =>
        invoke("chat_stream", {
          requestId,
          request: {
            baseUrl: config.baseUrl,
            model: config.model,
            messages,
            tools: AGENT_TOOLS,
          },
        }),
      )
      .catch((error: unknown) => {
        // ⚠ `invoke` 自己会失败（比如还没配密钥）——
        //   那时**不会**有 `chat-end` 事件，必须在这里兜住，
        //   否则这个 Promise 就永远挂着，UI 卡在「思考中」
        finish(() => reject(error instanceof Error ? error : new Error(String(error))));
      });
  });
}

/**
 * 跑完整的一轮：可能包含多次「要工具 → 执行 → 喂回去」。
 *
 * @param history 已有的对话历史（不含这一轮新产生的）
 * @param additions 本轮的增量消息由这里回传 —— 调用方拿它更新 UI，
 *                  不用自己猜循环里发生了什么
 */
export async function runAgent(
  config: AgentConfig,
  history: ChatMessage[],
  callbacks: AgentCallbacks,
  signal: AbortSignal,
  additions: ChatMessage[],
): Promise<void> {
  // ★ 复制一份再往里塞：不改调用方的数组，
  //   不然用户在流式过程中点了「重新发送」，历史就已经被污染了
  const messages = [...history];

  for (let turn = 1; turn <= MAX_TURNS; turn += 1) {
    if (signal.aborted) {
      callbacks.onDone?.("aborted");
      return;
    }

    const result = await chatOnce(config, messages, callbacks, signal);

    // 把这一轮模型的回复收进历史。
    // ⚠ `arguments` 是**字符串**（模型给的就是字符串）——
    //   解析成对象再传回去会被服务端拒掉
    const toolCalls = result.toolCalls.map((call) => ({
      id: call.id,
      type: "function" as const,
      function: { name: call.name, arguments: call.args },
    }));

    const assistantMessage: ChatMessage = {
      role: "assistant",
      // 带工具调用时 content 必须是 null，不能是空字符串
      content: result.content === "" ? (toolCalls.length > 0 ? null : "") : result.content,
      ...(toolCalls.length > 0 ? { tool_calls: toolCalls } : {}),
    };
    messages.push(assistantMessage);
    additions.push(assistantMessage);

    // 没有工具调用 ⇒ 它说完了
    if (toolCalls.length === 0) {
      callbacks.onDone?.("answered");
      return;
    }

    // 逐个执行。★ 串行而不是并行：
    //   这些工具都是读磁盘，并行不会更快，而且日志顺序会乱得没法看
    for (const call of toolCalls) {
      if (signal.aborted) {
        callbacks.onDone?.("aborted");
        return;
      }

      callbacks.onToolStart?.(call.function.name, call.function.arguments);

      const parsed = parseToolArguments(call.function.arguments);
      const output = parsed.ok
        ? await runAgentTool(call.function.name, parsed.value)
        : `错误：${parsed.error}`;

      callbacks.onToolEnd?.(call.function.name, output);

      const toolMessage: ChatMessage = {
        role: "tool",
        tool_call_id: call.id,
        content: output,
      };
      messages.push(toolMessage);
      additions.push(toolMessage);
    }
  }

  // 走到这儿说明用完了所有轮次 —— 得**明说**，
  // 不然用户只会看到「它突然不说话了」
  callbacks.onDone?.("max-turns");
}
