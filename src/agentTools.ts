// Agent 能用的工具
//
// ★★ 读写两类工具的**地位完全不同**：
//    · 读（read_file / list_directory / search_in_folder）—— 直接执行，无害
//    · 写（write_file）—— **不直接写盘**，而是登记一条「待审改动」，
//      等用户看过 diff 说「保留」才真的落盘（见 agentChanges.ts）
//    一个跑偏的模型加上写权限能一口气毁掉整个项目，
//    而用户连它改了什么都不知道
//
// ★ 工具声明用 OpenAI 的 function calling 格式 ——
//   换任何兼容接口（DeepSeek / 通义 / Ollama）都不用改一个字

import { invoke } from "@tauri-apps/api/core";
import { proposeChange } from "./agentChanges";
import { getOpenDocument } from "./editorBridge";
import type { FileNode } from "./types";

/** OpenAI 的 function calling 工具声明 */
export interface ToolSpec {
  type: "function";
  function: {
    name: string;
    description: string;
    parameters: {
      type: "object";
      properties: Record<string, { type: string; description: string }>;
      required: string[];
    };
  };
}

/**
 * 工具结果的字符上限。
 *
 * ★★ 不能不给：模型读一个 3 万行的文件，整份塞回去会把上下文撑爆 ——
 *    要么报错，要么把前面几轮对话挤出去，表现是「它忘了我们刚才在说什么」
 */
const MAX_TOOL_RESULT_CHARS = 8000;

/** 列目录时最多列多少条 */
const MAX_DIR_ENTRIES = 200;

export const AGENT_TOOLS: ToolSpec[] = [
  {
    type: "function",
    function: {
      name: "read_file",
      description:
        "读取一个文本文件的完整内容。读图片、压缩包这类二进制文件会失败 —— 那是预期的，不要去重试。",
      parameters: {
        type: "object",
        properties: {
          path: { type: "string", description: "文件的绝对路径" },
        },
        required: ["path"],
      },
    },
  },
  {
    type: "function",
    function: {
      name: "list_directory",
      description:
        "列出目录下的文件和子目录（递归若干层）。用于了解项目结构。",
      parameters: {
        type: "object",
        properties: {
          path: { type: "string", description: "目录的绝对路径" },
        },
        required: ["path"],
      },
    },
  },
  {
    type: "function",
    function: {
      name: "search_in_folder",
      description:
        "在整个文件夹里按关键词搜索，返回命中的文件、行号和那一行的内容。想找「某个函数在哪里定义」时用它，比逐个文件读快得多。",
      parameters: {
        type: "object",
        properties: {
          path: { type: "string", description: "要搜索的文件夹绝对路径" },
          query: { type: "string", description: "要搜索的关键词，区分大小写" },
        },
        required: ["path", "query"],
      },
    },
  },
];

/**
 * 解析模型给的参数。
 *
 * ★ 单独拆出来是因为它**是模型给的字符串** ——
 *   模型完全可能吐出一段不合法的 JSON（尤其是小模型、或者被截断的时候）。
 *   这一步出错不该让整个循环崩掉，要变成一条「告诉模型它给错了」的工具结果，
 *   让它自己纠正
 */
export function parseToolArguments(raw: string): { ok: true; value: Record<string, unknown> } | { ok: false; error: string } {
  const text = raw.trim();
  if (text === "") return { ok: true, value: {} };

  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return { ok: false, error: `参数不是合法的 JSON：${text.slice(0, 200)}` };
  }

  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    return { ok: false, error: `参数应该是一个对象，实际是 ${Array.isArray(parsed) ? "数组" : typeof parsed}` };
  }

  return { ok: true, value: parsed as Record<string, unknown> };
}

/** 把一棵文件树拍成给人（和给模型）看的文本 */
function renderTree(nodes: readonly FileNode[], depth: number, out: string[]): void {
  for (const node of nodes) {
    if (out.length >= MAX_DIR_ENTRIES) return;
    const indent = "  ".repeat(depth);
    out.push(node.type === "folder" ? `${indent}${node.name}/` : `${indent}${node.name}`);
    if (node.children) renderTree(node.children, depth + 1, out);
  }
}

