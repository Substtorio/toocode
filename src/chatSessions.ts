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
 *
 * ★★ 文件夹（工作区）是**一个维度，不是若干个文件**：所有会话仍在同一个
 *   `toocode.chats.json` 里，每条会话自己记着「我属于哪个文件夹」。
 *   为什么不按文件夹分文件、也不存进项目目录：
 *   · 存进项目目录的话，每个仓库里都会多出一个聊天记录文件，还被 git 看见
 *   · 分文件的话「切回上一个项目要找哪个文件」成了新问题，而**归属**本来
 *     就是一个字段能说清的事
 *   ⚠ 所以「隔离」是靠**过滤**实现的，不是靠存放位置 —— 见 `trimSessions`
 *     和 ChatPanel 里的 `visibleSessions`
 */
import { invoke } from "@tauri-apps/api/core";
import { normalizePath } from "./pathUtils";
import type { ChatMessage } from "./agentLoop";

/**
 * **每个文件夹**最多留多少个会话。
 *
 * ★ 语义是「每区」而不是「总共」：全局只留 20 段的话，多开几个项目之后，
 *   旧项目的对话会被**静默挤掉** —— 用户切回去发现记录没了，没有任何提示。
 *   而那种「东西不见了但没报错」的失效方式最难查
 */
export const MAX_SESSIONS = 20;

/**
 * 整个文件最多留多少个（所有文件夹加起来）。
 *
 * ★ 为什么每区封了顶还要这个：20 × N 个项目，N 大了文件会一直长。
 *   总上限按「最近更新」排，超出的必然是**长期没用过**的那些
 */
export const MAX_SESSIONS_TOTAL = 60;

/**
 * 一个文件夹的「身份」。
 *
 * ★ 用**归一化后的路径**而不是原始字符串：写 `D:\foo` 和 `d:/foo/`
 *   是同一个文件夹，两次打开不该被当成两个工作区
 * ★ `null` = 没打开文件夹 —— **单独一档**，和任何真实路径都不相等。
 *   这样「没打开文件夹时建的对话」天生就不会跑到某个项目里去
 * ⚠ `normalizePath` 复用 `pathUtils` 那一份（全项目唯一），
 *   不在这里再写一遍 —— 两份实现迟早会不一致
 */
export function workspaceKeyOf(root: string | null | undefined): string | null {
  if (root === null || root === undefined) return null;
  const trimmed = root.trim();
  if (trimmed === "") return null;
  return normalizePath(trimmed);
}

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
  /**
   * 这段对话属于哪个文件夹（`workspaceKeyOf` 归一化过的绝对路径）。
   *
   * ★ 为什么需要这一维：会话文件是**全局一个**，而「上下文」（当前文件夹 /
   *   打开的文件 / 光标 / 选区）是**每次发消息现取**的（见 ChatPanel 的
   *   `buildSystemPrompt`）。不记归属的话，切到另一个项目后面板里还是上一个
   *   项目的对话 —— 历史在说旧项目、下一条消息却带着新项目的上下文，
   *   模型只会答得莫名其妙，而且两边都不报错
   *
   * `null` = 建这段对话时没打开文件夹。
   * ⚠ 加这个字段**之前**存的会话读回来是 `undefined` ⇒ 归成 `null`（未归属）。
   *   这是故意的：猜「它属于哪个项目」比承认不知道更糟
   */
  workspace: string | null;
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

/** 新建一段。`workspace` 由调用方给 —— 归属是**建的时候**定下来的，之后不改 */
export function newSession(workspace: string | null, now = Date.now()): ChatSession {
  return {
    id: newSessionId(),
    title: "新对话",
    createdAt: now,
    updatedAt: now,
    workspace,
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

  // ★ 缺失 / 不是字符串 / 空串 ⇒ `null`（未归属）。
  //   不要在这里「顺手归给当前文件夹」—— 那等于替用户做了一个他看不见的决定
  const workspace =
    typeof raw.workspace === "string" && raw.workspace.trim() !== ""
      ? raw.workspace
      : null;

  return {
    id: raw.id,
    title: typeof raw.title === "string" && raw.title !== "" ? raw.title : "新对话",
    createdAt,
    updatedAt: typeof raw.updatedAt === "number" ? raw.updatedAt : createdAt,
    workspace,
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
    // ★ 这里**不截断**：截断统一在 saveSessions 里做。
    //   读的时候再截一次，只是把「读进来又丢掉」这件事多写了一遍
    return parsed
      .map(normalizeSession)
      .filter((session): session is ChatSession => session !== null);
  } catch (error) {
    // ★ 解析失败**不要**顺手把文件清掉 —— 万一只是我们这里理解错了，
    //   用户的记录还在原地，下次改对了还能读出来
    console.warn(`[Topilot] 聊天记录解析失败（已忽略）：${String(error)}`);
    return [];
  }
}

/**
 * 按上限修剪：**每个文件夹各自封顶**，再加一个总的硬上限。
 *
 * ★★ 这是**唯一**的截断入口。以前有两处（`newChat` 里一处、存盘时一处），
 *   两处都是全局排前 20 —— 那意味着「界面上还在、文件里已经没了」，
 *   或者反过来。两份逻辑迟早会不一致，所以收成一个函数
 * ★ 为什么必须「按文件夹分别算」：全局只留 20 段的话，多开几个项目之后，
 *   旧项目的对话会被静默挤掉。而「东西不见了但没报错」是最难查的一类
 * ⚠ 返回**新的数组**（而且按更新时间重排过）—— 调用方要接住它，
 *   否则界面上还是旧的（界面用 `sortedSessions` 自己排，所以看不出来；
 *   但 `length` 会不对）
 */
export function trimSessions(sessions: readonly ChatSession[]): ChatSession[] {
  // 按归属分桶。★ 用 `?? ""` 把 `null` 折成一个字符串 key ——
  //   Map 的 key 可以是 null，但字符串化之后读写都更省事
  const byWorkspace = new Map<string, ChatSession[]>();
  for (const session of sessions) {
    const key = session.workspace ?? "";
    const list = byWorkspace.get(key);
    if (list === undefined) byWorkspace.set(key, [session]);
    else list.push(session);
  }

  const kept: ChatSession[] = [];
  for (const list of byWorkspace.values()) {
    list.sort((a, b) => b.updatedAt - a.updatedAt);
    kept.push(...list.slice(0, MAX_SESSIONS));
  }

  kept.sort((a, b) => b.updatedAt - a.updatedAt);
  return kept.slice(0, MAX_SESSIONS_TOTAL);
}

/**
 * 写回去。★ 落盘前先 `trimSessions`。
 *
 * ⚠ 刻意**不抛异常**：调用它的地方都是「顺手存一下」（发完一轮、切换会话），
 *   存盘失败不该把对话本身打断 —— 只在控制台说一声（会进「输出」面板）
 */
export async function saveSessions(sessions: readonly ChatSession[]): Promise<void> {
  const newest = trimSessions(sessions).map(toStored);

  try {
    await invoke("save_chats", { data: JSON.stringify(newest) });
  } catch (error) {
    console.warn(`[Topilot] 保存聊天记录失败：${String(error)}`);
  }
}
