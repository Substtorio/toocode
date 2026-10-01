/**
 * Topilot 的「会话」—— 一段可以独立存、独立切回来的对话。
 *
 * ★ 为什么单独一个文件：这一层管的是**存哪儿、怎么存、存什么形状**，
 *   和界面一点关系都没有。拆出来之后
 *   · `ChatPanel.vue` 只管渲染和切换
 *   · 「序列化 / 反序列化 / 上限 / 坏数据」这些能单独验 ——
 *     而这恰好是「错了也看不出来」的那一类（存坏了要等下次启动才知道）
 *
 * ★★ 存哪儿：`appDataDir/toocode.chats.json`，**不是 localStorage**。
 *   localStorage 有容量上限，而且和 hot exit **共用**同一个配额 ——
 *   聊天记录一多，未保存内容的备份就会**静默**写失败。
 *   丢聊天记录只是烦，丢未保存的代码是真事故
 *
 * ★ 传的是**原始 JSON 文本**，Rust 那边不解析（和 `chat_stream` 转 SSE 同一套分工）：
 *   会话的形状是前端的事，加个字段不用重编 Rust
 */
import { invoke } from "@tauri-apps/api/core";
import type { ChatMessage } from "./agentLoop";

/** 最多留多少个会话。★ 文件是**每次对话都整份重写**的，不设上限会一直长 */
export const MAX_SESSIONS = 20;

/** 一次工具调用。和 `agentTools` 的执行结果一一对应 */
export interface ChatToolRun {
  name: string;
  args: string;
  result: string;
  done: boolean;
}

/**
 * 界面上的一条消息。
 *
 * ⚠ `html` 是**派生数据**（从 `text` 渲染出来的），所以**不存盘** ——
 *   存下来只会让文件又大又容易过期（换了 Markdown 渲染器，旧 HTML 就对不上了）。
 *   加载回来时它是 `undefined`，由界面在切过去的时候补渲染（见 ChatPanel 的 renderSession）
 */
export interface ChatSessionMessage {
  id: number;
  role: "user" | "assistant" | "tool";
  text: string;
  reasoning: string;
  tools: ChatToolRun[];
  html?: string;
}

export interface ChatSession {
  id: string;
  /** 列表上显示的标题，取自第一条用户消息 */
  title: string;
  createdAt: number;
  updatedAt: number;
  /** 界面用的消息 */
  messages: ChatSessionMessage[];
  /** 发给模型的历史（含 tool_calls / 工具结果），和上面那套是两回事 */
  history: ChatMessage[];
}

export function newSessionId(): string {
  // ★ 掺时间戳（36 进制）：光用自增序号的话，重新加载后会从 1 重新数，
  //   和文件里已有的撞名（语言服务器那边踩过这个坑）
  return `chat-${Date.now().toString(36)}`;
}

/** 从第一条用户消息里取个标题 */
export function titleFrom(text: string): string {
  const firstLine = text.trim().split("\n")[0].trim();
  if (firstLine === "") return "新对话";
  return firstLine.length > 24 ? `${firstLine.slice(0, 24)}…` : firstLine;
}

export function newSession(now = Date.now()): ChatSession {
  return {
    id: newSessionId(),
    title: "新对话",
    createdAt: now,
    updatedAt: now,
    messages: [],
    history: [],
  };
}

/** 存盘前**摘掉 `html`**（它是派生数据，见 ChatSessionMessage 的说明） */
function toStored(session: ChatSession): ChatSession {
  return {
    ...session,
    messages: session.messages.map((message) => ({
      id: message.id,
      role: message.role,
      text: message.text,
      reasoning: message.reasoning,
      tools: message.tools,
      // html 故意不写
    })),
  };
}

/**
 * 把磁盘上读到的东西「修」成能用的会话；实在不成形状就返回 null。
 *
 * ★ 为什么要逐项校验而不是直接 `as ChatSession[]`：
 *   文件是可以被手改、也可能来自旧版本。**一个字段不对就整个面板炸掉**，
 *   而那种崩法只在「用户已经用了很久」之后才出现 —— 最难查的一类。
 *   这里的原则是「读不出来的当它不存在」，而不是「相信它一定对」
 */
function normalizeSession(value: unknown): ChatSession | null {
  if (typeof value !== "object" || value === null) return null;
  const raw = value as Partial<ChatSession>;
  if (typeof raw.id !== "string" || raw.id === "") return null;

  const messages: ChatSessionMessage[] = Array.isArray(raw.messages)
    ? raw.messages
        .filter((item): item is ChatSessionMessage => typeof item === "object" && item !== null)
        .map((item, index) => ({
          id: typeof item.id === "number" ? item.id : index + 1,
          role: item.role === "user" || item.role === "assistant" ? item.role : "assistant",
          text: typeof item.text === "string" ? item.text : "",
          reasoning: typeof item.reasoning === "string" ? item.reasoning : "",
          tools: Array.isArray(item.tools) ? item.tools : [],
        }))
    : [];

  // 历史只做最轻的检查：它的形状由 agentLoop 决定，
  // 这里把关太严反而会在以后加字段时把老记录全判死
  const history: ChatMessage[] = Array.isArray(raw.history)
    ? raw.history.filter(
        (item): item is ChatMessage =>
          typeof item === "object" && item !== null && typeof (item as ChatMessage).role === "string",
      )
    : [];

  const createdAt = typeof raw.createdAt === "number" ? raw.createdAt : Date.now();

  return {
    id: raw.id,
    title: typeof raw.title === "string" && raw.title !== "" ? raw.title : "新对话",
    createdAt,
    updatedAt: typeof raw.updatedAt === "number" ? raw.updatedAt : createdAt,
    messages,
    history,
  };
}

/** 读回来。**任何读不出来的情况都只当成「没有记录」**，不抛 */
export async function loadSessions(): Promise<ChatSession[]> {
  let raw: string;
  try {
    raw = await invoke<string>("load_chats");
  } catch (error) {
    console.warn(`[Topilot] 读聊天记录失败（不影响新对话）：${String(error)}`);
    return [];
  }

  try {
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed
      .map(normalizeSession)
      .filter((session): session is ChatSession => session !== null)
      .slice(0, MAX_SESSIONS);
  } catch (error) {
    // ★ 解析失败**不要**顺手把文件清掉 —— 万一只是我们这里理解错了，
    //   用户的记录还在原地，下次改对了还能读出来
    console.warn(`[Topilot] 聊天记录解析失败（已忽略）：${String(error)}`);
    return [];
  }
}

/**
 * 写回去。★ 只留最新的 `MAX_SESSIONS` 个。
 *
 * ⚠ 刻意**不抛异常**：调用它的地方都是「顺手存一下」（发完一轮、切换会话），
 *   存盘失败不该把对话本身打断 —— 只在控制台说一声（会进「输出」面板）
 */
export async function saveSessions(sessions: readonly ChatSession[]): Promise<void> {
  const newest = [...sessions]
    .sort((a, b) => b.updatedAt - a.updatedAt)
    .slice(0, MAX_SESSIONS)
    .map(toStored);

  try {
    await invoke("save_chats", { data: JSON.stringify(newest) });
  } catch (error) {
    console.warn(`[Topilot] 保存聊天记录失败：${String(error)}`);
  }
}
