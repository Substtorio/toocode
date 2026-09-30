// 编辑器状态桥 —— 让 Agent 能「看见」用户此刻在编辑什么
//
// ★★ 为什么要有这么一层，而不是让 agentTools.ts 直接去拿 App.vue 里的状态：
//
//   ① **方向问题**。数据是从 UI 流向 Agent（App.vue 推、Agent 拉），
//      这和 agentChanges.ts 正好相反 —— 那边是 Agent 写、UI 读。
//      方向不同，就不能照抄它那种「模块级 ref」的写法，
//      否则你得倒过来想一遍「谁是真相来源」，很容易绕晕。
//
//   ② **边界问题**。App.vue 里的 models 是 `Map<string, ITextModel>`。
//      要是把 Map 直接暴露出去，agentTools.ts 就得开始懂 Monaco 的数据结构。
//      这里只开**几个函数**当窗口：两边都不知道对方内部长什么样。
//      想单独测 agentTools.ts 的时候，塞一个假的实现进去就行
//      （这也正是它能被单独测的前提）。
//
//   ③ **可选性**。桥在「浏览器里跑」或者「还没挂上编辑器」时是空的 ——
//      所以每个取数函数都可能返回 null。调用方必须处理好这种情况，
//      不能把它变成「一定存在」的依赖

/**
 * 归一化路径，专门用于「在一堆已打开的文档里找匹配」。
 *
 * ★ 为什么要转小写：Windows 上路径**不区分大小写**，
 *   而模型给 read_file / write_file 的路径是它**自己拼**出来的，
 *   大小写和真实文件名对不上是家常便饭。
 *   不归一化的话就会出现「文件明明开着，模型却读到了磁盘上那份旧内容」——
 *   而且完全没有任何报错，你只会觉得「它怎么在说一些对不上的话」。
 *
 * ⚠ 只在「查找已打开的文档」这种宽松场景用它，不要拿它当通用的路径比较
 *（Linux 上路径是区分大小写的）
 */
export function normalizePath(path: string): string {
  return path.replace(/\\/g, "/").replace(/\/+$/, "").toLowerCase();
}

/**
 * 选中内容最多带多少字符进 prompt。
 * ★ 和工具结果的截断是同一个道理：用户选中一整个文件时，
 *   不截断就会把上下文整个撑爆
 */
export const MAX_SELECTION_CHARS = 4000;

/** 用户此刻的编辑状态 */
export interface EditorContext {
  /** 当前激活文件的路径。没打开任何文件时是 null */
  path: string | null;
  /** 光标行号（1 开始） */
  line: number;
  /** 光标列号（1 开始） */
  column: number;
  /** 选中的文本。没选中时是空串 */
  selection: string;
  /** 选区的起始行，没选中时是 0 */
  selectionStartLine: number;
  /** 选区的结束行，没选中时是 0 */
  selectionEndLine: number;
  /** 选中内容因为过长被截断过 */
  truncated: boolean;
  /** 当前文件有未保存的改动 */
  dirty: boolean;
  /** 当前打开的所有标签路径 */
  openPaths: string[];
}

/** 编辑器里某个文档的样子 */
export interface OpenDocument {
  /** 编辑器里的**当前**内容（可能含未保存的改动） */
  text: string;
  /** 有未保存的改动 —— 也就是说它和磁盘上的版本**不一样** */
  dirty: boolean;
}

interface BridgeTarget {
  context(): EditorContext;
  /** 参数是**已经归一化过**的路径 */
  document(normalizedPath: string): OpenDocument | null;
}

let target: BridgeTarget | null = null;

/**
 * 由 App.vue 在挂载时注册。传 null 表示撤销注册（组件卸载时用）。
 *
 * ★ 是「推」而不是「拉」：App.vue 把取数函数送进来，
 *   而不是桥自己去 import App.vue 找。
 *   后者会形成循环依赖（App → agentTools → App），
 *   而且 agentTools.ts 就再也没法脱离 UI 单独跑了
 */
export function provideEditorBridge(next: BridgeTarget | null): void {
  target = next;
}

/** 用户此刻的编辑状态。桥没接上时返回 null */
export function getEditorContext(): EditorContext | null {
  if (!target) return null;
  try {
    return target.context();
  } catch {
    // 桥这一层必须「坏得安全」：拿不到状态只是让 Agent 少一点上下文，
    // 不该让整轮对话直接断掉
    return null;
  }
}

/**
 * 编辑器里那份文档。
 *
 * ★★ 这是本轮最值钱的一个改动：`read_file` 会**优先**走这里。
 *   因为磁盘上的文件可能已经被用户改了但还没保存 ——
 *   直接读磁盘的话，模型看到的是旧内容，于是它跟你讨论的是
 *   「几个小时前的那份代码」，而屏幕上显示的根本不是那些。
 *   这种错最难查：模型言之凿凿，你还以为是自己记错了
 *
 * 桥没接上、或者这个文件没被打开过，返回 null（调用方再回落到读磁盘）
 */
export function getOpenDocument(path: string): OpenDocument | null {
  if (!target) return null;
  try {
    return target.document(normalizePath(path));
  } catch {
    return null;
  }
}
