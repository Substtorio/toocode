/**
 * 调试适配器客户端（DAP）。
 *
 * ★ 为什么和 `lsp.ts` 分开：**两个协议只是长得像，语义完全不同**。
 *   分帧（`Content-Length` + JSON）是一样的，但一边是「代码长什么样」，
 *   一边是「程序跑到哪儿了」。混在一个文件里，排查问题时得同时怀疑两层。
 *   传输层（Rust 那边）倒是**真的共用**了同一份代码。
 *
 * ★★★ 一条和教科书不一样的实测事实（写在这里免得以后重新踩）：
 *   **`launch` 的响应不会马上回来**。debugpy 把它压到 `configurationDone`
 *   之后才发（那时候调试目标才真的跑起来）。
 *   ⇒ 所以 `launch` 只能**发出去然后不管**，绝不能 `await` ——
 *     等它的话整个启动流程就死在那儿了（实测：Node 会报 unsettled top-level await）。
 *
 * ★★ 同样实测出来的顺序（debugpy 1.8.22）：
 *   ```
 *   initialize(等响应)
 *     → launch（不等响应）
 *     → 「initialized」事件 ← **在这一步之后才来**，之前发 setBreakpoints 会被拒
 *     → setBreakpoints（这时候才被接受）
 *     → configurationDone
 *     → process / thread(started) / stopped
 *   ```
 *   ⚠ 规范里 `initialized` 是紧跟在 `initialize` 响应后面的，debugpy 不是 ——
 *     按规范写的客户端会卡在「等 initialized」，而且**不报错**，只是不动
 */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

// ============================ 类型 ============================

/** 一个可用的调试适配器（Rust 侧扫出来的） */
export interface DapAdapterInfo {
  id: string;
  label: string;
  /** 管哪些扩展名 —— 前端拿它决定「当前这个文件能不能调试」 */
  extensions: string[];
  program: string;
  args: string[];
}

/** 断点在编辑器里的一行 */
export interface DapBreakpoint {
  line: number;
  /** 适配器确认这个断点真的设上了。false = 设不上（比如那行没有可执行代码） */
  verified: boolean;
}

export interface DapStackFrame {
  id: number;
  name: string;
  line: number;
  column: number;
  /**
   * 这一帧在哪个文件里。
   *
   * ★ 命中断点时要靠它决定「打开哪个文件、高亮哪一行」——
   *   光有行号是不够的（栈里第 2 帧可能在另一个文件）
   *
   * ⚠ 可能是 null：适配器对「框架内部代码」不给路径，那时就别去动编辑器
   */
  path: string | null;
}

export interface DapScope {
  name: string;
  variablesReference: number;
  expensive: boolean;
}

export interface DapVariable {
  name: string;
  value: string;
  type?: string;
  /** 0 = 这个值不可展开（普通值）；非 0 = 可以继续往下展开 */
  variablesReference: number;
}

/** `stopped` 事件。reason 常见值：`breakpoint` / `step` / `pause` / `exception` */
export interface DapStoppedEvent {
  reason: string;
  threadId: number;
}

export interface DapAdapterCapabilities {
  supportsConfigurationDoneRequest?: boolean;
  supportsEvaluateForHovers?: boolean;
  supportsConditionalBreakpoints?: boolean;
  supportsFunctionBreakpoints?: boolean;
  supportsTerminateRequest?: boolean;
}

/**
 * App 提供的口子。和 LSP 那边一个套路 —— 两边都不知道对方内部长什么样
 */
export interface DapHost {
  log: (line: string) => void;

  /**
   * 某个文件当前有哪些断点（行号）。
   *
   * ★ 断点的**真相在 UI 那边**（数组 + 编辑器装饰都是它维护的），
   *   这里只是「要发给适配器的时候问它一下」——
   *   两份各存一份的话迟早不一致，而不一致的表现是
   *   「界面上有红点、程序却不停」
   */
  breakpointsFor: (path: string) => number[];

