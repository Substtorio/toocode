<script setup lang="ts">
/**
 * 底部面板的「终端」页。
 *
 * ★ 生命周期：这个组件**必须常驻**，不能跟着面板标签切换销毁 ——
 *   xterm 实例一销毁，滚动历史和当前会话就全没了（VS Code 里切走切回终端还在）。
 *   所以外层用 v-show 而不是 v-if 控制显隐。
 *
 * 数据流（和 Rust 侧对应）：
 *   键盘输入 --term.onData--> pty_write
 *   尺寸变化 --term.onResize--> pty_resize
 *   Rust 读线程 --emit("pty-output")--> 这里解码 base64 → term.write(字节)
 */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { FitAddon } from "@xterm/addon-fit";
import { Terminal } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { nextTick, onBeforeUnmount, onMounted, ref, watch } from "vue";

const props = defineProps<{
  /** 面板当前是不是停在「终端」这一页 */
  visible: boolean;
  /** 打开的工作区目录，作为 shell 的起始目录 */
  cwd: string | null;
  /** 主题 id。变了要重设 xterm 的配色 */
  theme: string;
}>();

const containerRef = ref<HTMLElement | null>(null);

let term: Terminal | null = null;
let fitAddon: FitAddon | null = null;
let unlisten: UnlistenFn | null = null;
let resizeObserver: ResizeObserver | null = null;
let sessionId: number | null = null;

/**
 * 会话 id 由前端生成。
 * ★ 不依赖后端返回 id —— 那样「发命令」和「开始监听输出」之间就有一个窗口期，
 *   这期间 shell 如果已经在打印东西，那几段输出会丢（因为还不知道 id 是多少）
 */
let nextId = 1;

/**
 * 终端用的 ANSI 16 色。
 *
 * ★ 为什么必须自己带一套：主题文件里**没有** `terminal.ansi*` 这些键
 *   （dark/light 六个主题文件全列过，0 个）—— 它们属于 VS Code **代码里的默认值**，
 *   和 `--color-shell-bg` 一个情况。
 *   下面的值是从 VS Code 的编译产物里直接挖出来的
 *   （`workbench.desktop.main.js` 里那张 `{ index, defaults: { light, dark } }` 表），
 *   不是拍脑袋编的
 *
 * ⚠ ★ 关键差别在**黄色**：深色用亮黄 `#e5e510`（黑底上醒目），
 *   浅色必须换成暗橄榄黄 `#949800` —— 亮黄铺在白底上根本看不清。
 *   PowerShell 恰好用黄色显示目录名 / 警告，所以这一个是必踩的
 *
 * 没配这份调色板时，xterm 用它内置的一套 —— 那是给深色背景挑的，
 * 浅色主题下黄、青、绿全都在白底上发糊
 */
const ANSI_PALETTE = {
  dark: {
    black: "#000000",
    red: "#cd3131",
    green: "#0dbc79",
    yellow: "#e5e510",
    blue: "#2472c8",
    magenta: "#bc3fbc",
    cyan: "#11a8cd",
    white: "#e5e5e5",
    brightBlack: "#666666",
    brightRed: "#f14c4c",
    brightGreen: "#23d18b",
    brightYellow: "#f5f543",
    brightBlue: "#3b8eea",
    brightMagenta: "#d670d6",
    brightCyan: "#29b8db",
    brightWhite: "#e5e5e5",
  },
  light: {
    black: "#000000",
    red: "#cd3131",
    green: "#107c10",
    yellow: "#949800",
    blue: "#0451a5",
    magenta: "#bc05bc",
    cyan: "#0598bc",
    white: "#555555",
    brightBlack: "#666666",
    brightRed: "#f14c4c",
    brightGreen: "#14ce14",
    brightYellow: "#b5ba00",
    brightBlue: "#3b8eea",
    brightMagenta: "#d670d6",
    brightCyan: "#29b8db",
    brightWhite: "#a5a5a5",
  },
};

/** 从当前主题的 CSS 变量里取配色 + 对应那套 ANSI 调色板 */
function readTheme() {
  if (!term) return;
  const style = getComputedStyle(document.documentElement);
  const pick = (name: string, fallback: string) => style.getPropertyValue(name).trim() || fallback;

  term.options.theme = {
    background: pick("--color-editor-bg", "#1e1e1e"),
    foreground: pick("--color-text", "#cccccc"),
    cursor: pick("--color-text-emphasis", "#ffffff"),
    selectionBackground: pick("--color-selection", "#094771"),
    ...ANSI_PALETTE[props.theme === "light" ? "light" : "dark"],
  };
}

