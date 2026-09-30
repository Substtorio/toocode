// 解析 OpenAI 兼容接口的流式响应（SSE）
//
// ★ 单独一个文件、零依赖 —— 和 `fuzzy.ts` / `pathUtils.ts` / `snippetParser.ts` 一个思路。
//   这段逻辑错了只表现为「回话内容错乱」，光看是分不清「解析错了」还是「模型就这样」的，
//   必须能单独跑用例
//
// 服务端吐出来的长这样：
//
//   data: {"choices":[{"delta":{"content":"你"}}]}
//   <空行>
//   data: {"choices":[{"delta":{"content":"好"}}]}
//   <空行>
//   data: [DONE]
//
// ★★ 为什么必须带缓冲：一个网络块**不保证**是一整行 ——
//   它可能刚好把 `data: {...}` 从中间切开，也可能一块里塞了三行。
//   所以不能拿到一块就 `JSON.parse`，得攒着、按行切、剩下的留着等下一块

/** 一次工具调用的增量 */
export interface ToolCallDelta {
  /** 模型给这次调用的序号，**同一个调用**的多个增量靠它归拢 */
  index: number;
  id?: string;
  /** 函数名。通常只在第一块里出现 */
  name?: string;
  /** 参数 JSON 的**片段** —— 它是逐段拼出来的，不是一次给完 */
  argsChunk?: string;
}

/** 从一块流式数据里解出来的东西 */
export interface ChatChunk {
  /** 正文增量 */
  content?: string;
  /** 推理过程增量（DeepSeek-R1 / o 系列那种「思考」字段） */
  reasoning?: string;
  toolCalls?: ToolCallDelta[];
  /** 非 null 表示这一轮结束了（`stop` / `tool_calls` / `length`） */
  finishReason?: string | null;
}

/** 一整轮回复攒完之后的样子 */
export interface ChatTurn {
  content: string;
  reasoning: string;
  toolCalls: Array<{ id: string; name: string; args: string }>;
  finishReason: string | null;
}

/** 把 `data:` 后面的那一段 JSON 解成 ChatChunk */
function parseDataLine(payload: string): ChatChunk | null {
  let raw: unknown;
  try {
    raw = JSON.parse(payload);
  } catch {
    // 半行 / 坏行都丢掉就是了 —— 丢一块的代价是少几个字，
    // 而整个中断会让用户以为「AI 挂了」
    return null;
  }

  if (raw === null || typeof raw !== "object") return null;

  const choices = (raw as { choices?: unknown }).choices;
  if (!Array.isArray(choices) || choices.length === 0) return null;

  const choice = choices[0] as { delta?: unknown; finish_reason?: unknown };
  const delta = (choice.delta ?? {}) as Record<string, unknown>;

  const chunk: ChatChunk = {};

  if (typeof delta.content === "string" && delta.content !== "") {
    chunk.content = delta.content;
  }

  // DeepSeek-R1 之类会把「思考」放在单独的字段里。
  // ⚠ 各家叫法不一样：`reasoning_content`（DeepSeek）/ `reasoning`（部分本地模型）
  const reasoning = delta.reasoning_content ?? delta.reasoning;
  if (typeof reasoning === "string" && reasoning !== "") {
    chunk.reasoning = reasoning;
  }

  if (Array.isArray(delta.tool_calls)) {
    const calls: ToolCallDelta[] = [];
    for (const item of delta.tool_calls) {
      if (item === null || typeof item !== "object") continue;
      const call = item as Record<string, unknown>;
      const fn = (call.function ?? {}) as Record<string, unknown>;

      calls.push({
        index: typeof call.index === "number" ? call.index : 0,
        id: typeof call.id === "string" ? call.id : undefined,
        name: typeof fn.name === "string" ? fn.name : undefined,
        argsChunk: typeof fn.arguments === "string" ? fn.arguments : undefined,
      });
    }
    if (calls.length > 0) chunk.toolCalls = calls;
  }

  if (typeof choice.finish_reason === "string") {
    chunk.finishReason = choice.finish_reason;
  }

  return chunk;
}

/**
 * 造一个有状态的解析器。
 *
 * ★ 为什么是「有状态」而不是纯函数：跨块的行是它必须记住的东西。
 *   把「攒缓冲」这件事收进对象里，调用方就只需要无脑 `push` 每一块
 */
export function createSseParser() {
  let buffer = "";

  return {
    /** 喂一块（Rust 侧 emit 过来的原始文本），返回能解出来的那些块 */
    push(text: string): ChatChunk[] {
      buffer += text;
      const chunks: ChatChunk[] = [];

      // ★ 只在**行尾**切。最后一段可能是不完整的行，留着等下一块 ——
      //   直接切成 "data: {\"cho" 去 parse 只会失败
      let newline = buffer.indexOf("\n");
      while (newline !== -1) {
        const line = buffer.slice(0, newline).trim();
        buffer = buffer.slice(newline + 1);

        // 空行是 SSE 的分隔符，跳过
        if (line.startsWith("data:")) {
          const payload = line.slice(5).trim();
          // `[DONE]` 是结束标记，不是一个 JSON
          if (payload !== "[DONE]") {
            const parsed = parseDataLine(payload);
            if (parsed !== null) chunks.push(parsed);
          }
        }

        newline = buffer.indexOf("\n");
      }

      return chunks;
    },
  };
}

/**
 * 把一堆增量块攒成完整的一轮回复。
 *
 * ★★ 工具调用是**分段拼**出来的，这是最容易漏的一处：
 *   模型不会一次性给你 `{"path":"a.txt"}`，而是像这样分几块：
 *     `{"` → `"path"` → `:"a.txt"}` → `}`
 *   而且**多路调用交错着来**，靠 `index` 归拢。所以必须按 index 累加，
 *   不能「见到一个就用一个」
 */
export function createTurnCollector() {
  let content = "";
  let reasoning = "";
  const calls = new Map<number, { id: string; name: string; args: string }>();
  let finishReason: string | null = null;

  return {
    feed(chunk: ChatChunk): void {
      if (chunk.content) content += chunk.content;
      if (chunk.reasoning) reasoning += chunk.reasoning;
      if (chunk.finishReason) finishReason = chunk.finishReason;

      for (const delta of chunk.toolCalls ?? []) {
        const existing = calls.get(delta.index) ?? { id: "", name: "", args: "" };
        // 补空而不是覆盖：id / name 一般只在第一块给，
        // 后面几块里没有这两个字段，覆盖会把已经拿到的东西抹掉
        if (delta.id) existing.id = delta.id;
        if (delta.name) existing.name = delta.name;
        if (delta.argsChunk) existing.args += delta.argsChunk;
        calls.set(delta.index, existing);
      }
    },

    /** 收尾。按 index 排好序 —— 模型的调用顺序就是 index 顺序 */
    finish(): ChatTurn {
      return {
        content,
        reasoning,
        toolCalls: [...calls.entries()]
          .sort(([a], [b]) => a - b)
          .map(([, call]) => call),
        finishReason,
      };
    },
  };
}