  /** 适配器回报「这些断点最后落在哪、验上没有」（行号可能被它挪动） */
  onBreakpoints: (path: string, breakpoints: DapBreakpoint[]) => void;
  onStopped: (event: DapStoppedEvent) => void;
  onContinued: () => void;
  onTerminated: () => void;
  /** 程序输出（stdout / stderr） */
  onOutput: (text: string) => void;
  /** 会话没了（进程退出 / 出错 / 被停） */
  onEnded: () => void;
}

// ============================ 会话 ============================

/** DAP 的每个请求都带一个自增 seq，响应靠 request_seq 对回来 */
let seq = 0;

/** 等 `initialized` 事件的上限。★ 不能无限等 —— 适配器不吭声时界面会永远卡在「启动中」 */
const INITIALIZED_TIMEOUT_MS = 15000;

/** 发一个请求后等响应的上限（`launch` 不在此列 —— 它压根不等） */
const REQUEST_TIMEOUT_MS = 15000;

interface PendingRequest {
  command: string;
  settle: (body: unknown | null) => void;
}

/** 一个调试会话 = 一个适配器进程（或一个端口连接）+ 一次调试活动 */
class DapSession {
  readonly id: string;
  /** 给人看的名字，只用在日志里 */
  private readonly label: string;
  private readonly host: DapHost;
  private readonly disposers: UnlistenFn[] = [];
  private readonly pending = new Map<number, PendingRequest>();

  /**
   * `launch` 请求里那个 `type`。
   *
   * ★ 它由适配器决定（debugpy 是 "python"），两种传输都一样 ——
   *   它描述的是「谁来跑这个程序」，不是「通道怎么连」
   */
  private launchType = "python";

  /**
   * `initialized` 事件还没来的时候挂在这儿。
   *
   * ★ 为什么不用一个布尔标记等轮询：事件是**异步一次**的，
   *   用 Promise 表达「等它来」最自然，而且天然带「只 resolve 一次」的语义
   */
  private initializedWaiter: (() => void) | null = null;

  /** 会话结束了没有。★ 结束了就别再往里发消息（发了也没人回） */
  private ended = false;

  /** 当前停在哪个线程上 —— `continue` / 单步都要用它 */
  private threadId = 0;

  constructor(label: string, id: string, host: DapHost) {
    this.label = label;
    this.id = id;
    this.host = host;
  }

  get isEnded(): boolean {
    return this.ended;
  }

  /**
   * 起适配器（stdio）并跑起来。
   */
  async start(
    adapter: DapAdapterInfo,
    program: string,
    cwd: string | null,
    breakpointFiles: string[],
  ): Promise<void> {
    this.launchType = "python";
    await this.connect(program, cwd, breakpointFiles, () =>
      invoke("dap_start", { session: this.id, adapterId: adapter.id, cwd }),
    );
  }

  /**
   * 连到一个**已经在监听端口**的适配器。
   *
   * ★★ 和 stdio 唯一的区别就是「怎么把通道接上」——
   *   握手、断点、单步、变量……**一模一样**（实测过完整流程）
   */
  async attach(
    remote: { host: string; port: number },
    program: string,
    cwd: string | null,
    breakpointFiles: string[],
  ): Promise<void> {
    await this.connect(program, cwd, breakpointFiles, () =>
      invoke("dap_attach", { session: this.id, host: remote.host, port: remote.port }),
    );
  }

