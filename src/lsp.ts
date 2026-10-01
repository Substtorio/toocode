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
    ...start,
    ...{ endLineNumber: end.lineNumber, endColumn: end.column },
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
}

/** 拆掉所有会话（退出 / 换工作区时用） */
export async function disposeLsp(): Promise<void> {
  const all = [...sessions.values()];
  sessions.clear();
  await Promise.all(all.map((session) => session.stop()));
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

  constructor(
    private readonly server: LspServerInfo,
    private readonly root: string | null,
  ) {
    this.id = `lsp-${++LspSession.sequence}`;
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
    await this.request("initialize", {
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
  }

  openDocument(path: string, languageId: string): void {
    const model = host?.findModel(path);
    if (model === undefined || model === null) return;
    this.versions.set(path, 1);
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
    this.notify("textDocument/didClose", { textDocument: { uri: pathToUri(path) } });
  }

  async stop(): Promise<void> {
    if (this.stopped) return;
    this.stopped = true;
    for (const dispose of this.disposers) dispose();
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
    const promise = new Promise<JsonRpc>((resolve) => this.waiting.set(id, resolve));
    void this.send({ jsonrpc: "2.0", id, method, params }).catch((error) => {
      // ⚠ 发不出去也要把等待者叫醒 —— 否则这个 Promise 永远挂着，
      //   连带着启动流程卡死在「正在初始化」
      this.waiting.delete(id);
      log(`[LSP] 发送 ${method} 失败：${String(error)}`);
      resolve({ jsonrpc: "2.0", error: String(error) });
    });
    return promise;
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
      const settle = this.waiting.get(message.id as number);
      if (settle) {
        this.waiting.delete(message.id as number);
        settle(message);
      }
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
  }
}