async function startSession() {
  const container = containerRef.value;
  if (!container || term) return;

  term = new Terminal({
    // 让 xterm 自己算行高，不要写死 —— 写死在不同 DPI 下会错位
    fontSize: 12,
    fontFamily: 'Consolas, "Courier New", monospace',
    cursorBlink: true,
    scrollback: 5000,
    // ★ convertEol 不能开（那是给"只有一个输出流"的伪终端用的）：
    //   真实 PTY 的换行由 shell 自己控制，开了会把 `\n` 变成回车+换行两下
    convertEol: false,
  });

  fitAddon = new FitAddon();
  term.loadAddon(fitAddon);
  term.open(container);
  readTheme();
  fitAddon.fit();

  const id = nextId++;
  sessionId = id;

  // ★ 先挂监听、再 spawn。反过来就会丢掉 shell 启动时那几行输出
  try {
    unlisten = await listen<{ id: number; data: string }>("pty-output", (event) => {
      if (event.payload.id !== id) return;
      // base64 → 字节 → 直接交给 xterm。
      // ★ 一定传 Uint8Array，不要 decode 成字符串：多字节字符可能横跨两个 chunk，
      //   解码会在边界处变成 `�`
      const bytes = Uint8Array.from(atob(event.payload.data), (char) => char.charCodeAt(0));
      term?.write(bytes);
    });
  } catch (error) {
    term.write(`\r\n\x1b[31m无法订阅终端输出：${String(error)}\x1b[0m\r\n`);
    return;
  }

  try {
    await invoke("pty_spawn", {
      id,
      cols: term.cols,
      rows: term.rows,
      cwd: props.cwd ?? null,
    });
  } catch (error) {
    // ★ 起不来时别静默失败（比如 powershell 不在 PATH 上、或浏览器里跑）。
    //   直接把原因写进终端，总比一个空白窗口不报错强
    term.write(`\r\n\x1b[31m无法启动终端：${String(error)}\x1b[0m\r\n`);
    return;
  }

  term.onData((data) => {
    // ⚠ 末尾的 .catch 不能省：`void invoke(...)` 只是丢弃返回值，
    //   **不会接住 rejection**。这条链路在没有 Tauri 时会 reject，
    //   不接住就是一条未处理的 promise rejection
    void invoke("pty_write", { id, data }).catch(() => {});
  });

  // xterm 自己 resize 之后才通知 PTY —— 反过来会算出不一致的列数
  term.onResize(({ cols, rows }) => {
    void invoke("pty_resize", { id, cols, rows }).catch(() => {});
  });

  // 面板被拖动改高度时，容器尺寸变了要重算列数
  resizeObserver = new ResizeObserver(() => {
    // 容器隐藏时（display: none）尺寸是 0，这时 fit() 会把 cols 算成 0，
    // 传给 PTY 会让 shell 疯狂重排。所以隐藏时跳过
    if (container.offsetParent === null) return;
    try {
      fitAddon?.fit();
    } catch {
      // fit 在容器尺寸为 0 时可能抛异常，忽略即可
    }
  });
  resizeObserver.observe(container);
}

onMounted(() => {
  // 只有真的要显示时才起会话 —— 用户从没点过「终端」就不用白起一个 shell
  if (props.visible) void startSession();
});

// 面板标签切到「终端」时才真正开会话（第一次）
watch(
  () => props.visible,
  async (isVisible) => {
    if (!isVisible) return;
    if (!term) {
      await startSession();
    }
    // ★ 从 display: none 恢复过来时，xterm 量到的尺寸还是隐藏状态下的，
    //   必须等 DOM 更新完再 fit，否则列数不对（换行位置全乱）
    await nextTick();
    fitAddon?.fit();
    term?.focus();
  },
);

watch(
  () => props.theme,
  () => readTheme(),
);

onBeforeUnmount(() => {
  unlisten?.();
  resizeObserver?.disconnect();
  if (sessionId !== null) void invoke("pty_kill", { id: sessionId }).catch(() => {});
  term?.dispose();
  term = null;
});
</script>

<template>
  <div ref="containerRef" class="terminal-host" />
</template>

<style scoped>
.terminal-host {
  /* ★ 绝对定位填满 .panel-body。
     它父级是 overflow: auto，用 height: 100% 会陷进「100% 到底是指谁」的循环，
     最后塔成一个很小的值（子元素高度靠父内容高，父内容高又靠子元素） */
  position: absolute;
  inset: 0;
  /* xterm 自己会算内边距，这里给一点让它别贴着面板边缘 */
  padding: 6px 8px;
  box-sizing: border-box;
}
</style>