  /** 握手：两条路都走这里 */
  private async connect(
    program: string,
    cwd: string | null,
    breakpointFiles: string[],
    open: () => Promise<unknown>,
  ): Promise<void> {
    // ⚠ ★★ 必须先 listen 再 start：反了会丢掉适配器启动时吐的头几帧
    //   （终端 PTY 和 LSP 那两处踩的都是同一个坑）。
    //   也正因为这个顺序，**会话 id 由前端生成**
    this.disposers.push(
      await listen<{ session: string; body: string }>("dap-message", (event) => {
        if (event.payload.session !== this.id) return;
        this.receive(event.payload.body);
      }),
    );
    this.disposers.push(
      await listen<string>("dap-stderr", (event) => {
        if (!event.payload.startsWith(`[${this.id}]`)) return;
        // debugpy 往 stderr 写得很多（连内部报错都倒在这儿），所以**不当错误**，
        // 只是原样进输出面板 —— 真正的失败会体现在请求的响应里
        this.host.log(`[DAP] ${event.payload}`);
      }),
    );
    this.disposers.push(
      await listen<string>("dap-error", (event) => this.host.log(`[DAP] ${event.payload}`)),
    );
    this.disposers.push(
      await listen<string>("dap-exit", (event) => {
        if (event.payload !== this.id) return;
        this.host.log("[DAP] 调试适配器退出了");
        this.finish();
      }),
    );

    await open();

    // ---- 握手 ----
    const capabilities = await this.request("initialize", {
      adapterID: this.launchType,
      clientID: "toocode",
      clientName: "Toocode",
      linesStartAt1: true,
      columnsStartAt1: true,
      pathFormat: "path",
      supportsRunInTerminalRequest: false,
      supportsVariableType: true,
    });
    this.host.log(
      `[DAP] ${this.label} 就绪（configurationDone：${
        readCapability(capabilities, "supportsConfigurationDoneRequest") ? "支持" : "不支持"
      }）`,
    );

    // ★ 先挂上「等 initialized」再发 launch —— 反过来的话，
    //   适配器回得够快时我们会漏掉那个事件，然后一直等到超时
    const initialized = this.waitForInitialized();

    // ★★ launch **不 await**！响应会被压到 configurationDone 之后（见文件头说明）
    this.notify("launch", {
      type: this.launchType,
      request: "launch",
      program,
      cwd,
      // ★ `internalConsole` 让程序输出走 DAP 的 output 事件回到我们手里 ——
      //   选 externalTerminal 的话就跑到别处去了，我们什么都看不到
      console: "internalConsole",
      // 只调自己的代码（不跟进标准库），否则一单步就掉进 python 内部
      justMyCode: true,
    });

    await initialized;

    // ---- 把断点设上 ----
    for (const path of breakpointFiles) {
      await this.pushBreakpoints(path);
    }

    // ---- 告诉适配器「配置完了，开始跑」 ----
    await this.request("configurationDone", {});
    this.host.log("[DAP] 开始运行");
  }

