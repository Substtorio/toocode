import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import * as monaco from "monaco-editor";

/**
 * 一个最小可用的 LSP 客户端。
 *
 * 分工：`src-tauri/src/lib.rs` 那节只负责**帧**（`Content-Length: N\r\n\r\n<body>`）
 * 和进程的收发；这个文件负责**语义** —— JSON-RPC 的请求/通知、LSP 的方法名、
 * 以及把结果翻译成 Monaco 能懂的东西。
 *
 * ★ 为什么这么切：解析放前端，改 LSP 版本 / 加个方法都不用重编 Rust。
 *   而且**整套东西能做成纯函数单独测**（`route` 那个函数就是干这个的）。
 *
 * ★★ 为什么 DAP 也能沾光：DAP 用的是**同一套分帧**，只是消息类型不同。
 *   Rust 那节换个 emit 事件名就能复用；这个文件里的 JSON-RPC 骨架也一样，
 *   只有「方法名 → 动作」那张表要换。
 */

// ============================ 类型 ============================

export interface LspServerInfo {
  id: string;
  label: string;
  /** 这个服务器管哪些语言 id（和语法扫描给出的语言 id 对齐） */
  languages: string[];
  program: string;
  args: string[];
}

/** Rust 侧发过来的每一条消息 */
interface LspMessage {
  session: string;
  /** 原始 JSON 文本 */
  body: string;
}

/** JSON-RPC 2.0 的一帧 */
interface JsonRpc {
  jsonrpc: "2.0";
  id?: number | string;
  method?: string;
  params?: unknown;
  result?: unknown;
  error?: unknown;
}

/** LSP 的 diagnostic（只列我们真正会读的字段） */
interface LspDiagnostic {
  range: {
    start: { line: number; character: number };
    end: { line: number; character: number };
  };
  /** 1=Error 2=Warning 3=Information 4=Hint */
  severity?: number;
  code?: string | number;
  source?: string;
  message: string;
}

// ======================= 语言特性的数据形状 =======================
//
// ⚠ 下面这些行列**都从 0 开始**（和 Monaco 相反），转换在 lspFeatures.ts 里做。
//   只声明我们真正会读的字段 —— LSP 的字段很多，全抄一遍只会掩盖「我们到底用了什么」

export interface LspPosition {
  line: number;
  character: number;
}

export interface LspRange {
  start: LspPosition;
  end: LspPosition;
}

/** `textDocument/completion` 的一项 */
export interface LspCompletionItem {
  label: string;
  /** 1=Text 2=Method 3=Function …（★ 和 Monaco 的编号**不一样**，见 lspFeatures.ts） */
  kind?: number;
  detail?: string;
  documentation?: string | { kind: string; value: string };
  sortText?: string;
  filterText?: string;
  insertText?: string;
  /** 1 = 纯文本，2 = 片段（里面有 $1 / ${2:name} 要展开） */
  insertTextFormat?: number;
  textEdit?: {
    range?: LspRange;
    newText: string;
    /** `InsertReplaceEdit` 的两种范围，比 range 更准 */
    insert?: LspRange;
    replace?: LspRange;
  };
  /**
   * 选中这一条时要**顺便**做的编辑 —— 自动导入（auto import）就靠它。
   *
   * ★ 按 LSP 规定，这些编辑**和主编辑在同一个文档里**（不带 uri）。
   *   所以能直接映射成 Monaco 那个同文档的 `ISingleEditOperation[]`。
   *   （LSP 3.17 想改这个设计，于是有了跨文档的 `textEditText` / `itemDefaults`，
   *    但真服务器基本还在用这一版）
   *
   * ⚠ 规定还要求它们**不和主编辑重叠** —— Monaco 也这么要求。
   *   TS 那种「在文件顶部插一行 import」天然不重叠，所以照搬即可
   */
  additionalTextEdits?: Array<{ range: LspRange; newText: string }>;
}