/** 超长结果要截断，而且**明说被截断了** —— 不说的话模型会以为这就是全部 */
function clamp(text: string): string {
  if (text.length <= MAX_TOOL_RESULT_CHARS) return text;
  return `${text.slice(0, MAX_TOOL_RESULT_CHARS)}\n\n…（内容过长已截断，只显示了前 ${MAX_TOOL_RESULT_CHARS} 个字符）`;
}

/**
 * 执行一次工具调用，返回要喂回给模型的文本。
 *
 * ★ 不管成功失败都返回字符串（而不是抛异常）——
 *   失败信息本身就是给模型看的：它知道「这个路径不存在」之后会自己换个路径试。
 *   抛异常反而会让整个循环断掉，而那些错本来就都是可预期的
 */
export async function runAgentTool(name: string, args: Record<string, unknown>): Promise<string> {
  const asPath = (value: unknown): string => (typeof value === "string" ? value : "");

  try {
    switch (name) {
      case "read_file": {
        const path = asPath(args.path);
        if (!path) return "错误：缺少 path 参数";

        // ★★ 优先读**编辑器里**那一份，而不是磁盘上的。
        //   磁盘上的可能已经过时 —— 用户改了还没保存，
        //   于是模型读到旧内容，一本正经地跟你讨论一段
        //   你屏幕上早就不存在的代码。这种错最难发现：
        //   它说得头头是道，你只会以为自己记错了
        const open = getOpenDocument(path);
        if (open !== null) {
          return open.dirty
            ? `（以下是编辑器里的**最新**内容 —— 用户有未保存的修改，磁盘上还不是这个版本）\n${clamp(open.text)}`
            : clamp(open.text);
        }

        const content = await invoke<string>("read_file", { path });
        return clamp(content);
      }

      case "list_directory": {
        const path = asPath(args.path);
        if (!path) return "错误：缺少 path 参数";
        const tree = await invoke<FileNode[]>("read_dir", { path });
        const lines: string[] = [];
        renderTree(tree, 0, lines);
        const suffix =
          lines.length >= MAX_DIR_ENTRIES ? `\n（只列出了前 ${MAX_DIR_ENTRIES} 条）` : "";
        return lines.join("\n") + suffix;
      }

      case "search_in_folder": {
        const path = asPath(args.path);
        const query = asPath(args.query);
        if (!path || !query) return "错误：缺少 path 或 query 参数";

        const result = await invoke<{
          matches: Array<{ path: string; line: number; text: string }>;
          truncated: boolean;
          filesScanned: number;
        }>("search_in_folder", { path, query, caseSensitive: true });

        if (result.matches.length === 0) {
          return `在 ${result.filesScanned} 个文件里没有任何匹配`;
        }

        const lines = result.matches.map((m) => `${m.path}:${m.line}: ${m.text.trim()}`);
        const suffix = result.truncated ? "\n（结果过多，只显示了前一批）" : "";
        return clamp(lines.join("\n") + suffix);
      }

      case "write_file": {
        const path = asPath(args.path);
        if (!path) return "错误：缺少 path 参数";
        if (typeof args.content !== "string") return "错误：content 必须是字符串";

        // 先拿原文 —— diff 要靠它。
        //
        // ★ 同样优先用编辑器里那份。不然 diff 会把「用户改了还没保存的部分」
        //   显示成「模型准备删掉它」—— 那是个假象，看着吓人而且完全是错的
        //
        // ⚠ 读不出来（文件不存在 = 新建，或者不是文本）就当「从空开始」，
        //   不当成错误：新建文件是完全合法的
        let original = "";
        const open = getOpenDocument(path);
        if (open !== null) {
          original = open.text;
        } else {
          try {
            original = await invoke<string>("read_file", { path });
          } catch {
            original = "";
          }
        }

        if (original === args.content) {
          // 模型反复提交同样的内容时很常见（它不确定改成功没有），
          // 直接告诉它，免得列表里堆一堆空改动
          return "内容没有变化，不需要修改";
        }

        proposeChange(path, original, args.content);
        return `已登记对 ${path} 的改动，等用户看过 diff 确认后才会写入`;
      }

      default:
        // ★ 模型偶尔会「发明」一个不存在的工具。告诉它就行，不用当故障
        return `错误：没有名为 ${name} 的工具`;
    }
  } catch (error) {
    return `调用 ${name} 失败：${String(error)}`;
  }
}