  /** 等 `initialized` 事件。超时不是错，但要**说出来** —— 否则界面会一直卡在「启动中」 */
  private waitForInitialized(): Promise<void> {
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        if (this.initializedWaiter === null) return;
        this.initializedWaiter = null;
        this.host.log(`[DAP] 等了 ${INITIALIZED_TIMEOUT_MS / 1000} 秒也没等到 initialized，继续往下走`);
        resolve();
      }, INITIALIZED_TIMEOUT_MS);

      this.initializedWaiter = () => {
        clearTimeout(timer);
        this.initializedWaiter = null;
        resolve();
      };
    });
  }

  /** 把某个文件的断点整份发给适配器。DAP 的语义就是「整份替换」，不是增量 */
  async pushBreakpoints(path: string): Promise<DapBreakpoint[]> {
    if (this.ended) return [];
    const lines = this.host.breakpointsFor(path);
    const body = await this.request("setBreakpoints", {
      source: { path },
      // ★ 要按行号排序再发：适配器回的数组**顺序和请求一致**，
      //   不排序的话「第几个断点验上了」会对错行
      breakpoints: [...lines].sort((a, b) => a - b).map((line) => ({ line })),
    });

    const listed = (body as { breakpoints?: Array<{ line?: number; verified?: boolean }> } | null)?.breakpoints ?? [];
    const result: DapBreakpoint[] = listed.map((item, index) => ({
      line: item.line ?? lines[index] ?? 0,
      verified: item.verified === true,
    }));
    this.host.onBreakpoints(path, result);
    return result;
  }

  // ---------------------- 请求 / 通知 ----------------------

  /**
   * 发一个请求并等响应。
   *
   * ★ `launch` **不能**用它 —— 那是个特例（响应会被压后），用 `notify`
   */
  async request(command: string, args: unknown): Promise<unknown> {
    if (this.ended) return null;
    const id = ++seq;
    const body = JSON.stringify({ seq: id, type: "request", command, arguments: args ?? {} });

    const promise = new Promise<unknown | null>((resolve) => {
      this.pending.set(id, { command, settle: resolve });
      setTimeout(() => {
        const waiting = this.pending.get(id);
        if (waiting === undefined) return;
        this.pending.delete(id);
        this.host.log(`[DAP] ${command} 超时（${REQUEST_TIMEOUT_MS / 1000} 秒没回响应）`);
        resolve(null);
      }, REQUEST_TIMEOUT_MS);
    });

    await invoke("dap_send", { session: this.id, message: body });
    return promise;
  }

  /** 发一个请求但**不等响应**（`launch` / `continue` / 单步 这类会挂很久的） */
  notify(command: string, args: unknown): void {
    if (this.ended) return;
    const id = ++seq;
    void invoke("dap_send", {
      session: this.id,
      message: JSON.stringify({ seq: id, type: "request", command, arguments: args ?? {} }),
    }).catch((error) => this.host.log(`[DAP] 发 ${command} 失败：${String(error)}`));
  }

  /** 收到适配器一条消息 */
  private receive(raw: string): void {
    let message: {
      type?: string;
      event?: string;
      command?: string;
      request_seq?: number;
      success?: boolean;
      body?: unknown;
      message?: string;
    };
    try {
      message = JSON.parse(raw);
    } catch (error) {
      this.host.log(`[DAP] 收到解析不了的消息：${String(error)}`);
      return;
    }

    if (message.type === "response") {
      const waiting = message.request_seq === undefined ? undefined : this.pending.get(message.request_seq);
      if (waiting === undefined) return; // 我们不关心的响应（比如那个迟到的 launch）
      this.pending.delete(message.request_seq as number);
      if (message.success !== true) {
        this.host.log(`[DAP] ${waiting.command} 失败：${message.message ?? "适配器没给原因"}`);
      }
      waiting.settle(message.success === true ? (message.body ?? {}) : null);
      return;
    }

    if (message.type === "event") this.handleEvent(message.event ?? "", message.body);
  }

  private handleEvent(event: string, body: unknown): void {
    const payload = (body ?? {}) as Record<string, unknown>;

    switch (event) {
      case "initialized":
        this.initializedWaiter?.();
        return;

      case "stopped": {
        const threadId = typeof payload.threadId === "number" ? payload.threadId : this.threadId;
        this.threadId = threadId;
        this.host.onStopped({
          reason: typeof payload.reason === "string" ? payload.reason : "unknown",
          threadId,
        });
        return;
      }

      case "continued":
        this.host.onContinued();
        return;

      case "output": {
        // ⚠ telemetry 是适配器自己发的遥测（debugpy 上来就先发两条），
        //   丢进调试控制台只会让人困惑
        if (payload.category === "telemetry") return;
        const text = typeof payload.output === "string" ? payload.output : "";
        if (text.length > 0) this.host.onOutput(text);
        return;
      }

      case "terminated":
        this.host.log("[DAP] 调试会话结束");
        this.finish();
        return;

      default:
        // 其它事件（process / thread / module / exited / breakpoint …）暂时不用，
        // 但**不能**当成错误 —— 各家适配器都会发一堆自己的事件
        return;
    }
  }

  // ---------------------- 调试动作 ----------------------

  get currentThreadId(): number {
    return this.threadId;
  }

  async stackTrace(threadId: number): Promise<DapStackFrame[]> {
    const body = await this.request("stackTrace", { threadId });
    const frames =
      (body as { stackFrames?: Array<{ id: number; name: string; line: number; column: number; source?: { path?: string } }> } | null)
        ?.stackFrames ?? [];
    return frames.map((frame) => ({
      id: frame.id,
      name: frame.name,
      // ⚠ 适配器可能发来 line: 0（比如停在框架内部代码里）——
      //   0 行在 Monaco 里是越界的，交给调用方夹一下
      line: frame.line,
      column: frame.column,
      // ★ 统一成正斜杠：它要拿去和 `models` 的 key 比，
      //   而两边一个用 / 一个用 \ 是常事（和 LSP 那边同一个理由）
      path: typeof frame.source?.path === "string" ? frame.source.path.replace(/\\/g, "/") : null,
    }));
  }

  async scopes(frameId: number): Promise<DapScope[]> {
    const body = await this.request("scopes", { frameId });
    const scopes = (body as { scopes?: Array<{ name: string; variablesReference: number; expensive?: boolean }> } | null)?.scopes ?? [];
    return scopes.map((scope) => ({
      name: scope.name,
      variablesReference: scope.variablesReference,
      expensive: scope.expensive === true,
    }));
  }

  async variables(reference: number): Promise<DapVariable[]> {
    const body = await this.request("variables", { variablesReference: reference });
    const list = (body as { variables?: Array<{ name: string; value: string; type?: string; variablesReference?: number }> } | null)?.variables ?? [];
    return list.map((item) => ({
      name: item.name,
      value: item.value,
      type: item.type,
      variablesReference: item.variablesReference ?? 0,
    }));
  }

  /** 求一个表达式的值（调试控制台 / 悬停都用它） */
  async evaluate(expression: string, frameId?: number): Promise<string | null> {
    const body = await this.request("evaluate", {
      expression,
      frameId,
      context: frameId === undefined ? "repl" : "watch",
    });
    const result = (body as { result?: string } | null)?.result;
    return typeof result === "string" ? result : null;
  }

  /** 继续 / 单步 —— 都不等响应（它们要等到下一个断点才回） */
  continue_(): void {
    this.notify("continue", { threadId: this.threadId });
    // ★ 不等 `continued` 事件就把状态收回 —— 事件可能会晚一拍，
    //   而用户按下 F5 那一刻界面就该变成「运行中」
    this.host.onContinued();
  }

  stepOver(): void {
    this.notify("next", { threadId: this.threadId });
    this.host.onContinued();
  }

  stepInto(): void {
    this.notify("stepIn", { threadId: this.threadId });
    this.host.onContinued();
  }

  stepOut(): void {
    this.notify("stepOut", { threadId: this.threadId });
    this.host.onContinued();
  }

  pause(): void {
    this.notify("pause", { threadId: this.threadId });
  }

  /** 收工。先礼貌地说一声 `disconnect`，再把进程停掉 */
  async stop(terminateDebuggee: boolean): Promise<void> {
    if (!this.ended) {
      // ⚠ 这一步也不能 await 响应：有的适配器要等目标进程死掉才回
      this.notify("disconnect", { terminateDebuggee, restart: false });
      await new Promise((resolve) => setTimeout(resolve, 120));
    }
    await invoke("dap_stop", { session: this.id }).catch(() => {});
    this.finish();
  }

  /** 会话收尾：退订 + 把「还在等响应」的请求全部放掉 */
  private finish(): void {
    if (this.ended) return;
    this.ended = true;

    // ★ 必须把 pending 全 settle 掉：不放的话那些 Promise 永远挂着，
    //   调用方的 await 就再也回不来了（界面卡在「启动中」）
    for (const [, waiting] of this.pending) waiting.settle(null);
    this.pending.clear();

    for (const dispose of this.disposers) {
      // ⚠ unlisten 是 async 的，同步 try/catch 接不住它的 rejection
      try {
        void Promise.resolve(dispose()).catch(() => {});
      } catch {
        // 已经没了就算了
      }
    }
    this.disposers.length = 0;

    this.host.onEnded();
  }
}