/** `textDocument/hover` 的内容有四种写法，都得摊平 */
export type LspMarkedString =
  | string
  | { language: string; value: string }
  | { kind: string; value: string };

export interface LspHover {
  contents: LspMarkedString | LspMarkedString[];
  range?: LspRange;
}

export interface LspLocation {
  uri: string;
  range: LspRange;
}

/** 服务器也可以回 `LocationLink`（三个范围，更精确） */
export interface LspLocationLink {
  targetUri: string;
  targetRange: LspRange;
  targetSelectionRange: LspRange;
}

/** 握手时从服务器那边学到、后续要用到的能力 */
export interface LspServerCapabilities {
  /** 哪些字符一敲就该弹补全（CSS 是 `.` `:`、JSON 是 `"` …）。**只有服务器自己知道** */
  triggerCharacters: string[];
  /**
   * 服务器声明支持 `textDocument/rename` 吗。
   *
   * ⚠ ★ 这个必须**问服务器**，不能一律注册：实测这台机器上
   *   html / css 回 `renameProvider: true`，而 **json 压根没这一项** ——
   *   给 json 也注册 rename 的话，按下 F2 会弹出一个框，输完名字却什么也没发生
   */
  supportsRename: boolean;
}

/** `textDocument/rename` 的结果 */
export interface LspWorkspaceEdit {
  /** 按 uri 分组的文本编辑 */
  changes?: Record<string, Array<{ range: LspRange; newText: string }>>;
  /** 另一种写法：一个数组，能按顺序带「新建 / 重命名 / 删除文件」的操作 */
  documentChanges?: Array<
    | { textDocument: { uri: string; version?: number | null }; edits: Array<{ range: LspRange; newText: string }> }
    | { kind: string; uri?: string }
  >;
}

// ============================ 路径 ↔ URI ============================

/**
 * 磁盘路径 → `file://` URI。
 *
 * ★★ 两个必须做的转换，少一个服务器就认不出这个文件：
 *   ① 盘符里的 `:` 要转义成 `%3A`，盘符还习惯性小写
 *      （`D:/a/b.ts` → `file:///d%3A/a/b.ts`）—— 这正是 VS Code 自己的写法
 *   ② 其余每一段都要 `encodeURIComponent`，否则中文 / 空格路径会拼出非法 URI
 */
export function pathToUri(path: string): string {
  let normalized = path.replace(/\\/g, "/");
  if (/^[a-zA-Z]:/.test(normalized)) {
    normalized = "/" + normalized[0].toLowerCase() + normalized.slice(1);
  }
  return "file://" + normalized.split("/").map(encodeURIComponent).join("/");
}

/** `file://` URI → 磁盘路径（上面那个的逆运算） */
export function uriToPath(uri: string): string {
  let path = decodeURIComponent(uri.replace(/^file:\/\//, ""));
  if (/^\/[a-zA-Z]:/.test(path)) {
    path = path[1].toUpperCase() + path.slice(2);
  }
  return path;
}

/**
 * 路径的最后一段，只为日志好读。
 *
 * ★ 日志里写全路径没意义 —— 屏幕上开着的基本都在同一个项目里，
 *   而全路径一长，真正想看的那几个字反而被挤没了
 */
function baseName(path: string): string {
  return path.split(/[\\/]/).pop() ?? path;
}

/**
 * 每个文档上一次报出来的诊断条数。
 *
 * ★ 只为了「条数没变就不记日志」—— 每敲一个字服务器都会推一次诊断，
 *   不挡的话「输出」面板会被刷屏，而刷屏的日志等于没有日志
 */
const lastDiagnosticCount = new Map<string, number>();

// ============================ 翻译 ============================

/**
 * LSP 的 diagnostic → Monaco 的 marker。
 *
 * ⚠ ★★ **两边是两套下标**：LSP 的行列都从 **0** 开始，Monaco 从 **1** 开始。
 *   不 +1 的话报错会整体往上偏一行 —— 看起来像「报到了上一行」，
 *   很容易被当成「服务器的 range 算错了」
 *
 * ⚠ ★★ 还要**夹一下列号**：LSP 允许 `end.character` 超出这一行的长度
 *   （比如「直到行尾」就用一个很大的值表示），而 Monaco 的 marker
 *   列号越界会**抛异常**。用 model 的真实行宽夹一次最稳
 *
 * ⚠ ★★ **字段名必须是 `startLineNumber` / `startColumn`**，不是 `lineNumber` /
 *   `column`。写成后者 Monaco 不会报错 —— 它只是读不到起点，
 *   然后把这个 marker 当成「零长度、在 1:1」处理。
 *   症状极难认：诊断**确实到了**（日志里行号也对），但泡泡线画在第 1 行第 1 列。
 *   ⇒ 这类错类型检查能抳（TS2739），所以改完一定得真跑一次 vue-tsc
 */
export function toMarker(
  diagnostic: LspDiagnostic,
  maxColumnFor: (lineNumber: number) => number,
): monaco.editor.IMarkerData {
  const clamp = (line: number, character: number) => {
    const lineNumber = line + 1;
    const max = maxColumnFor(lineNumber);
    return { lineNumber, column: Math.max(1, Math.min(max, character + 1)) };
  };

  const start = clamp(diagnostic.range.start.line, diagnostic.range.start.character);
  const end = clamp(diagnostic.range.end.line, diagnostic.range.end.character);

  return {
    startLineNumber: start.lineNumber,
    startColumn: start.column,
    endLineNumber: end.lineNumber,
    endColumn: end.column,
    severity: SEVERITY[diagnostic.severity ?? 1] ?? monaco.MarkerSeverity.Error,
    message: diagnostic.message,
    source: diagnostic.source,
    code: diagnostic.code === undefined ? undefined : String(diagnostic.code),
  };
}

/** LSP 的 severity 数字 → Monaco 的枚举。默认（没给）当错误 */
const SEVERITY: Record<number, monaco.MarkerSeverity> = {
  1: monaco.MarkerSeverity.Error,
  2: monaco.MarkerSeverity.Warning,
  3: monaco.MarkerSeverity.Info,
  4: monaco.MarkerSeverity.Hint,
};

// ============================ 客户端本体 ============================

/** 由 App 提供的两个「窗口」—— 和 editorBridge 一个套路，两边都不知道对方内部长什么样 */
export interface LspHost {
  /** 按路径找已经打开的 model。找不到就返回 null（比如文件关了） */
  findModel: (path: string) => monaco.editor.ITextModel | null;
  /** 工作区根。既当 `initialize` 的 rootUri，也当服务器的 cwd */
  workspaceRoot: () => string | null;
  /**
   * 某个语言的服务器握完手了。
   *
   * ★ 为什么要这个回调：**`triggerCharacters` 只有服务器自己知道**
   *   （CSS 是 `.` `:`、JSON 是 `"` `:`……），而 Monaco 的
   *   `registerCompletionItemProvider` 要求在**注册时**就写死它们。
   *   所以做不到「先把 provider 注册好、以后再补触发字符」——
   *   只能等到服务器真的起来了，再按它的声明注册一遍
   */
  onServerReady?: (languageIds: string[], capabilities: LspServerCapabilities) => void;

  /**
   * 某个语言的服务器**起不来了**（进程拉不起来 / 握手失败）。
   *
   * ★★ 为什么需要它：App 那边会在「还没有任何文件」的时候就把这些语言的
   *   **内置语言服务**关掉（因为那个开关只能在第一个文件之前生效 ——
   *   见 App 里 handLanguageToServer 的说明）。万一服务器真起不来，
   *   不把这个消息告诉 App，那个语言就**什么都没有**了：既没有服务器，
   *   内置的又被提前关了。
   * ⇒ App 收到之后会把内置那套装回去
   */
  onServerUnavailable?: (languageIds: string[]) => void;
}

let host: LspHost | null = null;

/** 服务器清单只问一次 —— 要遍历 VS Code 安装目录，不便宜 */
let serverCache: LspServerInfo[] | null = null;

/** 语言 id → 会话。★ 按**语言**缓存而不是按文件：一个服务器管一堆文件 */
const sessions = new Map<string, LspSession>();

/** 日志出口。由 App 接上「输出」面板；没接就当没发生 */
let log: (line: string) => void = () => {};

export function configureLsp(options: LspHost, logger?: (line: string) => void) {
  host = options;
  if (logger) log = logger;

  // ★★ 先清掉**上一次页面**留下的会话进程。
  //   刷新页面（HMR / F5）时旧页面的 disposeLsp() 不会执行 ——
  //   页面是被拆掉的，不是「卸载」，onUnmounted 根本不跑。
  //   不清的话每刷一次就多留一个语言服务器进程，谁也没法再用它
  void invoke("lsp_stop_all").catch(() => {
    // 还没起来 / 没有残留都无所谓，不值得报错
  });
}

/** 拆掉所有会话（退出 / 换工作区时用） */
export async function disposeLsp(): Promise<void> {
  const all = [...sessions.values()];
  sessions.clear();
  await Promise.all(all.map((session) => session.stop()));
}

/**
 * 这台机器上**有服务器可用**的语言 id。
 *
 * ★★ 为什么要在打开任何文件**之前**就问：
 *   Monaco 内置的 json / css / html 服务是在 `onLanguage` 那一次调用里
 *   **一次性注册好所有 provider** 的，而且之后再改 `modeConfiguration`
 *   **没有任何效果**（实测：那个开关在 setupMode 跑完之后就是空操作）。
 *   ⇒ 想避免「内置和真服务器同时应答」，只能在**第一个该语言的文件
 *     被创建之前**就把内置服务关掉。所以 App 要提前知道该关哪几个
 *
 * ★ 用「扫出来的服务器」而不是自己写一张表：机器上有没有那些服务器
 *   （VS Code 自带的 json / css / html，PATH 里的 rust-analyzer ……）
 *   是**环境事实**，只有扫描知道
 */
export async function availableServerLanguages(): Promise<string[]> {
  serverCache ??= await invoke<LspServerInfo[]>("lsp_servers");
  const all = new Set<string>();
  for (const server of serverCache) {
    for (const language of server.languages) all.add(language);
  }
  return [...all];
}

/**
 * 文件打开 / 切换语言时调它。
 *
 * ★ 按需起服务器：只有真的有文件用这个语言、且机器上确实有对应服务器时才起。
 *   启动一个语言服务器不便宜（rust-analyzer 要几秒），不能开工作区就全起一遍
 */
export async function openDocument(path: string, languageId: string): Promise<void> {
  if (host === null) return;

  serverCache ??= await invoke<LspServerInfo[]>("lsp_servers");
  const info = serverCache.find((server) => server.languages.includes(languageId));
  if (info === undefined) return; // 这个语言没有服务器，正常

  let session = sessions.get(info.id);
  if (session === undefined) {
    session = new LspSession(info, host.workspaceRoot());
    sessions.set(info.id, session);
    try {
      await session.start();
      log(`[LSP] 已启动 ${info.label} 语言服务器（${info.program}）`);
    } catch (error) {
      sessions.delete(info.id);
      log(`[LSP] ${info.label} 启动失败：${String(error)}`);
      // ★ 告诉 App：这个语言的服务器没了。App 那边会把内置语言服务装回去，
      //   不然这个语言就彻底没有语言特性了（见 LspHost.onServerUnavailable）
      host.onServerUnavailable?.(info.languages);
      return;
    }
  }

  session.openDocument(path, languageId);
}

/** 内容变了。★ 全量同步（`textDocumentSync` 用 Full），实现简单且不会错位 */
export function changeDocument(path: string): void {
  for (const session of sessions.values()) session.changeDocument(path);
}

/** 文件关了 */
export function closeDocument(path: string): void {
  for (const session of sessions.values()) session.closeDocument(path);
}

// ======================= 语言特性请求 =======================
//
// ★ 这三个都是「有人问才去问服务器」，所以每个都要先回答一个问题：
//   **这个文档归哪个会话管？** 答案是「didOpen 过它的那个」（见 sessionFor）——
//   而不是「路径扩展名对得上的那个」。后者会猜错（同一个 `.json` 可能归
//   别的服务器管），而且猜错的表现是“服务器不认识这个文档”⇒ 什么都不回，不报错

/** 找到正在管这个文档的会话 */
function sessionFor(path: string): LspSession | null {
  for (const session of sessions.values()) {
    if (session.handles(path)) return session;
  }
  return null;
}

/** 发一个请求给「管这个文档」的会话。没人管 / 请求出错都返回 null */
async function request(path: string, method: string, params: unknown): Promise<unknown> {
  const session = sessionFor(path);
  if (session === null) return null;
  return session.requestFeature(method, params);
}

/** 补全。返回 null 表示「这个文档没人管」或「服务器出错」 */
export async function requestCompletion(
  path: string,
  position: LspPosition,
): Promise<LspCompletionItem[] | null> {
  const result = await request(path, "textDocument/completion", {
    textDocument: { uri: pathToUri(path) },
    position,
  });
  if (result === null) return null;
  // 服务器可以只回一个数组，也可以回 `{ isIncomplete, items }`
  const items = Array.isArray(result) ? result : (result as { items?: unknown }).items;
  return Array.isArray(items) ? (items as LspCompletionItem[]) : null;
}

/** 悬停提示 */
export async function requestHover(path: string, position: LspPosition): Promise<LspHover | null> {
  const result = await request(path, "textDocument/hover", {
    textDocument: { uri: pathToUri(path) },
    position,
  });
  return result === null ? null : (result as LspHover);
}

/** 跳到定义。服务器可以回单个、一串、或者一串 LocationLink */
export async function requestDefinition(
  path: string,
  position: LspPosition,
): Promise<Array<LspLocation | LspLocationLink> | null> {
  const result = await request(path, "textDocument/definition", {
    textDocument: { uri: pathToUri(path) },
    position,
  });
  if (result === null) return null;
  return Array.isArray(result)
    ? (result as Array<LspLocation | LspLocationLink>)
    : [result as LspLocation | LspLocationLink];
}

/**
 * 重命名：告诉服务器「这里叫 xxx，改成 yyyy」，让它给出要改的每一处。
 *
 * ★ 服务器回的是 `WorkspaceEdit` —— **可能涉及多个文件**，
 *   这也正是它不能靠「自己改 model」实现的原因（要成套地改、还要能撤销）
 */
export async function requestRename(
  path: string,
  position: LspPosition,
  newName: string,
): Promise<LspWorkspaceEdit | null> {
  const result = await request(path, "textDocument/rename", {
    textDocument: { uri: pathToUri(path) },
    position,
    newName,
  });
  return result === null ? null : (result as LspWorkspaceEdit);
}

/**
 * 一个服务器会话。
 *
 * ★★ 一个会话 = 一个服务器进程 + 一堆文档。**不能一个文件起一个进程** ——
 *   rust-analyzer 那种要建全项目索引，起十次就是灾难
 */
class LspSession {
  private static sequence = 0;

  readonly id: string;
  private nextRequestId = 1;
  /** 发出去、等回音的请求 */
  private waiting = new Map<number, (message: JsonRpc) => void>();
  /** 已经 didOpen 过的文档 → 版本号。didChange 要带上递增的版本 */
  private versions = new Map<string, number>();
  private disposers: Array<() => void> = [];
  private stopped = false;
  /** 服务器声明的补全触发字符。握手之后才有值 */
  private triggers: string[] = [];

  constructor(
    private readonly server: LspServerInfo,
    private readonly root: string | null,
  ) {
    // ★★ id 必须**跨页面刷新**也不重复。
    //   以前是单纯的 `lsp-${序号}` —— 而序号是页面里的变量，刷新后从 1 重新数，
    //   于是新页面的第一个会话拿到的还是 `lsp-1`，和上一页留在 Rust 里的
    //   那个重名。Rust 侧 `lsp_start` 是 insert 覆盖：旧的被丢掉 →
    //   它的 stdin 跟着关掉 → 服务器读不到输入就退出 →
    //   而**同一个 id 的退出事件会被新页面收下**，把新会话直接标成 stopped，
    //   于是后面的 `initialized` / `didOpen` 全被静默丢掉 ——
    //   症状是「服务器启动了，但一条诊断都没有」，而且两边都不报错
    this.id = `lsp-${++LspSession.sequence}-${Date.now().toString(36)}`;
  }

  async start(): Promise<void> {
    // ★★ **必须先 listen 再 start** —— 顺序反了，服务器启动时吐的那几帧
    //   （有些服务器一上来就发日志 / 进度）就丢了。终端那块踩过同一个坑
    this.disposers.push(
      await listen<LspMessage>("lsp-message", (event) => {
        if (event.payload.session !== this.id) return;
        try {
          this.handle(JSON.parse(event.payload.body) as JsonRpc);
        } catch (error) {
          log(`[LSP] 收到解不动的消息：${String(error)}`);
        }
      }),
    );
    this.disposers.push(
      await listen<string>("lsp-stderr", (event) => {
        if (!event.payload.startsWith(`[${this.id}]`)) return;
        log(`[LSP] ${event.payload}`);
      }),
    );
    this.disposers.push(
      await listen<string>("lsp-error", (event) => log(`[LSP] ${event.payload}`)),
    );
    this.disposers.push(
      await listen<string>("lsp-exit", (event) => {
        if (event.payload !== this.id) return;
        log(`[LSP] ${this.server.label} 服务器退出了`);
        this.stopped = true;
      }),
    );

    await invoke("lsp_start", {
      session: this.id,
      serverId: this.server.id,
      cwd: this.root,
    });

    // 握手。rootUri 给服务器用来找项目配置（tsconfig / Cargo.toml 之类）
    const handshake = await this.request("initialize", {
      processId: null,
      rootUri: this.root === null ? null : pathToUri(this.root),
      capabilities: {
        // ★ 只声明我们**真的会处理**的能力。
        //   声明了却不回应，服务器会一直等
        textDocument: {
          synchronization: { dynamicRegistration: false },
          publishDiagnostics: { relatedInformation: false },
        },
      },
      workspaceFolders:
        this.root === null
          ? null
          : [{ uri: pathToUri(this.root), name: this.root.split(/[\\/]/).pop() ?? "" }],
    });

    // 通知（没有 id，不等回音）—— 告诉服务器「你的 initialize 回应我收到了，
    // 可以开始推诊断了」
    this.notify("initialized", {});

    // ★ 从握手回应里把「补全触发字符」掏出来。
    //   这份清单是服务器的私有知识（CSS 的 `.`、JSON 的 `"`），我们不可能内置一份
    const capabilities = (
      handshake.result as {
        capabilities?: {
          completionProvider?: { triggerCharacters?: string[] };
          renameProvider?: unknown;
        };
      } | undefined
    )?.capabilities;
    this.triggers = capabilities?.completionProvider?.triggerCharacters ?? [];
    // 服务器可以回 `true`，也可以回 `{ prepareProvider: … }` —— 两种都算支持；
    // 只有「没有这一项」或显式 false 才算不支持
    const supportsRename = capabilities?.renameProvider !== undefined && capabilities.renameProvider !== false;
    log(
      `[LSP] ${this.server.label} 就绪（补全触发字符：${this.triggers.join(" ") || "无"}` +
        `，重命名：${supportsRename ? "支持" : "不支持"}）`,
    );
    host?.onServerReady?.(this.server.languages, { triggerCharacters: this.triggers, supportsRename });
  }

  /** 这个会话管着这个文档吗（也就是它 didOpen 过） */
  handles(path: string): boolean {
    return this.versions.has(path);
  }

  /**
   * 发一个语言特性请求（补全 / 悬停 / 定义）。
   *
   * ★ 出错、服务器不回、会话已经停了 —— 一律返回 null。
   *   「问不出来」不是错误，是个常态：光标停在一个没有补全的地方而已
   */
  async requestFeature(method: string, params: unknown): Promise<unknown> {
    if (this.stopped) return null;
    const reply = await this.request(method, params);
    if (reply.error !== undefined) {
      log(`[LSP] ${method} 出错：${String(reply.error)}`);
      return null;
    }
    return reply.result ?? null;
  }

  openDocument(path: string, languageId: string): void {
    // ★ 已经开过的不要重复发。调用方那边（App 里 modelForPath）为了覆盖
    //   「hot exit 恢复出来的文件从来没 didOpen 过」这种情况，会重复调进来；
    //   而同一个 URI 发两次 didOpen 在协议上是错的
    if (this.versions.has(path)) return;

    const model = host?.findModel(path);
    if (model === undefined || model === null) return;
    this.versions.set(path, 1);
    log(`[LSP] 打开 ${baseName(path)}（${languageId}，${model.getValue().length} 字符）`);
    this.notify("textDocument/didOpen", {
      textDocument: {
        uri: pathToUri(path),
        languageId,
        version: 1,
        text: model.getValue(),
      },
    });
  }

  changeDocument(path: string): void {
    const version = this.versions.get(path);
    if (version === undefined) return; // 这个文件没在这个会话里开过
    const model = host?.findModel(path);
    if (model === undefined || model === null) return;

    const next = version + 1;
    this.versions.set(path, next);
    this.notify("textDocument/didChange", {
      textDocument: { uri: pathToUri(path), version: next },
      // 全量同步：直接给整份新文本（见上面的说明）
      contentChanges: [{ text: model.getValue() }],
    });
  }

  closeDocument(path: string): void {
    if (!this.versions.delete(path)) return;
    // 顺手把「上次几条诊断」的记忆也清掉 ——
    // 不然关掉再打开、而且条数恰好一样时，那一行日志就不会记了
    lastDiagnosticCount.delete(path);
    this.notify("textDocument/didClose", { textDocument: { uri: pathToUri(path) } });
  }

  async stop(): Promise<void> {
    if (this.stopped) return;
    this.stopped = true;
    // ⚠ 逐个接住：退订失败不能把它后面的步骤带走。
    //   ★ `unlisten` 是 async 的 —— 它报错时是「被拒绝的 Promise」，
    //     同步 try/catch 接不住（开发时 HMR 会走这一步，那时事件插件已经没了），
    //     所以要把返回值也当成 Promise 接一手
    for (const dispose of this.disposers) {
      try {
        void Promise.resolve(dispose()).catch(() => {});
      } catch {
        // 同步就抛的也兜住
      }
    }
    this.disposers = [];
    try {
      await invoke("lsp_stop", { session: this.id });
    } catch {
      // 进程可能已经自己退了，不值得报错
    }
  }

  // ---------- JSON-RPC ----------

  /** 发一个请求并等回应 */
  private request(method: string, params: unknown): Promise<JsonRpc> {
    const id = this.nextRequestId++;
    // ⚠ executor 这里要用**块体**：写成表达式体 `(resolve) => this.waiting.set(id, resolve)`
    //   的话，TS 6.0.3 会认为下面 `.catch` 回调里的 `resolve` 不存在（TS2304）。
    //   （实测：单单拿这几行放一个空文件里也能复现，不是本文件的其它问题）
    const promise = new Promise<JsonRpc>((resolve) => {
      this.waiting.set(id, resolve);
    });
    void this.send({ jsonrpc: "2.0", id, method, params }).catch((error) => {
      // ⚠ 发不出去也要把等待者叫醒 —— 否则这个 Promise 永远挂着，
      //   连带着启动流程卡死在「正在初始化」
      log(`[LSP] 发送 ${method} 失败：${String(error)}`);
      this.settle(id, { jsonrpc: "2.0", error: String(error) });
    });
    return promise;
  }

  /** 叫醒一个在等回音的请求。没人在等就当没发生 */
  private settle(id: number, message: JsonRpc): void {
    const waiter = this.waiting.get(id);
    if (waiter === undefined) return;
    this.waiting.delete(id);
    waiter(message);
  }

  /** 发一个通知（不要回应） */
  private notify(method: string, params: unknown): void {
    void this.send({ jsonrpc: "2.0", method, params });
  }

  private send(message: JsonRpc): Promise<void> {
    if (this.stopped) return Promise.resolve();
    return invoke("lsp_send", { session: this.id, message: JSON.stringify(message) });
  }

  private handle(message: JsonRpc): void {
    // ① 是别人**回应我们**的请求
    if (message.id !== undefined && message.method === undefined) {
      this.settle(message.id as number, message);
      return;
    }

    // ② 是服务器**反过来问我们**（registerCapability / workDoneProgress/create …）
    //    ★★ 必须回一条，哪怕内容只是 null ——
    //       不回的话服务器会一直等这个请求的回音，表现为「起来之后就再也不动了」，
    //       而且两边都不报错
    if (message.id !== undefined && message.method !== undefined) {
      log(`[LSP] 服务器请求 ${message.method}（暂不支持，回 null）`);
      void this.send({ jsonrpc: "2.0", id: message.id, result: null });
      return;
    }

    // ③ 通知
    if (message.method === "textDocument/publishDiagnostics") {
      this.publishDiagnostics(message.params as { uri: string; diagnostics: LspDiagnostic[] });
      return;
    }
    // 其余通知（日志、进度…）先忽略
  }

  private publishDiagnostics(params: { uri: string; diagnostics: LspDiagnostic[] }): void {
    const path = uriToPath(params.uri);
    const model = host?.findModel(path);
    if (model === undefined || model === null) return;

    // ★ 夹列号用 model 的真实行宽 —— 见 toMarker 里的说明
    const markers = params.diagnostics.map((diagnostic) =>
      toMarker(diagnostic, (lineNumber) =>
        lineNumber >= 1 && lineNumber <= model.getLineCount()
          ? model.getLineMaxColumn(lineNumber)
          : 1,
      ),
    );

    // ★ 用 "lsp" 当 owner：Monaco 按 owner 分组，
    //   换一批诊断时只清掉这个 owner 的，不会误伤别的（比如以后的 linter）
    monaco.editor.setModelMarkers(model, "lsp", markers);

    // ★ 只在**条数变了**的时候记一行。
    //   每敲一个字服务器都会推一次诊断，不这么挡的话「输出」面板会被刷爆，
    //   而刷屏的日志等于没有日志
    const previous = lastDiagnosticCount.get(path);
    if (previous !== markers.length) {
      lastDiagnosticCount.set(path, markers.length);
      log(
        markers.length === 0
          ? `[LSP] ${baseName(path)}：没有问题`
          : `[LSP] ${baseName(path)}：${markers.length} 条诊断（第一处在 ${markers[0].startLineNumber} 行）`,
      );
    }
  }
}