/** 从 `initialize` 的响应里读一个能力开关。适配器写得不规范也不该崩 */
function readCapability(capabilities: unknown, key: keyof DapAdapterCapabilities): boolean {
  if (capabilities === null || typeof capabilities !== "object") return false;
  // ⚠ 实测：debugpy 回的 body **就是** capabilities（没有再包一层 `{capabilities:…}`），
  //   而规范写的是包一层。两边都认一下，别为了「谁对」跟适配器较劲
  const source = capabilities as Record<string, unknown>;
  const nested = source.capabilities;
  const target = nested !== null && typeof nested === "object" ? (nested as Record<string, unknown>) : source;
  return target[key] === true;
}

// ============================ 模块级入口 ============================
//
// 和 lsp.ts 同构：一个模块级的「当前会话」+ 两个开关函数。
// ★ 单会话：同时调试一个程序是编辑器里最常见的用法（VS Code 也能开多个，
//   但那需要「调试配置」这一整套东西，先不做）

let host: DapHost | null = null;
let current: DapSession | null = null;

/** 服务器清单只问一次 —— 要真起进程问，不便宜 */
let adapterCache: DapAdapterInfo[] | null = null;

export function configureDap(options: DapHost): void {
  host = options;
  // ★ 刷新页面时旧页面的 disposeDap() 不会执行 —— 页面是被拆掉的，不是「卸载」。
  //   不清的话每刷一次就多留一个适配器进程（和 LSP 那边同一个坑）
  void invoke("dap_stop_all").catch(() => {});
}

export function disposeDap(): void {
  void current?.stop(true);
  current = null;
}

/** 本机可用的调试适配器 */
export async function availableAdapters(): Promise<DapAdapterInfo[]> {
  adapterCache ??= await invoke<DapAdapterInfo[]>("dap_adapters");
  return adapterCache;
}

/** 按扩展名找一个能调试它的适配器 */
export function adapterFor(adapters: DapAdapterInfo[], path: string): DapAdapterInfo | null {
  const dot = path.lastIndexOf(".");
  if (dot === -1) return null;
  const extension = path.slice(dot + 1).toLowerCase();
  return adapters.find((adapter) => adapter.extensions.includes(extension)) ?? null;
}

/** 当前有没有在调试 */
export function isDebugging(): boolean {
  return current !== null && !current.isEnded;
}

/**
 * 开始调试。
 *
 * @param breakpointFiles 要先把断点设上的文件（**包括当前没打开但打了红点的**）
 * @returns 成功起来了没有
 */
export async function startDebugging(
  adapter: DapAdapterInfo,
  program: string,
  cwd: string | null,
  breakpointFiles: string[],
): Promise<boolean> {
  if (host === null) return false;
  if (current !== null) await current.stop(true);

  // ★ 会话 id 里掺时间戳：光用自增序号的话，刷新页面后会从 1 重新数，
  //   和上一页留在 Rust 里的那个**重名**（LSP 那边踩过这个坑，
  //   症状是「新会话被旧的退出事件标记成已停止」，然后什么都不动了）
  const session = new DapSession(adapter.label, `dap-${Date.now().toString(36)}`, host);
  current = session;

  try {
    await session.start(adapter, program, cwd, breakpointFiles);
    return true;
  } catch (error) {
    host.log(`[DAP] 启动失败：${String(error)}`);
    await session.stop(true);
    current = null;
    return false;
  }
}

/**
 * 连到一个已经跑着的调试适配器端口，然后调试 `program`。
 *
 * ★ 什么场景用：适配器**只**提供端口模式（js-debug 那一系的 server）、
 *   或者适配器根本不在本机（远程调试）
 * ⚠ 不负责把那个适配器拉起来 —— 那是用户自己的事（比如先在终端里跑
 *   `python -m debugpy.adapter --port 5678`）
 */
export async function attachToDebugPort(
  remote: { host: string; port: number },
  program: string,
  cwd: string | null,
  breakpointFiles: string[],
): Promise<boolean> {
  if (host === null) return false;
  if (current !== null) await current.stop(true);

  const label = `端口 ${remote.host}:${remote.port}`;
  const session = new DapSession(label, `dap-${Date.now().toString(36)}`, host);
  current = session;

  try {
    await session.attach(remote, program, cwd, breakpointFiles);
    return true;
  } catch (error) {
    host.log(`[DAP] 连接调试端口失败：${String(error)}`);
    await session.stop(true);
    current = null;
    return false;
  }
}

/** 会话还活着的时候拿它做点事 */
export function currentSession(): DapSession | null {
  return current !== null && !current.isEnded ? current : null;
}
