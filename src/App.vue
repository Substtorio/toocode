<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, provide, ref, shallowRef, watch } from "vue";
import * as monaco from "monaco-editor";
import { invoke } from "@tauri-apps/api/core";
// 窗口 API：拦「关闭窗口」要用它。
// 注意它和上面那两个不是同一个模块 —— Tauri 的 API 是分模块导出的
import { getCurrentWindow } from "@tauri-apps/api/window";
// 起个别名：open 会和浏览器的 window.open 同名，容易看混
// message 是系统原生对话框，用来做「未保存」确认
import { message, open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
// opener 插件：把文件交给系统处理 —— 图片、PDF、压缩包这些
// 我们用编辑器本来就打不开（二进制），交给系统看才是正解
import { openPath, revealItemInDir } from "@tauri-apps/plugin-opener";
// 把 TextMate 语法接成 Monaco 分词器的地方（插件机制的执行层）
import { registerTextMateLanguage } from "./textmate";
// 把 VS Code 的主题文件翻译成 Monaco 主题的地方（插件机制的第二块）
import { loadVscodeTheme } from "./vscodeTheme";
import FileTreeNode from "./components/FileTreeNode.vue";
import { fileTreeSelectionKey } from "./injectionKeys";
import type { FileNode } from "./types";

// 当前打开的文件夹。没打开时为 null。
// 路径统一用 / —— Rust 侧返回的也统一了，两边保持一致
const workspaceRoot = ref<string | null>(null);

// 文件树数据：从 Rust 侧递归读回来的真实目录
const fileTree = ref<FileNode[]>([]);
// 读目录失败时的原因（路径不存在、权限不足等）
const treeError = ref<string | null>(null);

// 界面布局状态。
// 活动栏图标和「查看」菜单都会改它，所以放在这里，
// 而不是塞进菜单那一节（代码位置应该反映职责）
const sidebarVisible = ref(true);

/** 切换侧栏显隐 —— VS Code 里点活动栏上「已激活」的图标就是这个行为 */
function toggleSidebar() {
  sidebarVisible.value = !sidebarVisible.value;
}

// ---------- 最近打开的文件夹 ----------
//
// ★ 这里是**两个独立的 key**，别合成一个：
//   LAST_FOLDER_KEY   = 下次启动要先打开哪个（只记一个）
//   RECENT_FOLDERS_KEY = 最近打开过哪些（一个列表）
//   合并的话，「关闭文件夹」就没办法只影响前者了 ——
//   要么关不掉（下次启动又打开），要么把用户想去回的文件夹也一起忘掉
const LAST_FOLDER_KEY = "new_vscode:lastFolder";

// localStorage 是 WebView 自带的持久化存储，只能存字符串，所以列表要自己 JSON 序列化。
// 存「最近的在前」的数组，并封顶 —— 不封顶的话它会无限长大
const RECENT_FOLDERS_KEY = "new_vscode:recentFolders";
const MAX_RECENT_FOLDERS = 8;

function loadRecentFolders(): string[] {
  let parsed: unknown = [];

  const raw = window.localStorage.getItem(RECENT_FOLDERS_KEY);
  if (raw !== null) {
    try {
      parsed = JSON.parse(raw);
    } catch {
      // 存的东西坏了（手改过、写到一半断电）就当没有，不要让它把启动搞崩
      parsed = [];
    }
  }

  // 从 localStorage 读进来的一律先验证形状 ——
  // JSON.parse 成功不代表里面装的是我们想要的东西
  const list = Array.isArray(parsed)
    ? parsed.filter((item): item is string => typeof item === "string")
    : [];

  // 首次升级时把老版本记住的那个补进来。
  // 不补的话，用户一升级「最近」就是空的，上次用的文件夹像是被抹掉了
  const legacy = window.localStorage.getItem(LAST_FOLDER_KEY);
  if (legacy && !list.includes(legacy)) list.unshift(legacy);

  return list.slice(0, MAX_RECENT_FOLDERS);
}

const recentFolders = ref<string[]>(loadRecentFolders());

function saveRecentFolders(next: string[]) {
  recentFolders.value = next;
  window.localStorage.setItem(RECENT_FOLDERS_KEY, JSON.stringify(next));
}

/** 把一个文件夹记到最近列表的最前面 */
function rememberFolder(path: string) {
  // 先去重再插到最前 —— 重复打开的文件夹应该「提到最前」，而不是出现两次
  const rest = recentFolders.value.filter((item) => item !== path);
  saveRecentFolders([path, ...rest].slice(0, MAX_RECENT_FOLDERS));
}

/** 从最近列表里剔除一个 */
function forgetFolder(path: string) {
  saveRecentFolders(recentFolders.value.filter((item) => item !== path));
}

// ---------- 主题 ----------
//
// 一个主题其实是两件事的组合：
//   1. 一套 CSS 变量的值（管 UI 配色）
//   2. 一个 Monaco 主题 id（管编辑器内部配色）
// 两者必须同步 —— 只改一个就会出现「UI 亮了但编辑器还是黑的」这种半吊子效果
// 主题内容直接取自 VS Code 自带的两个默认主题（Dark Modern / Light Modern）。
// 名字也跟着它们走 —— 「Dark+」是 VS Code 1.7x 之前那套经典配色的名字，不是这个
const THEMES = [
  { id: "dark", label: "Dark Modern", monaco: "vs-dark" },
  { id: "light", label: "Light Modern", monaco: "vs" },
] as const;

type ThemeId = (typeof THEMES)[number]["id"];

const THEME_KEY = "new_vscode:theme";

/** 读本地记录的主题；存的值不合法（比如手改坏了）就回落到深色 */
function readSavedTheme(): ThemeId {
  const saved = window.localStorage.getItem(THEME_KEY);
  const found = THEMES.find((t) => t.id === saved);
  return found ? found.id : "dark";
}

const currentThemeId = ref<ThemeId>("dark");

const currentThemeLabel = computed(
  () => THEMES.find((t) => t.id === currentThemeId.value)?.label ?? "",
);

/**
 * VS Code 颜色键 → 我们自己的 CSS 变量。
 *
 * ★ 为什么需要这张表：VS Code 主题里的颜色其实是**两个世界**的混合体 ——
 *   `editor.*` 是编辑器内部的（Monaco 自己会认），
 *   而 `sideBar.*` / `tab.*` / `statusBar.*` / `activityBar.*` / `titleBar.*`
 *   是**工作台**颜色，Monaco 根本不认（它只管编辑器那一块）——
 *   可我们自己的 UI 偏偏就是照这些名字设计的。
 *   所以「换主题」在我们这儿天生分两半：一半透传给 Monaco，一半写成 CSS 变量
 *
 * ★ 只映射**深浅主题都有**的那些键。某一边没有的（比如 `menu.background`
 *   在浅色里就没有）就不列进来 —— 那种情况继续用 <style> 里写死的值，
 *   两边都不会出现空白颜色
 */
const CSS_VAR_BY_COLOR: Array<[cssVar: string, vscodeKey: string]> = [
  ["--color-activitybar-bg", "activityBar.background"],
  ["--color-sidebar-bg", "sideBar.background"],
  ["--color-editor-bg", "editor.background"],
  ["--color-tabbar-bg", "editorGroupHeader.tabsBackground"],
  ["--color-tab-bg", "tab.inactiveBackground"],
  ["--color-tab-bg-hover", "tab.hoverBackground"],
  ["--color-statusbar-bg", "statusBar.background"],
  ["--color-statusbar-fg", "statusBar.foreground"],
  ["--color-menubar-bg", "titleBar.activeBackground"],
  ["--color-text", "sideBar.foreground"],
  ["--color-text-dim", "tab.inactiveForeground"],
  ["--color-text-emphasis", "tab.activeForeground"],
  ["--color-icon", "activityBar.inactiveForeground"],
  ["--color-icon-active", "activityBar.foreground"],
  ["--color-link", "textLink.foreground"],
];

/** 每个主题从 VS Code 主题文件里读到的颜色表（读不到就是空的，用 CSS 里的默认值兜底） */
const VSCODE_COLORS_BY_THEME: Record<string, Record<string, string>> = {};

/**
 * 我们自己的主题 id → 真正交给 Monaco 的主题 id。
 *
 * ★ 一开始是空的，于是回落到 THEMES 里写的那两个（vs-dark / vs）。
 *   等 `loadVscodeThemes()` 把 VS Code 的 Dark+ / Light+ 接上之后会填上 ——
 *   因为内置那两个只有 45 条规则，远远盖不住 TextMate 那套 scope 命名。
 *   用「查表覆盖」而不是直接改 THEMES，是为了把「默认值」和「运行时覆盖」分开
 */
const MONACO_THEME_BY_THEME: Record<string, string> = {};

function monacoThemeFor(id: ThemeId): string {
  return MONACO_THEME_BY_THEME[id] ?? THEMES.find((t) => t.id === id)?.monaco ?? "vs-dark";
}

function applyTheme(id: ThemeId) {
  const theme = THEMES.find((t) => t.id === id);
  if (!theme) return;

  currentThemeId.value = id;

  // UI 配色：先改 <html> 上的属性，CSS 里用 :root[data-theme="..."] 接住。
  // 这是**兜底值** —— VS Code 主题读不到时就用它
  document.documentElement.dataset.theme = id;

  // ★ 再把 VS Code 主题里的工作台颜色铺上去。
  //   写在 <html> 的 inline style 上，优先级高于 <style> 里的 :root，
  //   所以能盖掉兜底值；也正因为如此，**切换主题时必须先清干净** ——
  //   不清的话「深色 → 浅色」时深色的值会赖着不走
  for (const [cssVar] of CSS_VAR_BY_COLOR) {
    document.documentElement.style.removeProperty(cssVar);
  }
  const vscodeColors = VSCODE_COLORS_BY_THEME[id];
  if (vscodeColors) {
    for (const [cssVar, vscodeKey] of CSS_VAR_BY_COLOR) {
      const value = vscodeColors[vscodeKey];
      if (value) document.documentElement.style.setProperty(cssVar, value);
    }
  }

  // 编辑器配色：Monaco 自己管渲染，CSS 管不到它，只能调 API
  monaco.editor.setTheme(monacoThemeFor(id));

  window.localStorage.setItem(THEME_KEY, id);
}

/** 点一下切到下一个主题（只有两个主题时就是来回切） */
function cycleTheme() {
  const index = THEMES.findIndex((t) => t.id === currentThemeId.value);
  const next = THEMES[(index + 1) % THEMES.length];
  if (next) applyTheme(next.id);
}

// 立即应用。放在 setup 顶层（而不是 onMounted）是为了在首次渲染前就把主题定好，
// 否则会先闪一下默认深色再变成浅色
applyTheme(readSavedTheme());

// ---------- 主题：接 VS Code 自带的 Dark+ / Light+ ----------

interface ThemeEntry {
  id: string;
  label: string;
  /** vs-dark / vs / hc-black / hc-light */
  uiTheme: string;
  path: string;
}

/**
 * 把 VS Code 自带的两套主题接过来当 Monaco 主题用。
 *
 * ★ 这不是「锦上添花」，是让语法高亮能看的**必要条件**：
 *   Monaco 内置的 vs-dark 只有 45 条规则，而 TextMate 的 scope 命名空间大得多 ——
 *   标签名（`entity.name.tag`）、属性名（`entity.other.attribute-name`）、
 *   CSS 选择器（`entity.other.attribute-name.class.css`）在它里面**一条都没有** ——
 *   匹配不到就渲染成默认色，看起来就是「颜色很单调」。
 *   VS Code 的 Dark+ 有 169 个规则，正好补上这些
 *
 * ★ 主题文件是现成的：VS Code 内置扩展 `theme-defaults` 里就有
 *   `dark_plus.json` / `light_plus.json`，而且格式和 Monaco 几乎一样
 */
async function loadVscodeThemes() {
  try {
    const entries = await invoke<ThemeEntry[]>("scan_theme_extensions");
    const readFile = (path: string) => invoke<string>("read_file", { path });

    // ★ 用 **Modern** 那两个（VS Code 现在的默认主题），而不是经典的 Dark+ / Light+：
    //   Modern 才带完整的**工作台配色**（130 条：侧栏、标签栏、状态栏、活动栏、标题栏……），
    //   而 Dark+ 的 colors 是空的 —— 那些颜色在 VS Code 里属于代码里的默认值，不在主题文件里。
    //   我们的 UI 要跟着变，就只能靠 Modern。
    //   （它的 include 链是三层：dark_modern → dark_plus → dark_vs，tokenColors 照样齐）
    const wanted: Array<{ ourId: ThemeId; fileName: string; base: "vs-dark" | "vs"; monacoId: string }> = [
      { ourId: "dark", fileName: "dark_modern.json", base: "vs-dark", monacoId: "vscode-dark-modern" },
      { ourId: "light", fileName: "light_modern.json", base: "vs", monacoId: "vscode-light-modern" },
    ];

    for (const item of wanted) {
      const entry =
        entries.find((theme) => theme.path.endsWith(`/${item.fileName}`)) ??
        entries.find((theme) => theme.uiTheme === item.base);
      if (!entry) continue;

      const loaded = await loadVscodeTheme({ path: entry.path, readFile, base: item.base });
      monaco.editor.defineTheme(item.monacoId, loaded.theme);
      MONACO_THEME_BY_THEME[item.ourId] = item.monacoId;
      // 工作台那一半要留给 applyTheme 铺到 CSS 变量上
      VSCODE_COLORS_BY_THEME[item.ourId] = loaded.colors;
    }

    // 定义好了还得重新应用一次 ——
    // applyTheme 在 setup 顶层就跑过了，那时这两个主题还不存在
    applyTheme(currentThemeId.value);
  } catch (error) {
    console.warn("[主题] 接 VS Code 主题失败（不影响其它功能）：", error);
  }
}

// ---------- 编辑器字号 ----------
//
// Ctrl + 滚轮缩放是 Monaco **内置**的能力（`mouseWheelZoom`，默认关闭）。
// 所以这里不用自己监听 wheel 事件、也不用自己算新字号 —— 只负责它不管的两件事：
//   ① 把字号记下来，下次启动恢复
//   ② 让用户知道当前是多少（状态栏提示 + 菜单里显示）

const FONT_SIZE_KEY = "new_vscode:editorFontSize";
const DEFAULT_FONT_SIZE = 14; // Monaco 和 VS Code 的默认值
const MIN_FONT_SIZE = 6;
const MAX_FONT_SIZE = 72;

/** 读本地记录的字号；没存过或值不合法就回落到默认值 */
function readSavedFontSize(): number {
  const raw = window.localStorage.getItem(FONT_SIZE_KEY);

  // 先判 null 再 Number()：Number(null) 是 0 而不是 NaN，
  // 直接算下去会得到「字号 0」这种荒唐值
  if (raw === null) return DEFAULT_FONT_SIZE;

  const saved = Number(raw);
  if (!Number.isFinite(saved)) return DEFAULT_FONT_SIZE;

  // 夹一下范围：就算 localStorage 被手改坏了，
  // 也不至于让编辑器小到看不见、或者大到一屏只有两个字
  return Math.min(MAX_FONT_SIZE, Math.max(MIN_FONT_SIZE, Math.round(saved)));
}

/** 当前字号。菜单里要显示它，所以得是响应式的 */
const currentFontSize = ref(readSavedFontSize());

/** 恢复默认字号 —— 缩到看不清的时候总得有个出口 */
function resetFontSize() {
  editorInstance.value?.updateOptions({ fontSize: DEFAULT_FONT_SIZE });
}

/**
 * 按增量调整字号（Ctrl + = 放大 / Ctrl + - 缩小，每次 1px）。
 *
 * 这里只改「字号」这一个数字，不碰 DOM、不自己做重排版 ——
 * Monaco 收到新字号会自己重新计算行高、重新渲染，
 * 并触发 onDidChangeConfiguration；上面那个订阅会顺手把新值存下来、报给状态栏。
 *
 * 所以这里**故意不写**持久化和提示 —— 写了就是两处逻辑，迟早不一致
 */
function stepFontSize(delta: number) {
  const editor = editorInstance.value;
  if (!editor) return;

  const current = editor.getOption(monaco.editor.EditorOption.fontSize);

  // 和 readSavedFontSize 共用同一对上下限 ——
  // 否则「键盘能调到的范围」和「存下来能读回的范围」会各是一套，迟早对不上
  const next = Math.min(MAX_FONT_SIZE, Math.max(MIN_FONT_SIZE, current + delta));

  // 已经顶到边界就不动 —— 白改一次会白白触发一轮订阅和状态栏提示
  if (next === current) return;

  editor.updateOptions({ fontSize: next });
}

// ---------- 标签页状态 ----------
//
// 两个状态各司其职，不要合成一个：
//   openTabs      = 打开了哪些文件、按什么顺序（决定标签栏显示什么）
//   activeTabPath = 当前激活的是哪一个（决定编辑器显示什么）
//
// 合成「一个数组 + 一个下标」也能跑，但删除中间某个标签时下标要跟着调整，
// 很容易算错。分成两个独立状态更不容易出 bug。
const openTabs = ref<string[]>([]);
const activeTabPath = ref<string | null>(null);

/**
 * 打开一个文件。
 * 已经在标签栏里就只切换激活项 —— 这就是「同一个文件不会开出两个标签」的保证。
 */
function openFile(node: FileNode) {
  if (!openTabs.value.includes(node.path)) {
    openTabs.value.push(node.path);
  }
  activeTabPath.value = node.path;
}

/** 点击标签栏上的标签 */
function activateTab(path: string) {
  activeTabPath.value = path;
}

// ---------- 未命名文档 ----------
//
// 「未命名文档」就是一份**还没有磁盘路径**的内容。
//
// ★ 这里不发明什么新的数据结构，而是给它一个**假路径**：
//     untitled:Untitled-1
//   于是标签栏、model、脏状态、关闭流程**一行都不用改** ——
//   它们本来就是按「路径字符串」索引的，假路径和真路径对它们没区别。
//
// 路径里用 : 是为了和真实路径区分（Windows 是 D:/...，
// 不可能以 untitled: 开头）
const UNTITLED_PREFIX = "untitled:";

// 自增编号。
// 不用「找最小空缺」：关掉 Untitled-1 再新建的话，
// 标签栏里会出现两个内容不同、名字却一样的文件，很容易搞混
let untitledSeq = 0;

function isUntitledPath(path: string): boolean {
  return path.startsWith(UNTITLED_PREFIX);
}

function nextUntitledPath(): string {
  untitledSeq += 1;
  return `${UNTITLED_PREFIX}Untitled-${untitledSeq}`;
}

/**
 * 新建一个未命名文档。
 *
 * 它「生来就是脏的」：一份从来没写进磁盘的东西，
 * 哪怕内容是空的也不能算「已保存」——
 * 否则关窗口时就不会提醒你，而它确实会丢
 */
function newUntitledFile() {
  const path = nextUntitledPath();
  const model = createModelFor(path, "", "plaintext");

  // ★ 注意这里没有 savedVersions.set(path, ...)：
  //   没有基线，下面的订阅就会一直判定它为「脏」
  watchDirtyState(path, model);

  openTabs.value.push(path);
  activeTabPath.value = path;
  markDirty(path);
}

// ---------- 关闭脏标签 ----------

/**
 * 关闭脏标签时那三个按钮的文字。
 *
 * ★ 必须抽成常量：message() 用自定义按钮时，**返回值就是按钮文字本身**，
 *   所以下面要拿同一份字符串去比对。两处各写一遍，改一处就会静默失效。
 */
const DIRTY_CLOSE_LABELS = {
  save: "保存",
  discard: "不保存",
  cancel: "取消",
} as const;

/**
 * 有未保存改动时弹框问一句。
 *
 * 为什么必须是三选一：
 *   · 只有「保存 / 不保存」→ 点下去就没法反悔了，手滑就关掉了
 *   · 只有「确定 / 取消」→ 又没法决定到底要不要存
 * 三个出口刚好覆盖全部意图，所以系统的 YesNoCancel 正合适。
 */
async function askAboutDirty(path: string): Promise<"save" | "discard" | "cancel"> {
  const choice = await message(`是否保存对「${fileNameOf(path)}」的修改？`, {
    title: "未保存的修改",
    kind: "warning",
    buttons: {
      yes: DIRTY_CLOSE_LABELS.save,
      no: DIRTY_CLOSE_LABELS.discard,
      cancel: DIRTY_CLOSE_LABELS.cancel,
    },
  });

  if (choice === DIRTY_CLOSE_LABELS.save) return "save";
  if (choice === DIRTY_CLOSE_LABELS.discard) return "discard";
  // 剩下的就是「取消」，另外用户直接叉掉对话框也会落到这里 ——
  // 拿不准的时候，选那个不会丢数据的
  return "cancel";
}

/**
 * 放弃改动：把 model 的内容拉回磁盘上的版本。
 *
 * ★ 为什么不直接 model.dispose()：
 *   被关的标签很可能就是编辑器**当前显示**的那个，此时 model 正挂在编辑器上。
 *   销毁一个还挂着的 model，编辑器内部会留着一个「已销毁」的对象。
 *   而「让编辑器换掉 model」这件事由 watch(activeTabPath) 驱动，
 *   它是异步的 —— 我们在这里保证不了它已经换完。时序不可控。
 *
 *   改成「把内容改回磁盘版本」，model 自身始终有效，
 *   编辑器压根不需要换 model，一点时序风险都没有。
 *   代价只是多读一次盘，而这个代价换来的是确定性，很划算。
 */
async function discardChanges(path: string) {
  const model = models.get(path);
  if (!model) return;

  // 未命名文档没有「磁盘版本」可以回退，只能整个丢掉
  if (isUntitledPath(path)) {
    unwatchDirtyState(path);
    model.dispose();
    models.delete(path);
    savedVersions.delete(path);
    markClean(path);
    return;
  }

  try {
    const content = await invoke<string>("read_file", { path });
    model.setValue(content);

    // 和保存一样：把基线挪到当前状态，它才算「干净」
    savedVersions.set(path, model.getAlternativeVersionId());
    markClean(path);
  } catch {
    // 文件已经被删了之类 —— 读不到原内容，就退回「丢掉这个 model」
    model.dispose();
    models.delete(path);
    savedVersions.delete(path);
    markClean(path);
  }
}

/**
 * 关闭一个标签。
 * 如果关掉的正好是激活的那个，就切到「顶上来的那个」（同位置的）；
 * 没有就往前退一个 —— 和 VS Code 的行为一致。
 *
 * ★ 它现在是 async 的：有未保存改动时要先弹框。
 *   一旦有了 await，函数中间就成了「另一个世界」——
 *   await 期间用户完全可能干别的事（关文件夹、关别的标签），
 *   所以 **await 之后所有状态都当它可能变了，重新取一遍**，
 *   绝不能拿 await 之前算好的下标接着用。
 */
async function closeTab(path: string) {
  if (isDirty(path)) {
    const choice = await askAboutDirty(path);

    if (choice === "cancel") return;

    if (choice === "save") {
      // 存盘失败就别关 —— 强行关掉才是真的把数据弄丢了
      if (!(await saveFile(path))) return;
    } else {
      await discardChanges(path);
    }
  }

  // ↓ 以下全部在 await 之后：下标必须重新算
  const index = openTabs.value.indexOf(path);
  if (index === -1) return; // await 期间它可能已经被关掉了

  openTabs.value.splice(index, 1);

  if (activeTabPath.value === path) {
    activeTabPath.value = openTabs.value[index] ?? openTabs.value[index - 1] ?? null;
  }
}

/** 从路径取文件名，用于标签显示 */
function fileNameOf(path: string): string {
  // ★ 未命名文档的「路径」是假路径（untitled:Untitled-1），**里面没有 /**。
  //   直接 split("/").pop() 会把整个字符串原样返回 ——
  //   于是标签栏上会显示成 "untitled:Untitled-1" 这种鬼东西
  if (isUntitledPath(path)) return path.slice(UNTITLED_PREFIX.length);

  return path.split("/").pop() ?? path;
}

/** 压成相对工作区根的形式，用于状态栏显示（绝对路径太长） */
function shortPath(path: string): string {
  // 未命名文档没有磁盘位置，显示名字就够了
  if (isUntitledPath(path)) return fileNameOf(path);

  const root = workspaceRoot.value;
  if (!root) return path;

  const prefix = `${root}/`;
  return path.startsWith(prefix) ? path.slice(prefix.length) : path;
}

/** 侧栏标题：显示当前文件夹名，没打开时显示视图名 */
const sidebarTitle = computed(() =>
  workspaceRoot.value ? fileNameOf(workspaceRoot.value) : "资源管理器",
);

// provide 出去之后，任意深度的后代都能 inject 到，不需要逐层 emit 转发
provide(fileTreeSelectionKey, { activePath: activeTabPath, openFile });

const editorContainer = ref<HTMLElement | null>(null);

// 用 shallowRef 而不是 ref！这是关键。
// ref() 会把对象值交给 reactive() 包一层 Proxy，而 Monaco 实例内部大量依赖
// 对象同一性和 this 绑定 —— 被 Proxy 包住之后调用 setValue 等方法会把编辑器拖死。
// shallowRef 只跟踪「整个值被替换」，不做深层代理，正是这里需要的。
const editorInstance = shallowRef<monaco.editor.IStandaloneCodeEditor | null>(null);

// 状态栏显示的「光标位置」，由 Monaco 的订阅事件回填
const cursor = ref({ line: 1, column: 1 });

// 状态栏右下角那个「语言」。图片也在这里露个脸，
// 免得打开一张图片却显示成 plaintext 让人困惑
const currentLanguage = computed(() => {
  const path = activeTabPath.value;
  if (!path) return "plaintext";
  return isImagePath(path) ? "图片" : languageFromPath(path);
});

// Monaco 的事件订阅会返回一个 IDisposable，组件卸载时要手动释放，否则会内存泄漏
let cursorSubscription: monaco.IDisposable | null = null;
let fontSizeSubscription: monaco.IDisposable | null = null;

// ---------- 文档模型（model）管理 ----------
//
// Monaco 里「编辑器」和「文档」是两回事：
//   editor = 视图（谁在显示）
//   model  = 文档（文本 + 语言 + 撤销栈 + 光标/滚动位置）
//
// 一个文件对应一个 model。切文件时换 model，而不是换文本 ——
// 这样每个文件的撤销栈和视图状态都能各自保留下来。
const models = new Map<string, monaco.editor.ITextModel>();

// ---------- 「脏」状态 ----------
//
// 要判断一个文件有没有未保存的改动，就必须有一个「基线」来比。
// 基线 = 上次和磁盘一致时的状态。
//
// 为什么用「版本号」而不是把内容存一份来比字符串：
//   · model.getValue() 每次都要把整个文档拼成字符串，大文件很费
//   · Monaco 的 alternativeVersionId 在有改动时递增，
//     而且「改一下再撤销回去」会回到原来的值 ——
//     这正是我们要的语义：撤销回保存时的状态，就该自动变回「干净」
const savedVersions = new Map<string, number>();

// 有未保存改动的文件路径。
// 用数组而不是 Set：数量最多等于打开的标签数，includes 的开销可忽略；
// 而且整体替换能确保触发 Vue 更新，没有响应式上的坑
const dirtyPaths = ref<string[]>([]);

function markDirty(path: string) {
  // 已经是脏的就什么也没变，不用惊动备份
  if (dirtyPaths.value.includes(path)) return;
  dirtyPaths.value.push(path);
  scheduleHotExitBackup();
}

function markClean(path: string) {
  const next = dirtyPaths.value.filter((p) => p !== path);
  // 本来就不脏 → 没变化
  if (next.length === dirtyPaths.value.length) return;
  dirtyPaths.value = next;
  // ★ 这里也要排一次备份：保存 / 放弃改动都会走到这，
  //   不排的话备份里会留着这个文档，下次启动把它当成「未保存」又开出来
  scheduleHotExitBackup();
}

function isDirty(path: string): boolean {
  return dirtyPaths.value.includes(path);
}

// 每个 model 的「内容变化」订阅句柄。
//
// ★ 为什么要存起来：另存为会把 model 从一个 key 搬到另一个 key，
//   而订阅回调里捕获的是**旧路径** —— 不退掉重订的话，
//   它之后会一直去更新一个已经不存在的路径的脏状态
const contentSubscriptions = new Map<string, monaco.IDisposable>();

/** 给一个 model 挂上「内容一变就重判脏状态」的订阅 */
function watchDirtyState(path: string, model: monaco.editor.ITextModel) {
  contentSubscriptions.get(path)?.dispose();

  contentSubscriptions.set(
    path,
    model.onDidChangeContent(() => {
      // savedVersions 里没记录（比如未命名文档）时 get 返回 undefined，
      // 比较结果一定是 false → 判定为脏。这正是我们要的
      if (model.getAlternativeVersionId() === savedVersions.get(path)) {
        markClean(path);
      } else {
        markDirty(path);
      }
      // ★ 内容变了就排一次备份。
      //   不能只靠 markDirty —— 它已经是脏的时候会直接 return，
      //   而这时候**内容已经变了**，备份必须更新
      scheduleHotExitBackup();
    }),
  );
}

function unwatchDirtyState(path: string) {
  contentSubscriptions.get(path)?.dispose();
  contentSubscriptions.delete(path);
}

// ---------- hot exit（未保存内容恢复）----------
//
// 目标：Ctrl+R / 关掉应用再打开，未保存的内容还在。
//
// ★ 存在哪：localStorage。
//
// ★ 什么时候存：**内容一变就防抖备份**，而不是「退出时写一次」。
//   退出时写法看似干净，但**活不过崩溃和任务管理器强杀** ——
//   而那恰恰是最需要它的场景。VS Code 也是这个思路（定期备份到 backups 目录）。
//
// ⚠ 局限：localStorage 有容量上限（几 MB）且只能存字符串。
//   真要更稳得写文件（Tauri 的 appDataDir），那是另一套东西
const HOT_EXIT_KEY = "new_vscode:hotExit";

/** 备份里最多放几个文档、加起来多少字符 —— 不能无限往里塞 */
const HOT_EXIT_MAX_DOCS = 20;
const HOT_EXIT_MAX_CHARS = 500_000;

interface HotExitDoc {
  path: string;
  content: string;
}

interface HotExitSnapshot {
  active: string | null;
  docs: HotExitDoc[];
}

/**
 * 读备份。**读进来的一律先当它可能是坏的** ——
 * 手改过、写到一半断电、被别的版本写过，都可能让它不是我们要的形状
 */
function readHotExit(): HotExitSnapshot | null {
  const raw = window.localStorage.getItem(HOT_EXIT_KEY);
  if (!raw) return null;

  try {
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== "object" || parsed === null) return null;

    const snapshot = parsed as Partial<HotExitSnapshot>;
    if (!Array.isArray(snapshot.docs)) return null;

    const docs = snapshot.docs.filter(
      (doc): doc is HotExitDoc =>
        typeof doc === "object" &&
        doc !== null &&
        typeof (doc as HotExitDoc).path === "string" &&
        typeof (doc as HotExitDoc).content === "string",
    );

    return {
      active: typeof snapshot.active === "string" ? snapshot.active : null,
      docs,
    };
  } catch {
    return null;
  }
}

/** 把当前所有「脏」文档收集起来 */
function collectDirtyDocs(): HotExitDoc[] {
  const docs: HotExitDoc[] = [];
  let chars = 0;

  for (const path of dirtyPaths.value) {
    const model = models.get(path);
    if (!model) continue;

    const content = model.getValue();
    // 太大的不进备份：为了一个超大文件把整个备份写崩（配额报错）不划算
    if (chars + content.length > HOT_EXIT_MAX_CHARS) continue;

    chars += content.length;
    docs.push({ path, content });
    if (docs.length >= HOT_EXIT_MAX_DOCS) break;
  }

  return docs;
}

/** 备份当前所有脏文档。没脏的就直接把备份删掉 */
function writeHotExit() {
  const docs = collectDirtyDocs();

  try {
    if (docs.length === 0) {
      window.localStorage.removeItem(HOT_EXIT_KEY);
      return;
    }
    const snapshot: HotExitSnapshot = { active: activeTabPath.value, docs };
    window.localStorage.setItem(HOT_EXIT_KEY, JSON.stringify(snapshot));
  } catch {
    // 写不进去（配额满了、隐私模式）就算了。
    // ★ hot exit 是「锦上添花」，绝不能因为它把编辑本身搞崩
  }
}

// 防抖定时器：连续敲键盘时，不要每敲一下就序列化一遍
let hotExitTimer: ReturnType<typeof setTimeout> | null = null;

function scheduleHotExitBackup() {
  if (hotExitTimer) clearTimeout(hotExitTimer);
  hotExitTimer = setTimeout(() => {
    hotExitTimer = null;
    writeHotExit();
  }, 600);
}

/**
 * 启动时把上次没存完的内容接回来。
 *
 * 恢复出来的文档**一律标成脏**：它们的内容还没落盘，
 * 必须让关窗口时的确认框认得它们
 */
function restoreHotExit() {
  const snapshot = readHotExit();
  if (!snapshot) return;

  for (const doc of snapshot.docs) {
    // 模型或者标签已经在了就不重复建（正常流程不会走到，防御一下）
    if (openTabs.value.includes(doc.path)) continue;

    const model = createModelFor(doc.path, doc.content, languageFromPath(doc.path));
    watchDirtyState(doc.path, model);
    openTabs.value.push(doc.path);
    markDirty(doc.path);

    // 未命名文档要把编号也「续上」——
    // 不续的话再点「新建文件」会又叫出 Untitled-1，和恢复出来的撞名
    if (isUntitledPath(doc.path)) {
      const matched = /^untitled:Untitled-(\d+)$/.exec(doc.path);
      if (matched) untitledSeq = Math.max(untitledSeq, Number(matched[1]));
    }
  }

  // 回到上次看的那一个（没了就退而求其次开第一个）
  const active = snapshot.active;
  if (active && openTabs.value.includes(active)) {
    activeTabPath.value = active;
  } else if (openTabs.value.length > 0) {
    activeTabPath.value = openTabs.value[0];
  }
}

// 没选中文件时编辑器要挂一个 model（Monaco 不能没有 model 还正常工作），
// 所以用一个不会和真实路径冲突的 key 存进同一个 Map，
// 这样它也能被统一释放，不会变成没人管的孤儿 model。
//
// ⚠ 真正显示给用户看的是 DOM 画的那层欢迎页（模板里的 .welcome），
//   这个 model 只是垫在下面、用户看不到 —— 所以内容随便写点提示就行
const WELCOME_KEY = ":welcome:";
const WELCOME_TEXT = "// 点左侧文件树里的文件试试";

// 欢迎页右栏的快捷键表。
// 用数据 + v-for 生成，而不是手写五行 HTML ——
// 以后加快捷键只要往数组里追一项，不用碰模板
const WELCOME_SHORTCUTS = [
  { keys: ["Ctrl", "S"], label: "保存当前文件" },
  { keys: ["Ctrl", "滚轮"], label: "缩放编辑器字号" },
  { keys: ["Ctrl", "="], label: "放大字号" },
  { keys: ["Ctrl", "-"], label: "缩小字号" },
  { keys: ["Ctrl", "0"], label: "重置字号" },
] as const;

function createModelFor(key: string, content: string, language: string): monaco.editor.ITextModel {
  const existing = models.get(key);
  if (existing) return existing;

  // 语言在创建时一次定好，以后不用再调 setModelLanguage
  const model = monaco.editor.createModel(content, language);
  models.set(key, model);
  return model;
}

/**
 * 取一个文件的 model，没有就去磁盘读。
 *
 * 注意这是 async —— 内容不再存在前端内存里，而是按需从磁盘拿。
 * 所以之前那个 findNodeByPath（在内存树里找节点拿 content）已经不需要了：
 * 路径本身就是足够的标识，不需要先找到节点。
 *
 * 这也是为什么 FileNode 上的 content 字段被删掉了 —— 内容的真相在磁盘 + model，
 * 前端再存一份就是「两份真相」，迟早不一致。
 */
async function modelForPath(path: string): Promise<monaco.editor.ITextModel> {
  const existing = models.get(path);
  if (existing) return existing;

  const content = await invoke<string>("read_file", { path });
  const model = createModelFor(path, content, languageFromPath(path));

  // 刚从磁盘读回来，内容和磁盘一致 → 这就是最初的「干净」基线
  savedVersions.set(path, model.getAlternativeVersionId());
  markClean(path);

  // 内容一变就重新判断脏状态（订阅细节见 watchDirtyState）
  watchDirtyState(path, model);

  return model;
}

/** 读文件失败时，用一个临时 model 把原因显示在编辑器里 */
function errorModelFor(path: string, reason: unknown): monaco.editor.ITextModel {
  const key = `:error:${path}`;
  // 同一个文件的失败原因可能每次不同，所以先丢掉旧的再建
  models.get(key)?.dispose();
  models.delete(key);

  // 每一行都补上注释前缀 —— 错误信息里可能带换行，
  // 只给第一行加的话，后面几行会变成「看着像代码但不是代码」的东西
  const detail = String(reason)
    .split("\n")
    .map((line) => `// ${line}`)
    .join("\n");

  return createModelFor(key, `// 无法打开：${path}\n${detail}`, "plaintext");
}

// 扩展名 → Monaco 的语言 id。Monaco 不会自己猜语言，必须显式告诉它。
// 注意：右边的名字必须是 Monaco 已注册的语言 id，否则会静默退化成 plaintext
// （编辑器不会报错，只是不上色 —— 所以打错了很难发现）
//
// ★ 这张表还有第二个身份：**插件语法用不了时的退路**。
//   比如 `vue: "html"` —— Monaco 没有 vue 语言，先拿 html 顶着；
//   而万一 Volar 的 Vue 语法缺依赖，这一条就变成「别把 .vue 交给插件」的理由。
//   插件会覆盖它（插件更权威），但覆盖之前它得是一份**能独立工作**的完整表
const LANGUAGE_BY_EXT: Record<string, string> = {
  // 前端
  ts: "typescript",
  tsx: "typescript",
  js: "javascript",
  jsx: "javascript",
  mjs: "javascript",
  cjs: "javascript",
  vue: "html", // Monaco 没有 vue 语言，用 html 代替（至少标签能上色）
  html: "html",
  htm: "html",
  css: "css",
  scss: "scss",
  less: "less",

  // 数据 / 配置
  json: "json",
  yml: "yaml",
  yaml: "yaml",
  ini: "ini",
  toml: "ini", // Monaco 没有 toml，ini 是最接近的（其实它俩语法差挺多）
  xml: "xml",
  svg: "xml", // SVG 本质就是 XML，当 XML 看正好

  // 其它常用
  md: "markdown",
  markdown: "markdown",
  rs: "rust",
  py: "python",
  sh: "shell",
  bash: "shell",
  ps1: "powershell",
  sql: "sql",
  java: "java",
  go: "go",
  rb: "ruby",
  php: "php",
  lua: "lua",
  c: "c",
  h: "c",
  cpp: "cpp",
  hpp: "cpp",
  cs: "csharp",
};

/**
 * 取扩展名（小写、不含点）。
 *
 * 只看文件名部分，避免目录名里带点造成误判（比如 "a.b/main.ts"）。
 * 抽成独立函数是因为「按扩展名判断文件类型」不止语言一处要用 ——
 * 图片预览也要。两边各写一份，迟早会走偏
 */
function extOf(path: string): string {
  const fileName = path.split("/").pop() ?? "";
  const dot = fileName.lastIndexOf(".");
  return dot === -1 ? "" : fileName.slice(dot + 1).toLowerCase();
}

function languageFromPath(path: string): string {
  return LANGUAGE_BY_EXT[extOf(path)] ?? "plaintext";
}

/**
 * 能直接在 <img> 里显示的扩展名 → MIME 类型。
 *
 * ★ 故意**没有 svg**：SVG 本质是 XML 文本，VS Code 默认也当文本打开
 *   （能编辑、能语法高亮）。我们跟它保持一致，不抢文本路线的活。
 *
 * ★ icns 映射到 image/png：浏览器不认 icns，但 Rust 那边的
 *   read_preview_bytes 会把 icns 里内嵌的 PNG 抠出来再返回，
 *   所以到前端手里时它已经是一张 PNG 了。
 */
const IMAGE_MIME_BY_EXT: Record<string, string> = {
  png: "image/png",
  jpg: "image/jpeg",
  jpeg: "image/jpeg",
  jfif: "image/jpeg",
  gif: "image/gif",
  webp: "image/webp",
  bmp: "image/bmp",
  ico: "image/x-icon",
  avif: "image/avif",
  icns: "image/png", // 见上面的说明：Rust 会把内嵌 PNG 抠出来
};

/** 是图片就返回它的 MIME，不是就返回 null */
function imageMimeOf(path: string): string | null {
  return IMAGE_MIME_BY_EXT[extOf(path)] ?? null;
}

function isImagePath(path: string): boolean {
  return imageMimeOf(path) !== null;
}

// ---------- 语法扩展（插件机制的第一块）----------

interface GrammarEntry {
  /** 挂到哪个 Monaco 语言上。null = 注入语法，只给别的语法当 include 源 */
  id: string | null;
  scopeName: string;
  path: string;
  extensions: string[];
}

/**
 * 启动时扫一遍本机的 VS Code 扩展，把它们贡献的 TextMate 语法登记上来。
 *
 * ★ 分两步，而且只有第一步在启动路径上：
 *   1) 这里 —— 只登记「哪些语言、叫什么名字、管哪些扩展名」，很快（一次 IPC）
 *   2) 用户真的打开这种文件时 —— 才去读语法文件并编译它（在 textmate.ts 里）
 *   一百多个语法共几 MB，启动时全读进来编译一遍会让启动明显变慢，还白占内存
 *
 * ★ 「本机的扩展」不只有用户装的那些：Rust 侧还会去扫 **VS Code 程序目录**里的内置扩展。
 *   这步不能省 —— html / css / typescript / json / markdown 这些**父语法**只在那里，
 *   而 Vue 这类语法几乎全靠 include 它们。只扫用户目录的话，
 *   .vue 里 html 和表达式那部分一个 token 都分不出来（就只剩标签名有颜色了）
 *
 * ★ 失败一律吞掉：没装扩展、某个语法文件坏了 ——
 *   这些都只意味着「这个语言没高亮」，绝不能让编辑器本身用不了
 *
 * ★ 扫不到内置扩展时要**降级**（见下面 hasCoreGrammars 那段）：
 *   拿不到最好的，就退回够用的那个 —— 而不是让一个残缺的语法把有用的高亮挤掉
 */
async function loadSyntaxExtensions() {
  try {
    // 只扫元数据：语言 id / scopeName / 文件路径 / 管哪些扩展名
    const entries = await invoke<GrammarEntry[]>("scan_grammar_extensions");

    // scopeName → 语法文件路径。
    // ★ 这一遍**必须包含所有语法**（包括那些没写 language 的「注入语法」），
    //   因为 include 是跳文件的（Vue 会要 html / typescript / json 的语法），
    //   少一个，用到它的那个语法就缺一块
    const pathByScope = new Map<string, string>();
    for (const entry of entries) pathByScope.set(entry.scopeName, entry.path);

    // 交给引擎的「按 scopeName 取语法」回调。
    // ★ 它是在**用户第一次打开这种文件时**才被调用的，不是现在
    const loadSource = async (scopeName: string) => {
      const path = pathByScope.get(scopeName);
      if (!path) return null;

      try {
        // 文件名要原样带上：parseRawGrammar 靠后缀决定用 JSON 还是 plist 解析器
        return { text: await invoke<string>("read_file", { path }), fileName: path };
      } catch {
        // 单个语法读不了（文件被删、不是 UTF-8）只影响那一个语言
        return null;
      }
    };

    // ★ 「VS Code 程序目录里的内置扩展，到底扫到没有？」
    //
    //   这几个 scopeName 只存在于程序目录的扩展里，而且它们几乎是所有
    //   「寄生语法」的公共依赖 —— Vue / Svelte / Astro 这类语法自己只描述「壳」，
    //   html / css / 表达式那部分全靠 include 它们。
    //
    //   ★ 为什么用这个当判据，而不是逐个语法去查它的 include 依赖：
    //     Vue 语法还 include 了 source.pug / source.coffee / source.graphql /
    //     source.sass / source.stylus / source.postcss / source.json5 …
    //     —— 这些**内置扩展里也没有**。按「依赖必须全齐」来判断，
    //     .vue 会被永远误判成「依赖不全」而白白降级。
    //     判「基础语法库到手没有」才是真正要问的问题
    const CORE_GRAMMAR_SCOPES = ["text.html.basic", "source.css", "source.ts", "source.json"];
    const hasCoreGrammars = CORE_GRAMMAR_SCOPES.some((scope) => pathByScope.has(scope));

    if (!hasCoreGrammars) {
      console.warn(
        "[语法扩展] 没扫到 VS Code 程序目录里的内置扩展 —— html / css / typescript 这些" +
          "父语法拿不到，Vue 这类语法只会剩下标签名有颜色。更差的是：它照样能分词，" +
          "于是会把本来有用的高亮挤掉。所以凡是手写表里有退路的语言都退回手写表"
      );
      // 光在控制台说一声不够 —— 不然用户只会发现「.vue 怎么变成 html 了」而不知道原因
      flashSaveNotice("没找到 VS Code 内置语法，.vue 等已退回内置分词器");
    }

    let takenOver = 0;

    for (const entry of entries) {
      // 没写 language 的语法不挂到任何语言上 ——
      // 它的路径已经进了 pathByScope，别的语法 include 它时找得到，这就够了
      if (!entry.id) continue;

      // ★ Monaco 自带的语言就让它用自带的。
      //   它那套分词器和自己的 worker / 语义高亮 / 语言配置是配套的，已经能用；
      //   而它的语法文件依然在 pathByScope 里，照样能给别的语法当 include 源
      //   —— 「能不能当 include 源」和「谁来当主分词器」是两件事，别混一起
      const handled = monaco.languages.getLanguages().some((item) => item.id === entry.id);
      if (handled) continue;

      // ★ 失败降级：基础父语法拿不到时，「有退路的语言」就别接管。
      //
      //   判据是「这个扩展名在手写表里已经有映射」—— 那就说明它有条凑合能用的退路
      //   （`.vue` → `html`）。Vue 语法在这儿虽然也能分词，但吐出来的语义缺一大块，
      //   而 html 分词器至少能把标签、属性、内嵌的 CSS/JS 都认出来 —— 明显更好。
      //
      //   手写表里**没有**的扩展名照常接管：反正原本就是纯文本，接管了只会更好
      if (
        !hasCoreGrammars &&
        entry.extensions.some((ext) => LANGUAGE_BY_EXT[ext.replace(/^\./, "").toLowerCase()])
      ) {
        continue;
      }

      // 让这个语法管到它声明的那些扩展名。
      // ★ 这里是**覆盖** LANGUAGE_BY_EXT —— 插件比我们手写的那份更权威。
      //   典型例子：vue 原来只能「假扮成 html」，现在能真上 Vue 语法了
      for (const ext of entry.extensions) {
        LANGUAGE_BY_EXT[ext.replace(/^\./, "").toLowerCase()] = entry.id;
      }

      registerTextMateLanguage({
        languageId: entry.id,
        scopeName: entry.scopeName,
        loadSource,
      });
      takenOver += 1;
    }

    console.info(`[语法扩展] 扫到 ${entries.length} 个语法，接管了 ${takenOver} 种语言（按需加载）`);
  } catch (error) {
    console.warn("[语法扩展] 加载失败（不影响其它功能）：", error);
  }
}

// ---------- 保存 ----------

// 保存结果提示（成功或失败），用 setTimeout 让它自动消失
const saveNotice = ref<string | null>(null);
// setTimeout 返回的定时器句柄 —— 和 Monaco 的 IDisposable 一样，
// 它是需要手动释放的「外部资源」
let saveNoticeTimer: ReturnType<typeof setTimeout> | null = null;

function flashSaveNotice(message: string) {
  saveNotice.value = message;

  // 上一条提示可能还在倒计时，先取消 —— 否则它会把新提示提前清掉
  if (saveNoticeTimer) clearTimeout(saveNoticeTimer);

  saveNoticeTimer = setTimeout(() => {
    saveNotice.value = null;
    saveNoticeTimer = null;
  }, 2000);
}

/**
 * 把指定文件的内容写回磁盘。返回「是否真的存成功了」。
 *
 * ★ 为什么要返回值：关闭标签时要靠它决定能不能关。
 *   存盘失败还硬把标签关掉，那才是真的丢数据。
 */
async function saveFile(path: string): Promise<boolean> {
  // 未命名文档没有磁盘路径，「保存」对它来说只能是「另存为」
  if (isUntitledPath(path)) return saveFileAs(path);

  const model = models.get(path);
  if (!model) {
    // 比如读取失败时编辑器里显示的是错误提示 model，那种情况不该"保存"
    flashSaveNotice("文件尚未加载，无法保存");
    return false;
  }

  // 本来就是干净的 → 没有要写的，当作成功
  if (!isDirty(path)) return true;

  try {
    await invoke("write_file", { path, content: model.getValue() });

    // ★ 「保存」这个动作的本质：把基线挪到当前状态。
    // 不挪基线的话，这个文件会永远显示为「脏」
    savedVersions.set(path, model.getAlternativeVersionId());
    markClean(path);

    flashSaveNotice(`已保存 ${fileNameOf(path)}`);
    return true;
  } catch (error) {
    flashSaveNotice(`保存失败：${error}`);
    return false;
  }
}

/** 保存当前激活的文件（Ctrl+S 和菜单里的「保存」都走这条） */
async function saveActiveFile() {
  const path = activeTabPath.value;
  if (!path) return;

  // 干净的文件不用白写一遍盘，给个提示就够了。
  // （未命名文档永远不干净，所以它会直接落到下面去走另存为）
  if (models.has(path) && !isDirty(path)) {
    flashSaveNotice(`${fileNameOf(path)} 没有改动`);
    return;
  }

  await saveFile(path);
}

/**
 * 另存为。未命名文档的「保存」也会走到这里。
 *
 * ★ 真正的难点不是写文件，而是**搬家**：
 *   model / 基线 / 脏状态 / 标签**全都是按路径当 key 的**。
 *   存完之后路径变了，这四样东西必须**整套搬过去** ——
 *   少搬一样就会出现「同一个文档有两条记录」，而且不会报错。
 */
async function saveFileAs(path: string): Promise<boolean> {
  const model = models.get(path);
  if (!model) return false;

  const target = await saveDialog({
    title: "另存为",
    // 未命名文档只给个名字（让用户自己选目录）；
    // 有路径的就给完整路径，对话框会直接开在它所在的那个目录
    defaultPath: isUntitledPath(path) ? fileNameOf(path) : path,
  });
  // 用户取消时返回 null
  if (typeof target !== "string") return false;
  // 存回原位置没有任何意义，当取消处理（不报错、也不提示成功）
  if (target === path) return true;

  try {
    await invoke("write_file", { path: target, content: model.getValue() });
  } catch (error) {
    flashSaveNotice(`保存失败：${error}`);
    return false;
  }

  // ---- 搬家：把 model 从旧 key 挪到新 key ----
  models.delete(path);
  models.set(target, model);

  // 内容刚落盘，所以新路径的基线就是「现在」
  savedVersions.delete(path);
  savedVersions.set(target, model.getAlternativeVersionId());

  // ★ 顺序不能反：先退订旧路径，再按新路径重订。
  //   不退的话那个回调会一直往旧路径上写脏状态
  unwatchDirtyState(path);
  watchDirtyState(target, model);

  markClean(path);
  markClean(target);

  // 文件名变了，语言也可能要跟着变 ——
  // 存成 .ts 却不给 TypeScript 高亮就太怪了
  monaco.editor.setModelLanguage(model, languageFromPath(target));

  const index = openTabs.value.indexOf(path);
  if (index !== -1) openTabs.value.splice(index, 1, target);
  if (activeTabPath.value === path) activeTabPath.value = target;

  flashSaveNotice(`已保存 ${fileNameOf(target)}`);
  return true;
}

/**
 * 全局快捷键。绑在 window 上而不是 Monaco 上 ——
 * 这样焦点在侧栏或标签栏时也照样生效。
 */
function handleKeydown(event: KeyboardEvent) {
  // Esc 收起菜单
  if (event.key === "Escape") {
    closeMenu();
    return;
  }

  // 下面全是 Ctrl / Cmd 组合键
  if (!(event.ctrlKey || event.metaKey)) return;

  // Ctrl+S 保存 / Ctrl+Shift+S 另存为。
  // 必须阻止默认行为，否则 WebView 会弹出「保存网页」对话框
  if (event.key.toLowerCase() === "s") {
    event.preventDefault();
    const path = activeTabPath.value;
    if (!path) return;
    if (event.shiftKey) {
      void saveFileAs(path);
    } else {
      void saveActiveFile();
    }
    return;
  }

  // Ctrl+N 新建未命名文档。
  // 同样要 preventDefault —— 否则 WebView 会开一个新窗口
  if (event.key.toLowerCase() === "n") {
    event.preventDefault();
    newUntitledFile();
    return;
  }

  // Ctrl+= / Ctrl++ 放大，Ctrl+- 缩小，Ctrl+0 恢复默认。
  //
  // ★ "=" 和 "+" 都要认：不按 Shift 时 e.key 是 "="，
  //   按住 Shift 打出加号时才是 "+"。
  //   用户想放大时最自然的按法就是 Shift+=，只认 "=" 的话它反而没反应
  //
  // ★ 这些键都必须 preventDefault：
  //   WebView2 自带「Ctrl+=/- 缩放页面」，不管的话就会缩放两次
  if (event.key === "=" || event.key === "+") {
    event.preventDefault();
    stepFontSize(1);
    return;
  }

  if (event.key === "-") {
    event.preventDefault();
    stepFontSize(-1);
    return;
  }

  if (event.key === "0") {
    event.preventDefault();
    resetFontSize();
  }
}

// ---------- 工作区（文件夹）----------

async function loadTree() {
  const root = workspaceRoot.value;
  if (!root) return;

  treeError.value = null;
  try {
    fileTree.value = await invoke<FileNode[]>("read_dir", { path: root });
  } catch (error) {
    fileTree.value = [];
    treeError.value = String(error);
  }
}

/**
 * 切换工作区前，把上一个工作区的状态清干净。
 *
 * 为什么必须清：标签、model、脏状态**都是以绝对路径为 key** 的。
 * 换了文件夹之后旧路径全部失效，不清就会剩一堆指向不存在文件的标签。
 * VS Code 打开新文件夹时也是这个行为（概念上等于开了一个新工作区）。
 */
function resetWorkspace() {
  // ★ 未命名文档不属于任何工作区，切文件夹不该把它们清掉。
  //   当普通标签一起删的话，用户会觉得「新建的文件莫名其妙没了」
  openTabs.value = openTabs.value.filter(isUntitledPath);
  activeTabPath.value = openTabs.value[0] ?? null;

  // 释放上一批文件文档。
  // 跳过欢迎页那个 model（编辑器正用着它），也跳过未命名文档
  for (const [key, model] of models) {
    if (key === WELCOME_KEY || isUntitledPath(key)) continue;
    unwatchDirtyState(key);
    model.dispose();
    models.delete(key);
  }

  savedVersions.clear();
  // 脏状态同理：只保留未命名文档的
  dirtyPaths.value = dirtyPaths.value.filter(isUntitledPath);
}

/** 还没保存的文件（按打开顺序） */
function unsavedFiles(): string[] {
  return openTabs.value.filter((path) => isDirty(path));
}

/**
 * 「有未保存改动」那个对话框的按钮文字。
 * 和 DIRTY_CLOSE_LABELS 同理：用自定义按钮时 message() 返回的就是按钮文字本身，
 * 所以比对必须用同一份字符串
 */
const UNSAVED_LABELS = {
  saveAll: "全部保存",
  discard: "不保存",
  cancel: "取消",
} as const;

/** 把一串路径压成「a.ts、b.ts 等 N 个文件」，免得对话框被文件名撑爆 */
function describeFiles(paths: string[]): string {
  const names = paths.map(fileNameOf);
  return names.length > 5
    ? `${names.slice(0, 5).join("、")} 等 ${names.length} 个文件`
    : names.join("、");
}

/**
 * 有未保存的文件时，问一句「这些改动怎么办」。
 *
 * 三处破坏性操作共用它：关闭工作区、切换工作区、退出应用 ——
 * 它们的语义完全一样，只是 dialogTitle 不同（好让用户知道自己在干什么）。
 * 返回 "cancel" 就是用户反悔了，调用方别继续。
 *
 * 为什么必须有：resetWorkspace() 会把所有 model 直接 dispose 掉，
 * 未保存的改动连「撤销」的机会都没有。这比关单个标签更粗暴，
 * 更不能静默执行。
 */
async function askAboutUnsaved(dialogTitle: string): Promise<"saved" | "discard" | "cancel"> {
  const dirty = unsavedFiles();
  // 没有要保存的 → 直接放行，不弹框打扰
  if (dirty.length === 0) return "saved";

  const choice = await message(`${describeFiles(dirty)} 有未保存的修改。`, {
    title: dialogTitle,
    kind: "warning",
    buttons: {
      yes: UNSAVED_LABELS.saveAll,
      no: UNSAVED_LABELS.discard,
      cancel: UNSAVED_LABELS.cancel,
    },
  });

  if (choice === UNSAVED_LABELS.saveAll) {
    // 逐个保存。★ 只要有一个失败就中止整个流程 ——
    // 存不进去还硬往下走（关窗口 / 换文件夹），那才是真丢数据
    for (const path of dirty) {
      if (!(await saveFile(path))) return "cancel";
    }
    return "saved";
  }

  if (choice === UNSAVED_LABELS.discard) return "discard";

  // 剩下的就是「取消」，用户直接叉掉对话框也落在这里 ——
  // 拿不准的时候，选那个不会丢数据的
  return "cancel";
}

async function openFolder() {
  // 用户取消时返回 null，所以要判断类型
  const selected = await openDialog({ directory: true, multiple: false });
  if (typeof selected !== "string") return;

  // ★ 问的时机：选完文件夹之后、动手清理之前。
  //   选文件夹本身是无害的只读操作，没必要提前打断；
  //   而 resetWorkspace() 一旦跑起来就晚了 —— 内容已经销毁。
  if ((await askAboutUnsaved("切换工作区")) === "cancel") return;

  resetWorkspace();
  await openFolderPath(selected);
}

/**
 * 打开一个「已知路径」的文件夹（不走文件对话框）。
 * 启动时恢复、点欢迎页的最近列表，都走它。
 *
 * ★ 打不开就把它从最近列表里剔掉：
 *   一个已经不存在（或读不了）的路径留在那儿，下次还会再骗你一次。
 *   踢掉之后回到「没有打开文件夹」的状态，让用户重新选。
 */
async function openFolderPath(path: string) {
  workspaceRoot.value = path;
  await loadTree();

  if (treeError.value) {
    forgetFolder(path);
    window.localStorage.removeItem(LAST_FOLDER_KEY);

    // 清掉 workspaceRoot 而不是留着一个死路径 ——
    // 侧栏会回到「还没有打开文件夹」，比挂着一屏红色报错有用
    workspaceRoot.value = null;
    fileTree.value = [];
    treeError.value = null;
    flashSaveNotice(`打不开 ${fileNameOf(path)}，已从最近列表移除`);
    return;
  }

  window.localStorage.setItem(LAST_FOLDER_KEY, path);
  rememberFolder(path);
}

/** 点欢迎页「最近」里的文件夹 */
async function openRecentFolder(path: string) {
  if ((await askAboutUnsaved("切换工作区")) === "cancel") return;

  resetWorkspace();
  await openFolderPath(path);
}

/** 关闭当前工作区 */
async function closeFolder() {
  if ((await askAboutUnsaved("关闭工作区")) === "cancel") return;

  resetWorkspace();
  workspaceRoot.value = null;
  fileTree.value = [];
  treeError.value = null;

  // ★ 只清「下次启动恢复哪个」，让「关闭」持久生效。
  //   **不**把它从 recentFolders 里删掉 ——
  //   「关闭文件夹」的意思是「我现在不想看它」，不是「我以后也不想看它」。
  //   它还应该躺在欢迎页的最近列表里，随时能点回来
  window.localStorage.removeItem(LAST_FOLDER_KEY);
}

/**
 * 用系统默认程序打开当前文件。
 *
 * 二进制文件（png / ico / pdf / zip）我们用编辑器打不开 ——
 * read_file 会因为不是合法 UTF-8 而失败。交给系统处理是正解。
 */
async function openActiveWithSystem() {
  const path = activeTabPath.value;
  if (!path) {
    flashSaveNotice("没有打开的文件");
    return;
  }

  try {
    await openPath(path);
  } catch (error) {
    flashSaveNotice(`打开失败：${error}`);
  }
}

/** 在系统文件资源管理器里定位这个文件 */
async function revealActiveInExplorer() {
  const path = activeTabPath.value;
  if (!path) {
    flashSaveNotice("没有打开的文件");
    return;
  }

  try {
    await revealItemInDir(path);
  } catch (error) {
    flashSaveNotice(`定位失败：${error}`);
  }
}

// ---------- 菜单栏 ----------

interface MenuItem {
  /** 分隔线 */
  separator?: boolean;
  label?: string;
  shortcut?: string;
  run?: () => void;
}

interface Menu {
  label: string;
  items: MenuItem[];
}

// 当前展开的菜单标题；null 表示全部收起
const openMenu = ref<string | null>(null);

function toggleMenu(label: string) {
  openMenu.value = openMenu.value === label ? null : label;
}

/**
 * 已经展开了某个菜单时，鼠标移到别的标题上直接切换 —— 这是桌面菜单的惯例。
 * 但本来没展开时悬停不该打开，否则鼠标扫过菜单栏就会乱开一堆。
 */
function hoverMenu(label: string) {
  if (openMenu.value !== null) openMenu.value = label;
}

function closeMenu() {
  openMenu.value = null;
}

/** 执行菜单项：先收起菜单，再执行 —— 桌面应用的惯例 */
function runMenuItem(item: MenuItem) {
  closeMenu();
  item.run?.();
}

/**
 * 调用 Monaco 的内置 action。
 * Monaco 自带一大堆编辑命令（撤销、查找、切换注释…），不用自己实现。
 */
function runEditorAction(actionId: string) {
  const editor = editorInstance.value;
  if (!editor) return;

  // getAction 返回 null 表示这个 action 当前不可用（比如没打开任何文件）
  editor.getAction(actionId)?.run();
}

// 菜单用 computed：有的标题要跟着状态变（比如主题名、侧栏当前是显示还是隐藏）
const menus = computed<Menu[]>(() => [
  {
    label: "文件",
    items: [
      { label: "新建文件", shortcut: "Ctrl+N", run: newUntitledFile },
      { separator: true },
      { label: "打开文件夹…", run: () => void openFolder() },
      { label: "关闭文件夹", run: () => void closeFolder() },
      { separator: true },
      { label: "保存", shortcut: "Ctrl+S", run: () => void saveActiveFile() },
      {
        label: "另存为…",
        shortcut: "Ctrl+Shift+S",
        run: () => {
          const path = activeTabPath.value;
          if (path) void saveFileAs(path);
        },
      },
      {
        label: "关闭编辑器",
        run: () => {
          const path = activeTabPath.value;
          if (path) void closeTab(path);
        },
      },
      { separator: true },
      {
        label: "用系统默认程序打开",
        run: () => void openActiveWithSystem(),
      },
      {
        label: "在文件资源管理器中显示",
        run: () => void revealActiveInExplorer(),
      },
      { separator: true },
      { label: "退出", run: () => void quitApp() },
    ],
  },
  {
    label: "编辑",
    items: [
      { label: "撤销", shortcut: "Ctrl+Z", run: () => runEditorAction("undo") },
      { label: "重做", shortcut: "Ctrl+Y", run: () => runEditorAction("redo") },
      { separator: true },
      { label: "剪切", shortcut: "Ctrl+X", run: () => runEditorAction("editor.action.clipboardCutAction") },
      { label: "复制", shortcut: "Ctrl+C", run: () => runEditorAction("editor.action.clipboardCopyAction") },
      { label: "粘贴", shortcut: "Ctrl+V", run: () => runEditorAction("editor.action.clipboardPasteAction") },
      { separator: true },
      { label: "查找", shortcut: "Ctrl+F", run: () => runEditorAction("actions.find") },
      { label: "替换", shortcut: "Ctrl+H", run: () => runEditorAction("editor.action.startFindReplaceAction") },
      { separator: true },
      { label: "切换行注释", shortcut: "Ctrl+/", run: () => runEditorAction("editor.action.commentLine") },
    ],
  },
  {
    label: "查看",
    items: [
      { label: `切换主题（当前：${currentThemeLabel.value}）`, run: cycleTheme },
      { separator: true },
      { label: sidebarVisible.value ? "隐藏侧栏" : "显示侧栏", run: toggleSidebar },
      { separator: true },
      { label: `重置编辑器字号（当前 ${currentFontSize.value}px）`, run: resetFontSize },
    ],
  },
  {
    label: "帮助",
    items: [
      {
        label: "关于 new_vscode",
        run: () => flashSaveNotice("new_vscode · Tauri 2 + Vue 3 + Monaco"),
      },
    ],
  },
]);

// 「关闭窗口」事件的退订函数。onCloseRequested 是异步注册的，所以要存起来
let unlistenClose: (() => void) | null = null;

/**
 * 退出应用（菜单里的「文件 → 退出」）。
 *
 * ★ 为什么不直接 destroy() 了事：
 *   那样「点标题栏 ×」会问你，「菜单里的退出」却不问 —— 用户会以为菜单退出是安全的，
 *   结果一点就丢了改动。**同一个动作不能有两个不同的严格程度。**
 *   所以这里和 × 走的是同一个对话框助手 askAboutUnsaved()，标题也一样。
 *
 * ★ 为什么用 destroy() 而不是 close()：
 *   close() 会再触发一次 closeRequested，于是又进那个处理函数、再问一遍 ——
 *   用户刚答完又弹一次框。destroy() 是「不打招呼地关」，一步到位。
 *   （反过来，这也是为什么没让菜单去调 close() 复用那个处理函数：
 *     直接 destroy 不依赖事件监听器是否注册成功，菜单点了就一定有反应）
 */
async function quitApp() {
  if ((await askAboutUnsaved("退出前确认")) === "cancel") return;
  await getCurrentWindow().destroy();
}

onMounted(async () => {
  if (!editorContainer.value) return;//安全检查，如果容器还没准备好就退出

  window.addEventListener("keydown", handleKeydown);

  // 点页面别处就收起菜单。
  // 菜单栏本身加了 @click.stop，所以点菜单不会冒泡到这里
  window.addEventListener("click", closeMenu);

  editorInstance.value = monaco.editor.create(editorContainer.value, {
    // 传 model 而不是 value/language：文档交给我们自己的 Map 统一管理
    model: createModelFor(WELCOME_KEY, WELCOME_TEXT, "typescript"),
    // 不在这里写 theme！
    // create() 的 theme 选项会覆盖全局主题 —— 那样启动时会先把 UI 设成浅色，
    // 再被这里硬编码的深色覆盖掉，变成「界面白了、编辑器是黑的」。
    // 主题统一由 applyTheme() 管，编辑器创建后会自动沿用全局主题。
    automaticLayout: true,

    // 恢复上次的字号
    fontSize: currentFontSize.value,

    // ★ Ctrl + 滚轮缩放。默认是关的，打开就行 ——
    //   它会自己监听 wheel、自己算新字号、自己 preventDefault
    //   挡掉 WebView 的页面缩放，我们不用插手
    mouseWheelZoom: true,
  });

  // 订阅「光标位置变化」，同步给状态栏
  cursorSubscription = editorInstance.value.onDidChangeCursorPosition((event) => {
    cursor.value = {
      line: event.position.lineNumber,
      column: event.position.column,
    };
  });

  // 字号一变就记下来，并同步给菜单显示。
  //
  // ★ onDidChangeConfiguration 是「任何选项变化」都会触发的大事件，
  //   所以必须先用 hasChanged 筛出「真正变的是 fontSize」——
  //   否则切主题、改布局都会误报成「字号变了」
  fontSizeSubscription = editorInstance.value.onDidChangeConfiguration((event) => {
    if (!event.hasChanged(monaco.editor.EditorOption.fontSize)) return;

    const size = editorInstance.value?.getOption(monaco.editor.EditorOption.fontSize);
    if (typeof size !== "number") return;

    currentFontSize.value = size;
    window.localStorage.setItem(FONT_SIZE_KEY, String(size));

    // 滚轮会连续触发很多次。flashSaveNotice 每次都会重置倒计时，
    // 所以停手后还能看 2 秒 —— 正好当「实时指示器」用
    flashSaveNotice(`编辑器字号 ${size}px`);
  });

  // 先把语法扩展登记上（只扫元数据、不读语法内容，很快）。
  // 必须在恢复标签 / 打开文件**之前**完成 ——
  // 语言 id 得先注册好，否则模型会退化成纯文本
  await loadSyntaxExtensions();

  // 再把 VS Code 自带的主题接过来。它和语法扩展是两件事，各自独立失败
  await loadVscodeThemes();

  // 把上次没保存完的内容接回来。
  // 放在这里（而不是放到最后）是为了：即使下面注册关闭监听失败了，恢复也已经做完
  restoreHotExit();

  // Ctrl+R 刷新 / 关窗前浏览器会派发 beforeunload ——
  // 在这里**同步**补一次备份，免得「刚敲完就刷新」丢掉最后那几百毫秒
  window.addEventListener("beforeunload", writeHotExit);

  // 拦窗口关闭：有未保存的改动就先问一句。
  //
  // ★ 这是官方文档给的范式：**只在需要拦住的时候才 preventDefault**。
  //   不调 → 问完就放行，Tauri 自己会把窗口关掉；
  //   调了 → 窗口留下。
  //
  // ★ 为什么不「无脑先 preventDefault、确认后再自己 destroy()」：
  //   那样一旦中间抛异常（比如对话框调用失败），preventDefault 已经生效、
  //   destroy 却没跑到 —— **窗口就永远关不掉了**。
  //   官方这种写法是「失败安全」的：最坏情况只是没问就关了，至少不会关不掉。
  //
  // ★ 另外注意：Tauri 内部放行时调的是 destroy() 而不是 close()。
  //   因为 close() 会**再次**触发 closeRequested，又回到这个处理函数 —— 无限弹框。
  //   destroy() 是「不打招呼地关」，不会再触发事件。
  //   （也正因为这个内部调用，即便我们从不自己调 destroy，
  //     也需要 core:window:allow-destroy 这个权限）
  unlistenClose = await getCurrentWindow().onCloseRequested(async (event) => {
    if ((await askAboutUnsaved("退出前确认")) === "cancel") {
      event.preventDefault();
    }
  });

  // 恢复上次打开的文件夹。
  // 如果它已经被删了 / 读不了，openFolderPath 会把它从最近列表里剔掉，
  // 并回到「没有打开文件夹」的状态
  const lastFolder = window.localStorage.getItem(LAST_FOLDER_KEY);
  if (lastFolder) await openFolderPath(lastFolder);
});

onUnmounted(() => {
  window.removeEventListener("keydown", handleKeydown);
  window.removeEventListener("click", closeMenu);
  window.removeEventListener("beforeunload", writeHotExit);
  unlistenClose?.();

  // 定时器也是需要释放的外部资源，否则回调可能在组件销毁后才触发
  if (saveNoticeTimer) clearTimeout(saveNoticeTimer);
  if (hotExitTimer) clearTimeout(hotExitTimer);

  // 对象 URL 同理：不释放，那份图片数据会一直压在内存里
  swapPreviewUrl(null);

  // 顺序不能反：先退订事件，再销毁编辑器
  cursorSubscription?.dispose();
  fontSizeSubscription?.dispose();
  editorInstance.value?.dispose();

  // editor.dispose() 不会连带释放它用过的 model —— 必须自己关。
  // 否则这些 model 会一直留在 Monaco 的全局注册表里
  //（控制台里用 monaco.editor.getModels() 能看到）。
  for (const model of models.values()) model.dispose();
  models.clear();
  savedVersions.clear();
  contentSubscriptions.clear();
});

// ---------- 图片预览 ----------
//
// 图片不挂 Monaco，直接用 <img> 显示。
//
// 但 WebView 不能直接读磁盘：file:// 会被安全沙箱拒掉
//（否则任何一个网页都能拿绝对路径扫你的硬盘）。
// 所以链路是三步：
//   Rust 读字节 → 前端包成 Blob → createObjectURL 得到 blob: 地址 → 交给 <img>
//
// 这个 blob: 地址指向的是**内存里的那份数据**，和磁盘已经没有关系了，
// 所以用完之后必须自己释放 —— 见 swapPreviewUrl

const imagePreviewUrl = ref<string | null>(null);
const imagePreviewError = ref<string | null>(null);

// 滚动容器和里面的 <img>。缩放要靠它们测量真实尺寸
const imagePreviewRef = ref<HTMLElement | null>(null);
const imagePreviewImageRef = ref<HTMLImageElement | null>(null);

// ---------- 图片缩放 ----------
//
// 两个状态，别混成一个：
//   imageZoom = null   → 「适应窗口」。尺寸交给 CSS 的 max-width/max-height，
//                         窗口变了自己就跟上了，不用我们操心
//   imageZoom = 数字   → 用户手动指定的倍数，直接算出像素宽高
//
// 这样窗口缩放只影响前者，**不会把用户调好的倍数悄悄改掉**

/** null = 适应窗口 */
const imageZoom = ref<number | null>(null);

/** 原图尺寸。由 <img> 的 load 事件给出（naturalWidth / naturalHeight）*/
const imageNaturalSize = ref<{ width: number; height: number } | null>(null);

/** 正在拖动平移 */
const isPanning = ref(false);

const MIN_IMAGE_ZOOM = 0.05;
const MAX_IMAGE_ZOOM = 32;
/** 工具条上按一下缩放多少 */
const IMAGE_ZOOM_STEP = 1.25;

/** 只有手动缩放过才可能有东西可拖 */
const imagePannable = computed(() => imageZoom.value !== null);

const imageZoomLabel = computed(() =>
  imageZoom.value === null ? "适应" : `${Math.round(imageZoom.value * 100)}%`,
);

/** 手动倍数时给出具体像素宽高；适应窗口时返回空对象，让 CSS 去管 */
const imageZoomStyle = computed(() => {
  const size = imageNaturalSize.value;
  if (imageZoom.value === null || !size) return {};
  return {
    width: `${Math.round(size.width * imageZoom.value)}px`,
    height: `${Math.round(size.height * imageZoom.value)}px`,
  };
});

function onImageLoad(event: Event) {
  const img = event.target as HTMLImageElement;
  imageNaturalSize.value = { width: img.naturalWidth, height: img.naturalHeight };
}

/** 回到适应窗口，并把原图尺寸也忘掉（换图时要重算）*/
function resetImageZoom() {
  imageZoom.value = null;
  imageNaturalSize.value = null;
}

function fitImageToWindow() {
  imageZoom.value = null;
}

function showImageActualSize() {
  imageZoom.value = 1;
}

/**
 * 按比例缩放。cursor 是「以哪个点为锚」——
 * 给了就用鼠标位置（滚轮），没给就用容器中心（工具条按钮）。
 *
 * ★ 这里没用「居中偏移 + 滚动量」那套公式去反推锚点该滚到多少，
 *   而是**缩完再量一次**：量出那个锚点跑到哪去了，把差值用 scrollLeft/Top 补回来。
 *   公式得同时考虑 margin: auto 怎么居中、padding 多少、滚动条占几像素 ——
 *   全是容易算错的东西；而浏览器量出来的就是事实
 */
async function zoomImageBy(factor: number, cursor?: { x: number; y: number }) {
  const container = imagePreviewRef.value;
  const img = imagePreviewImageRef.value;
  const size = imageNaturalSize.value;
  if (!container || !img || !size) return;

  const before = img.getBoundingClientRect();
  if (before.width === 0 || before.height === 0) return;

  // 「适应窗口」时倍数是多少算不出来（是 CSS 定的），但**量得出来** —— 直接问浏览器
  const current = imageZoom.value ?? before.width / size.width;

  const next = Math.min(MAX_IMAGE_ZOOM, Math.max(MIN_IMAGE_ZOOM, current * factor));
  // 已经顶到上下限就别动 —— 白改一次会白触发一轮重新布局和滚动修正
  if (Math.abs(next - current) < 1e-4) return;

  // 锚点落在图片的哪个相对位置（0..1）
  const box = container.getBoundingClientRect();
  const cx = cursor?.x ?? box.left + box.width / 2;
  const cy = cursor?.y ?? box.top + box.height / 2;
  const u = (cx - before.left) / before.width;
  const v = (cy - before.top) / before.height;

  imageZoom.value = next;
  // ★ 必须等 DOM 换完尺寸再量 —— 量早了拿到的还是旧尺寸
  await nextTick();

  const after = img.getBoundingClientRect();
  // 锚点现在跑到了 after.left + u * after.width，把它拉回 cx 就行
  container.scrollLeft += after.left + u * after.width - cx;
  container.scrollTop += after.top + v * after.height - cy;
}

function onImageWheel(event: WheelEvent) {
  // deltaMode 为 1 表示单位是「行」而不是「像素」（部分鼠标驱动会这么报），
  // 先统一换算成像素，否则那种设备上一格只能挪一丁点
  const delta = event.deltaMode === 1 ? event.deltaY * 16 : event.deltaY;

  // 指数映射：一格滚轮（deltaY ≈ ±100）约放大 1.13 倍 / 缩小 0.88 倍；
  // 触控板那种很小的 delta 就是平滑的连续变化
  const factor = Math.exp(-delta * 0.0012);

  void zoomImageBy(factor, { x: event.clientX, y: event.clientY });
}

// ---------- 拖动平移 ----------
//
// 用**滚动位置**当真相，而不是自己算 translate：
// 这样滚动条自然就是「当前位置指示器」，和滚轮缩放的坐标系也是同一套

let panStart: { x: number; y: number; left: number; top: number } | null = null;

function onPreviewPointerDown(event: PointerEvent) {
  const container = imagePreviewRef.value;
  if (!container) return;

  // 没溢出就没得拖 —— 不拦的话光标会变成「抓手」但拖不动，反而像坏了
  const canPan =
    container.scrollWidth > container.clientWidth || container.scrollHeight > container.clientHeight;
  if (!canPan) return;

  panStart = {
    x: event.clientX,
    y: event.clientY,
    left: container.scrollLeft,
    top: container.scrollTop,
  };
  isPanning.value = true;
  // 把后续的 pointermove 都绑到这个元素上 ——
  // 否则鼠标拖出容器再回来就断了
  container.setPointerCapture(event.pointerId);
}

function onPreviewPointerMove(event: PointerEvent) {
  const container = imagePreviewRef.value;
  if (!panStart || !container) return;

  // 鼠标往右拖 → 内容跟着往右走 → 滚动位置减小。方向别反了
  container.scrollLeft = panStart.left - (event.clientX - panStart.x);
  container.scrollTop = panStart.top - (event.clientY - panStart.y);
}

function onPreviewPointerUp(event: PointerEvent) {
  panStart = null;
  isPanning.value = false;
  imagePreviewRef.value?.releasePointerCapture?.(event.pointerId);
}

/**
 * 换掉预览图源，并把旧的对象 URL 释放掉。
 *
 * ★ 后释放，不是先释放：先释放会闪一下空白。
 *
 * ★ 必须释放：createObjectURL 建的地址会让浏览器一直持着那份数据，
 *   不 revoke 就等于每预览一张图就往内存里塞一份永不回收的副本 ——
 *   这是 Blob URL 最典型的泄漏方式（它和 new 出来的对象一样，要「配对释放」）
 */
function swapPreviewUrl(next: string | null) {
  const previous = imagePreviewUrl.value;
  imagePreviewUrl.value = next;
  if (previous) URL.revokeObjectURL(previous);
}

async function showImagePreview(path: string | null) {
  // 切到非图片 / 所有标签都关了 → 清空预览
  if (!path) {
    swapPreviewUrl(null);
    imagePreviewError.value = null;
    return;
  }

  imagePreviewError.value = null;
  // 换图要把缩放和原图尺寸一起清掉 ——
  // 不清的话新图会沿用上一张的倍数，而且尺寸还是旧的，宽高会算得离谱
  resetImageZoom();

  try {
    const bytes = await invoke<ArrayBuffer>("read_preview_bytes", { path });

    // 竞态保护和 modelForPath 那条性质完全一样：
    // await 期间用户可能已经切到别的文件了，过期的图片不能覆盖新状态
    if (activeTabPath.value !== path) return;

    const blob = new Blob([bytes], {
      type: imageMimeOf(path) ?? "application/octet-stream",
    });
    swapPreviewUrl(URL.createObjectURL(blob));
  } catch (error) {
    if (activeTabPath.value !== path) return;
    swapPreviewUrl(null);
    imagePreviewError.value = String(error);
  }
}

// 激活的标签一变，就切换编辑器绑定的 model
// 注意监听的是 activeTabPath 而不是 openTabs —— 打开新标签但不激活它，
// 编辑器内容不该动
watch(activeTabPath, async (path) => {
  const editor = editorInstance.value;
  // 文件树在 DOM 挂载之后才渲染，正常情况下这里已经拿到实例了；
  // 留着判断是为了防御「在挂载完成前就被触发」的情况
  if (!editor) return;

  // 所有标签都关了 → 回欢迎页
  if (!path) {
    swapPreviewUrl(null);
    imagePreviewError.value = null;
    editor.setModel(createModelFor(WELCOME_KEY, WELCOME_TEXT, "typescript"));
    return;
  }

  // 图片：不建 model、不让 Monaco 参与，只换 <img> 的图源
  if (isImagePath(path)) {
    // 编辑器退回欢迎页 —— 它马上会被预览层完全盖住。
    // 但保持一个有效 model，之后切回文本文件时不用重来一遍
    editor.setModel(createModelFor(WELCOME_KEY, WELCOME_TEXT, "typescript"));
    cursor.value = { line: 1, column: 1 };
    await showImagePreview(path);
    return;
  }

  // 文本文件：先把可能开着的预览层关掉
  await showImagePreview(null);

  let model: monaco.editor.ITextModel;
  try {
    model = await modelForPath(path);
  } catch (error) {
    model = errorModelFor(path, error);
  }

  // ★ 竞态保护：await 期间用户完全可能又点了别的文件。
  // 不等这一下就 setModel 的话，假如 A 文件的磁盘读取慢了一拍，
  // 就会在你已经切到 B 之后把编辑器内容倒回 A ——
  // 这叫「过期的异步结果覆盖了新状态」，是异步 + 响应式最经典的 bug。
  if (activeTabPath.value !== path) return;

  editor.setModel(model);

  // 换 model 之后光标位置由 Monaco 决定（它会恢复该 model 上次的位置），
  // 所以我们不再假设「一定回到 1:1」，而是直接问它当前在哪，重新同步镜像
  const position = editor.getPosition();
  cursor.value = position
    ? { line: position.lineNumber, column: position.column }
    : { line: 1, column: 1 };
});
</script>

<template>
  <div class="app"> <!-- 最外层，占满全屏-->
    <!-- 菜单栏：应用内自绘的，不是系统菜单。
         @click.stop 让点菜单时不冒泡到 window —— 否则会被全局的关闭菜单监听器立刻收起来 -->
    <div class="menubar" @click.stop>
      <div v-for="menu in menus" :key="menu.label" class="menu">
        <button
          class="menu-title"
          type="button"
          :class="{ open: openMenu === menu.label }"
          @click="toggleMenu(menu.label)"
          @mouseenter="hoverMenu(menu.label)"
        >
          {{ menu.label }}
        </button>

        <div v-if="openMenu === menu.label" class="menu-dropdown">
          <template v-for="(item, index) in menu.items" :key="index">
            <div v-if="item.separator" class="menu-separator" />
            <button v-else class="menu-item" type="button" @click="runMenuItem(item)">
              <span>{{ item.label }}</span>
              <span v-if="item.shortcut" class="menu-shortcut">{{ item.shortcut }}</span>
            </button>
          </template>
        </div>
      </div>
    </div>

    <div class="main"> <!-- 中间区域：横向排列-->
      <!-- 活动栏：最左边那条窄图标栏。
           图标同时承担两个作用：标识当前视图 + 点击切换侧栏显隐（和 VS Code 一致） -->
      <nav class="activitybar">
        <button
          class="activity-item"
          type="button"
          :class="{ active: sidebarVisible }"
          title="资源管理器"
          @click="toggleSidebar"
        >
          <svg
            viewBox="0 0 24 24"
            width="24"
            height="24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.6"
            stroke-linecap="round"
            stroke-linejoin="round"
          >
            <path d="M3 6a2 2 0 0 1 2-2h3.5l2 2.5H19a2 2 0 0 1 2 2V18a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" />
          </svg>
        </button>
      </nav>

      <aside class="sidebar" v-show="sidebarVisible"> <!--左侧栏-->
        <div class="sidebar-header">
          <span class="sidebar-title" :title="workspaceRoot ?? ''">{{ sidebarTitle }}</span>
          <button class="sidebar-action" type="button" title="打开文件夹" @click="openFolder">
            <svg
              viewBox="0 0 24 24"
              width="16"
              height="16"
              fill="none"
              stroke="currentColor"
              stroke-width="1.8"
              stroke-linecap="round"
              stroke-linejoin="round"
            >
              <path d="M3 6a2 2 0 0 1 2-2h3.5l2 2.5H19a2 2 0 0 1 2 2V18a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" />
            </svg>
          </button>
        </div>

        <!-- 树单独包一层滚动容器，否则标题栏会跟着一起滚走 -->
        <div class="sidebar-tree">
          <p v-if="treeError" class="tree-error">{{ treeError }}</p>
          <p v-else-if="!workspaceRoot" class="sidebar-empty">还没有打开文件夹</p>
          <!-- 只遍历「顶层」节点；每个节点内部的子节点由 FileTreeNode 自己递归展开 -->
          <FileTreeNode v-for="node in fileTree" :key="node.path" :node="node" />
        </div>
      </aside>

      <!-- 编辑区：纵向 = 标签栏在上，Monaco 在下 -->
      <div class="editor-area">
        <div class="tabbar">
          <div
            v-for="path in openTabs"
            :key="path"
            class="tab"
            :class="{ active: path === activeTabPath }"
            :title="path"
            @click="activateTab(path)"
          >
            <span class="tab-name">{{ fileNameOf(path) }}</span>
            <!-- 有未保存改动时亮一个圆点 -->
            <span v-if="isDirty(path)" class="tab-dot" title="有未保存的改动">●</span>
            <!-- .stop 阻止冒泡：否则点 × 会先触发外层标签的 @click -->
            <span class="tab-close" @click.stop="closeTab(path)">×</span>
          </div>
        </div>
        <div class="editor" ref="editorContainer"></div> <!--Monaco 挂载点-->

        <!-- 图片预览层：盖在 Monaco 上面（定位基准是 .editor-area）。
             它不算「另一种标签」—— 标签栏里还是同一个 path，
             只是这一格的内容换了个渲染方式 -->
        <div
          v-if="activeTabPath && isImagePath(activeTabPath)"
          ref="imagePreviewRef"
          class="image-preview"
          :class="{ pannable: imagePannable, panning: isPanning }"
          @wheel.prevent="onImageWheel"
          @pointerdown="onPreviewPointerDown"
          @pointermove="onPreviewPointerMove"
          @pointerup="onPreviewPointerUp"
          @pointercancel="onPreviewPointerUp"
        >
          <img
            v-if="imagePreviewUrl"
            ref="imagePreviewImageRef"
            :src="imagePreviewUrl"
            :alt="fileNameOf(activeTabPath)"
            :class="{ 'image-fit': imageZoom === null }"
            :style="imageZoomStyle"
            draggable="false"
            @load="onImageLoad"
          />
          <p v-else-if="imagePreviewError" class="image-preview-msg error">
            {{ imagePreviewError }}
          </p>
          <p v-else class="image-preview-msg">正在加载…</p>
        </div>

        <!-- 缩放工具条。放在 .image-preview **外面** ——
             放里面的话它会跟着内容一起滚（那个容器是 overflow: auto 的）。
             它是「浮在浮层之上的控件」，所以 z-index 比浮层再高一档 -->
        <div
          v-if="activeTabPath && isImagePath(activeTabPath) && imagePreviewUrl"
          class="image-toolbar"
        >
          <button type="button" title="缩小" @click="zoomImageBy(1 / IMAGE_ZOOM_STEP)">−</button>
          <span class="image-zoom-label">{{ imageZoomLabel }}</span>
          <button type="button" title="放大" @click="zoomImageBy(IMAGE_ZOOM_STEP)">+</button>
          <span class="image-toolbar-sep"></span>
          <button type="button" title="适应窗口" @click="fitImageToWindow">适应</button>
          <button type="button" title="实际大小（100%）" @click="showImageActualSize">1:1</button>
        </div>

        <!-- 欢迎页：没打开任何文件时盖在编辑器上。
             和图片预览层是同一个套路 —— 不动标签数据结构，只在渲染层加一层。
             两层互斥：这层要求 activeTabPath 为空，那层要求它非空 -->
        <div v-if="!activeTabPath" class="welcome">
          <div class="welcome-body">
            <h1 class="welcome-title">new_vscode</h1>
            <p class="welcome-subtitle">编辑，从这里开始</p>

            <div class="welcome-columns">
              <section>
                <h2 class="welcome-heading">开始</h2>
                <ul class="welcome-list">
                  <li>
                    <button type="button" class="welcome-link" @click="newUntitledFile">
                      新建文件
                    </button>
                  </li>
                  <li>
                    <button type="button" class="welcome-link" @click="openFolder">
                      打开文件夹…
                    </button>
                  </li>
                  <li>
                    <button type="button" class="welcome-link" @click="cycleTheme">
                      切换主题（当前：{{ currentThemeLabel }}）
                    </button>
                  </li>
                </ul>
              </section>

              <section v-if="recentFolders.length > 0">
                <h2 class="welcome-heading">最近</h2>
                <ul class="welcome-list">
                  <li v-for="folder in recentFolders" :key="folder">
                    <button
                      type="button"
                      class="welcome-link recent-folder"
                      :title="folder"
                      @click="openRecentFolder(folder)"
                    >
                      <span class="recent-name">{{ fileNameOf(folder) }}</span>
                      <span class="recent-path">{{ folder }}</span>
                    </button>
                  </li>
                </ul>
              </section>

              <section>
                <h2 class="welcome-heading">键盘快捷键</h2>
                <ul class="welcome-list">
                  <li v-for="item in WELCOME_SHORTCUTS" :key="item.label" class="welcome-row">
                    <span class="welcome-keys">
                      <template v-for="(key, index) in item.keys" :key="key">
                        <span v-if="index > 0" class="welcome-plus">+</span>
                        <kbd>{{ key }}</kbd>
                      </template>
                    </span>
                    <span class="welcome-row-label">{{ item.label }}</span>
                  </li>
                </ul>
              </section>
            </div>
          </div>
        </div>
      </div>
    </div>
    <footer class="statusbar"> <!--底部状态栏-->
      <!-- 左侧：当前文件路径 -->
      <span class="status-path">{{ activeTabPath ? shortPath(activeTabPath) : "就绪" }}</span>
      <!-- 右侧：光标位置 / 语言 / 编码 / 缩进 -->
      <span class="status-items">
        <span v-if="saveNotice" class="status-notice">{{ saveNotice }}</span>
        <span>行 {{ cursor.line }}, 列 {{ cursor.column }}</span>
        <span>{{ currentLanguage }}</span>
        <span>UTF-8</span>
        <span>空格: 2</span>
        <!-- 点一下切下一个主题。等做了菜单栏可以移到「首选项」里 -->
        <span class="status-action" title="点击切换主题" @click="cycleTheme">
          {{ currentThemeLabel }}
        </span>
      </span>
    </footer>
  </div>
</template>

<style scoped>
.app {
  display: flex;
  flex-direction: column;
  height: 100%;
  background: var(--color-editor-bg);
  color: var(--color-text);
}

/* ---------- 菜单栏 ---------- */

.menubar {
  display: flex;
  flex: 0 0 var(--size-menubar);
  align-items: center;
  padding: 0 4px;
  background: var(--color-menubar-bg);
  font-size: 13px;
  user-select: none;
}

.menu {
  position: relative;   /* 下拉面板的定位基准 */
  height: 100%;
}

.menu-title {
  height: 100%;
  padding: 0 8px;
  border: 0;
  background: transparent;
  color: var(--color-text);
  font: inherit;
  cursor: pointer;
}

.menu-title:hover,
.menu-title.open {
  background: var(--color-menubar-hover);
}

.menu-dropdown {
  position: absolute;
  top: 100%;
  left: 0;
  /* 要盖在编辑器上面 */
  z-index: 10;
  min-width: 220px;
  padding: 4px 0;
  border: 1px solid var(--color-menu-border);
  background: var(--color-menu-bg);
  box-shadow: 0 4px 12px #00000055;
}

.menu-item {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 24px;
  width: 100%;
  padding: 5px 14px;
  border: 0;
  background: transparent;
  color: var(--color-text);
  font: inherit;
  text-align: left;
  cursor: pointer;
}

.menu-item:hover {
  background: var(--color-selection);
  color: var(--color-text-on-accent);
}

.menu-shortcut {
  color: var(--color-text-dim);
  font-size: 12px;
}

/* 悬停时快捷键提示也要变白，否则在深蓝底上根本看不清 */
.menu-item:hover .menu-shortcut {
  color: var(--color-text-on-accent);
}

.menu-separator {
  height: 1px;
  margin: 4px 0;
  background: var(--color-menu-border);
}

.main {
  display: flex;
  flex: 1;
  min-height: 0;
}

/* 活动栏：最左边那条窄图标栏。
   目前只有「资源管理器」一个，以后加搜索 / 源代码管理就往里追按钮 */
.activitybar {
  display: flex;
  flex: 0 0 var(--size-activitybar);
  flex-direction: column;
  align-items: center;
  background: var(--color-activitybar-bg);
}

.activity-item {
  position: relative;   /* 给下面那条 ::before 竖线当定位基准 */
  display: flex;
  align-items: center;
  justify-content: center;
  width: 100%;
  height: var(--size-activitybar);
  padding: 0;
  border: 0;
  background: transparent;
  color: var(--color-icon);
  cursor: pointer;
}

.activity-item:hover,
.activity-item.active {
  color: var(--color-icon-active);
}

/* 激活项左侧那条 2px 白线 —— VS Code 的标志性细节。
   用伪元素而不是额外加一个 div：它是纯装饰，不该进 DOM 结构 */
.activity-item.active::before {
  content: "";
  position: absolute;
  left: 0;
  top: 0;
  bottom: 0;
  width: 2px;
  background: var(--color-icon-active);
}

.sidebar {
  display: flex;
  flex: 0 0 var(--size-sidebar);
  flex-direction: column;
  background: var(--color-sidebar-bg);
  color: var(--color-text);
}

/* 侧栏标题栏：固定高度，不跟着文件树滚动 */
.sidebar-header {
  display: flex;
  flex: 0 0 var(--size-tabbar);   /* 和标签栏同高，视觉上对齐 */
  align-items: center;
  justify-content: space-between;
  padding: 0 6px 0 16px;
  font-size: 11px;
  letter-spacing: 0.04em;
}

.sidebar-title {
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}

.sidebar-action {
  display: flex;
  flex: 0 0 auto;
  align-items: center;
  justify-content: center;
  padding: 4px;
  border: 0;
  border-radius: 3px;
  background: transparent;
  color: var(--color-text);
  cursor: pointer;
}

.sidebar-action:hover {
  background: var(--color-hover);
}

/* 文件树自己的滚动容器 */
.sidebar-tree {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
}

.sidebar-empty {
  margin: 12px 16px;
  color: var(--color-text-dim);
  font-size: 12px;
}

/* 读目录失败时的提示（路径不存在、权限不足等） */
.tree-error {
  margin: 8px;
  padding: 8px;
  background: var(--color-error-bg);
  color: var(--color-error-text);
  font-size: 12px;
  border-radius: 4px;
  white-space: pre-wrap;
}

/* 编辑区整体：纵向排列（标签栏 + 编辑器），横向吃掉侧栏之外的全部宽度。
   这里的 min-width: 0 是给「横向」用的 —— 它现在是 .main 这个 row 容器的 item */
.editor-area {
  display: flex;
  flex: 1;
  flex-direction: column;
  min-width: 0;
  position: relative;   /* 给图片预览层当定位基准 */
}

.tabbar {
  display: flex;
  flex: 0 0 var(--size-tabbar);
  overflow-x: auto;    /* 标签多了横向滚动，不换行 */
  background: var(--color-tabbar-bg);
}

.tab {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 0 10px;
  background: var(--color-tab-bg);
  border-right: 1px solid var(--color-tabbar-bg);
  color: var(--color-text-dim);
  font-size: 13px;
  white-space: nowrap;
  cursor: pointer;
  user-select: none;
}

/* 未激活的标签悬停时给一点反馈，否则看不出可点 */
.tab:not(.active):hover {
  background: var(--color-tab-bg-hover);
}

.tab.active {
  background: var(--color-editor-bg);   /* 和编辑器背景同色，视觉上"连"成一体 */
  color: var(--color-text-emphasis);
}

.tab-name {
  min-width: 0;
  max-width: 140px;
  overflow: hidden;
  text-overflow: ellipsis;
}

/* 未保存标记 */
.tab-dot {
  color: var(--color-dirty);
  font-size: 10px;
  line-height: 1;
}

/* 关闭按钮平时透明，悬停到标签或标签激活时才出现 —— 否则标签栏太吵 */
.tab-close {
  padding: 0 4px;
  font-size: 14px;
  line-height: 1;
  border-radius: 3px;
  opacity: 0;
}

.tab:hover .tab-close,
.tab.active .tab-close {
  opacity: 1;
}

.tab-close:hover {
  background: var(--color-hover);
}

/* 编辑器本体：纵向吃掉标签栏之外的高度。
   注意这里是 min-height: 0（纵向），不再是 min-width —— 因为父容器 .editor-area
   是 column 方向，主轴变成纵向了。哪个方向在分配空间，就在哪个方向归零。 */
.editor {
  flex: 1;
  min-height: 0;
  /* 给个底色，避免 Monaco 初始化完成前闪一下白 */
  background: var(--color-editor-bg);

  /* ★★ 把 Monaco 关进它自己的层叠上下文里（这几行是欢迎页/图片预览能盖住它的关键）。

     背景：Monaco 内部有 `z-index: 5` 的元素（右侧那个 minimap 缩略图），
     而 `.editor` / `.editor-area` / `.main` 那时候全是 `z-index: auto` ——
     从上到下没有一层形成层叠上下文，于是那个 5 直接「漏」到了根层叠上下文，
     和我们的浮层同场比大小。按 CSS 绘制顺序，**正 z-index 永远压过 z-index: auto**，
     所以浮层盖不住它，缩略图会从右上角钻出来。

     修法不是把浮层的 z-index 调到 6、7（Monaco 一改版就崩），
     而是给这里一个 `position + z-index: 0`：
     从这一刻起 `.editor` 变成层叠上下文，**它内部的 z-index 再大也出不了这个盒子**。
     浮层就只需要「比这个盒子大」就行了，不用关心 Monaco 内部用了多少。

     通用原则：**嵌入式第三方组件一定要单独关进层叠上下文**，
     否则它的内部 z-index 会跑到你的布局里和别的东西打架。 */
  position: relative;
  z-index: 0;
}

/* ---------- 图片预览层 ---------- */

/* inset 的四个值对应 上 / 右 / 下 / 左。
   上边空出标签栏的高度，所以它正好盖住 Monaco 那一块 */
.image-preview {
  position: absolute;
  inset: var(--size-tabbar) 0 0 0;
  /* 只要比 .editor 那个「盒子」（z-index: 0）大就行 ——
     不用也没必要去追 Monaco 内部的 z-index 到底用了多少 */
  z-index: 1;
  /* 居中**不靠** justify-content，交给 img 的 margin: auto（见下面那条注释）。
     另外这层还能滚 —— 图放大到溢出之后靠它平移到看得见的部分 */
  display: flex;
  overflow: auto;
  padding: 16px;
  /* 必须不透明 —— 否则底下 Monaco 的内容会透上来 */
  background: var(--color-editor-bg);
}

/* 手动缩放过才可能有东西可拖，这时候给个「抓手」提示。
   真没溢出时拖了也不会动 —— 处理函数里有判断兜着 */
.image-preview.pannable {
  cursor: grab;
}

.image-preview.panning {
  cursor: grabbing;
}

.image-preview img {
  /* ★★ 必须禁掉浏览器原生的「拖拽图片」行为。
     不禁的话：按住图片一拖，浏览器会启动 HTML5 拖放，
     把 pointer 手势整个抢走（并派发 pointercancel）——
     桌面上的表现就是「拖动平移只能动第一下，然后就没反应了」。
     属性 draggable="false" + 这两行一起上，双保险 */
  -webkit-user-drag: none;
  user-select: none;

  /* ★ margin: auto 是「又能居中、又能滚」的关键：
     空间够时两个 auto 把多余空间对半分，图片就居中了；
     空间不够时 auto 解析成 0，图片贴在起始边，左上角能完整滚到。
     换成 justify-content: center 就没这个特性了 —— 图片放大到溢出之后，
     被挤到容器左边外面的那部分**永远滚不到** */
  margin: auto;
  /* 保持宽高比：宁可留白，也不把图拉伸变形 */
  object-fit: contain;
  /* 棋盘格底纹：透明背景的 png 也能看清图片边界。
     四层渐变错开叠出格子，是纯 CSS 画棋盘格的经典写法 */
  background-image: linear-gradient(45deg, var(--color-hover) 25%, transparent 25%),
    linear-gradient(-45deg, var(--color-hover) 25%, transparent 25%),
    linear-gradient(45deg, transparent 75%, var(--color-hover) 75%),
    linear-gradient(-45deg, transparent 75%, var(--color-hover) 75%);
  background-size: 16px 16px;
  background-position: 0 0, 0 8px, 8px -8px, -8px 0;
}

/* 只有「适应窗口」模式才让 CSS 管尺寸。
   手动倍数模式由 :style 直接给出像素宽高 ——
   这两行 max-* 要是写在基础规则里，手动放大就会被它们死死卡住 */
.image-preview img.image-fit {
  max-width: 100%;
  max-height: 100%;
}

/* ---------- 缩放工具条 ---------- */

/* 浮在预览层右上角 */
.image-toolbar {
  position: absolute;
  top: calc(var(--size-tabbar) + 10px);
  right: 14px;
  z-index: 2;   /* 比浮层（1）再高一档：它是浮在浮层之上的控件 */
  display: flex;
  align-items: center;
  gap: 2px;
  padding: 2px 4px;
  /* 底色/边框直接复用下拉菜单那套变量 —— 它们已经跟主题走了 */
  border: 1px solid var(--color-menu-border);
  border-radius: 6px;
  background: var(--color-menu-bg);
  box-shadow: 0 2px 8px #00000055;
}

.image-toolbar button {
  min-width: 24px;
  padding: 2px 6px;
  border: 0;
  border-radius: 4px;
  background: transparent;
  color: var(--color-text);
  /* font: inherit 不能省 —— button 有自己的默认字体，会和周围不搭 */
  font: inherit;
  font-size: 12px;
  line-height: 1.6;
  cursor: pointer;
}

.image-toolbar button:hover {
  background: var(--color-hover);
}

/* 百分比要定宽，否则数字位数一变，两边的按钮就跟着左右跳 */
.image-zoom-label {
  min-width: 44px;
  color: var(--color-text-dim);
  font-size: 12px;
  text-align: center;
}

.image-toolbar-sep {
  width: 1px;
  height: 14px;
  margin: 0 4px;
  background: var(--color-menu-border);
}

.image-preview-msg {
  margin: 0;
  color: var(--color-text-dim);
  font-size: 13px;
  text-align: center;
  white-space: pre-wrap;
}

.image-preview-msg.error {
  padding: 12px;
  border-radius: 4px;
  background: var(--color-error-bg);
  color: var(--color-error-text);
}

/* ---------- 欢迎页 ---------- */

/* 和 .image-preview 一样盖住标签栏以下的整个编辑区 */
.welcome {
  position: absolute;
  inset: var(--size-tabbar) 0 0 0;
  /* 见 .editor 里的说明：靠 z-index: 1 盖住那个「盒子」 */
  z-index: 1;
  overflow-y: auto;   /* 窗口矮的时候内容还能滚动看到 */
  background: var(--color-editor-bg);
}

/* 内容居中，但限宽 —— 不限的话宽屏下两栏会被扯得很远，行也长到不好读 */
.welcome-body {
  max-width: 720px;
  margin: 0 auto;
  /* 内边距跟着窗口缩（clamp 比固定 32px 在小窗口下友好得多）*/
  padding: 6vh clamp(16px, 4vw, 32px) 32px;
}

.welcome-title {
  margin: 0;
  font-size: 28px;
  /* 细字重：大字号再用粗体显得笨重，VS Code 那个大标题也是细的 */
  font-weight: 300;
  color: var(--color-text-emphasis);
}

.welcome-subtitle {
  margin: 4px 0 28px;
  color: var(--color-text-dim);
}

.welcome-columns {
  /* ★ 用 auto-fit + minmax 而不是固定的 1fr 1fr：
     定死两栏的话，窗口一窄每栏就只有一百多 px，
     快捷键行（键位 + 说明）直接溢出被裁。
     auto-fit 会在放不下两栏时自动改成上下堆叠 —— 一行 CSS 顶一条媒体查询 */
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
  gap: 28px 32px;
}

.welcome-heading {
  margin: 0 0 10px;
  font-size: 12px;
  font-weight: 600;
  letter-spacing: 0.06em;
  color: var(--color-text-dim);
}

.welcome-list {
  margin: 0;
  padding: 0;
  list-style: none;
}

.welcome-link {
  /* 铺满整行：这样高亮块看着像「一条链接」而不是「一个按钮」 */
  display: block;
  width: 100%;
  margin-bottom: 2px;
  padding: 4px 8px;
  border: 0;
  border-radius: 4px;
  background: transparent;
  /* font: inherit 不能省 —— button 有自己的默认字体，会明显比周围大一号 */
  font: inherit;
  color: var(--color-link);
  text-align: left;
  cursor: pointer;
}

.welcome-link:hover {
  background: var(--color-hover);
  text-decoration: underline;
}

.welcome-row {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 3px 8px;
}

/* 键位区定宽，右边的说明才能对齐成一列 ——
   不定宽的话每行说明都会跟着键位的长短漂移。
   120px 是按最长那行（Ctrl + 滚轮）量的，再多就是浪费 */
.welcome-keys {
  display: flex;
  flex: 0 0 120px;
  align-items: center;
  gap: 3px;
}

kbd {
  padding: 1px 5px;
  border: 1px solid var(--color-kbd-border);
  border-radius: 3px;
  background: var(--color-kbd-bg);
  color: var(--color-text);
  /* kbd 默认是等宽字体，改成跟 UI 字体走，看着更统一 */
  font-family: inherit;
  font-size: 11px;
  line-height: 1.7;
}

.welcome-plus {
  color: var(--color-text-dim);
}

.welcome-row-label {
  color: var(--color-text-dim);
}

/* 最近打开的文件夹：一行里左边是名字、右边是路径。
   覆盖面比 .welcome-link 的 display: block，要改成 flex 才能一行放两个 */
.recent-folder {
  display: flex;
  align-items: baseline;
  gap: 8px;
}

.recent-name {
  /* ★ 必须是 0 1 auto 而不是 0 0 auto：名字也得能收缩。
     写成不收缩的话，一个长名字会把右边的路径挤到只剩几十 px，
     路径反而看不见了 —— 而路径才是用来区分两个同名文件夹的信息。
     max-width 再给名字封个顶，保证路径至少能拿到一半宽度 */
  flex: 0 1 auto;
  min-width: 0;
  max-width: 50%;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}

.recent-path {
  /* flex: 1 1 auto + min-width: 0 是关键 ——
     不写 min-width 的话 flex 子项的默认 min-width 是 auto，
     它会**拒绝缩到内容以下**，ellipsis 永远不会生效 */
  flex: 1 1 auto;
  min-width: 0;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
  color: var(--color-text-dim);
  font-size: 12px;
}

.statusbar {
  flex: 0 0 var(--size-statusbar);
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0 10px;
  background: var(--color-statusbar-bg);
  /* ★ 单独一个前景色变量，而不是复用 on-accent：
     状态栏底色是**跟着 VS Code 主题走**的（Dark Modern 是深灰、Light Modern 是近白），
     不是固定的蓝底，所以文字颜色也必须跟着主题一起换 ——
     以前写死白色，浅色主题下就成了「白字白底」 */
  color: var(--color-statusbar-fg);
  font-size: 12px;
  user-select: none;
}

/* min-width: 0 让这个 flex item 允许被压缩到比内容更小，路径过长时才会出现省略号。
   这和之前 .main 上的 min-height: 0 是同一个道理，只是一个管纵向、一个管横向。 */
.status-path {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}

/* 右边这一组保持自身宽度，不被左边的长路径挤压 */
.status-items {
  display: flex;
  flex: 0 0 auto;
  gap: 14px;
  padding-left: 12px;
}

/* 保存提示：高亮一下，2 秒后自动消失 */
.status-notice {
  padding: 0 6px;
  border-radius: 3px;
  background: #ffffff33;
}

/* 状态栏上可点的小项（目前只有主题切换） */
.status-action {
  padding: 0 6px;
  border-radius: 3px;
  cursor: pointer;
}

.status-action:hover {
  background: #ffffff33;
}
</style>

<style>
:root {
  font-family: Inter, "Segoe UI", "Microsoft YaHei", Avenir, Helvetica, Arial, sans-serif;
  font-weight: 400;

  /* 告诉浏览器「当前是深色」。
     原生滚动条、文本选中高亮、<input> 这类由浏览器自己绘制的东西，
     完全不看我们的 CSS 变量，只认这个属性 —— 它是滚动条发白的根因 */
  color-scheme: dark;

  /* ============ 配色变量（深色 = VS Code Dark+）============
     为什么把颜色抽成变量：
       1. 同一个颜色散落在十几条规则里，改一次要改十几处
       2. 集中之后，「换主题」就变成「换这一组值」——
          下面 :root[data-theme="light"] 就是活生生的例子

     ★ 命名按「语义」而不是「颜色值」：
       --color-text-emphasis 而不是 --color-white。
       因为浅色主题下“想强调的文字”是黑的，叫 white 就完全误导了。

     CSS 自定义属性会沿 DOM 继承，所以定义在 :root 上，
     下面 scoped 样式里的 var(...) 一样能取到 */
  --color-activitybar-bg: #333333;
  --color-sidebar-bg: #252526;
  --color-editor-bg: #1e1e1e;
  --color-tabbar-bg: #252526;
  --color-tab-bg: #2d2d2d;
  --color-tab-bg-hover: #323233;
  --color-statusbar-bg: #007acc;
  --color-statusbar-fg: #ffffff;   /* 兜底：主题读不到时按经典「蓝底白字」来 */

  --color-text: #cccccc;
  --color-text-dim: #969696;
  --color-text-emphasis: #ffffff;   /* 激活标签、强调文字 */
  --color-text-on-accent: #ffffff;  /* **我们自己的深色底**（选中项、菜单悬停）上的文字 */

  /* 活动栏图标。这两个值必须和「活动栏背景」配套 ——
     背景深，图标就得浅；背景浅，图标就得深。
     所以它们在下面浅色主题里会被覆盖，不再是一处定义两处通用 */
  --color-icon: #858585;
  --color-icon-active: #ffffff;   /* 兼任激活项左侧那条竖线 */

  --color-hover: #2a2d2e;
  --color-selection: #094771;
  --color-dirty: #e8ab53;
  --color-error-bg: #5a1d1d;
  --color-error-text: #f48771;

  /* 滚动条拇指。故意用半透明而不是写死的实色 ——
     轨道是透明的，拇指直接压在侧栏/标签栏的背景上，
     半透明能让它自己跟背景融合，不用为每个容器各配一个值 */
  --color-scrollbar-thumb: rgba(121, 121, 121, 0.4);
  --color-scrollbar-thumb-hover: rgba(121, 121, 121, 0.7);

  /* 欢迎页里的链接色。VS Code 的 TextLink 就是这个色 */
  --color-link: #3794ff;

  /* 快捷键帽（kbd）。故意用**半透明灰**而不是实色 ——
     深浅两套主题可以共用同一组值（下面浅色主题不用再写一遍） */
  --color-kbd-bg: rgba(128, 128, 128, 0.17);
  --color-kbd-border: rgba(128, 128, 128, 0.35);

  --color-menubar-bg: #3c3c3c;
  --color-menubar-hover: #505050;
  --color-menu-bg: #252526;
  --color-menu-border: #454545;

  --size-menubar: 35px;
  --size-activitybar: 48px;
  --size-sidebar: 240px;
  --size-tabbar: 35px;
  --size-statusbar: 24px;

  font-synthesis: none;
  text-rendering: optimizeLegibility;
  -webkit-font-smoothing: antialiased;
  -moz-osx-font-smoothing: grayscale;
  -webkit-text-size-adjust: 100%;
}

/* 浅色主题：只覆盖「和深色不一样」的变量。
   写在这里的每一条都是真的和深色不同 —— 取值相同的写进来只是冗余 */
:root[data-theme="light"] {
  color-scheme: light;

  /* 比侧栏（#f3f3f3）深一档，和菜单栏（#dddddd）同色 ——
     于是顶栏和左栏连成一个「L 形的框」，把明亮的内容区包在中间。
     深色主题里活动栏比侧栏「亮」，浅色里比侧栏「深」：
     方向是反的，但效果一致 —— 都是让它比侧栏更靠近观者 */
  --color-activitybar-bg: #dddddd;
  --color-sidebar-bg: #f3f3f3;
  --color-editor-bg: #ffffff;
  --color-tabbar-bg: #f3f3f3;
  --color-tab-bg: #ececec;
  --color-tab-bg-hover: #e0e0e0;
  --color-statusbar-bg: #007acc;
  --color-statusbar-fg: #ffffff;

  --color-text: #333333;
  --color-text-dim: #6f6f6f;
  --color-text-emphasis: #000000;
  --color-text-on-accent: #ffffff;

  /* 活动栏背景变浅了，图标就必须翻成深的，
     否则灰图标糊在浅灰底上根本看不清。
     这两个值一变，激活项那条竖线也跟着变黑（它俩共用 --color-icon-active）*/
  --color-icon: #616161;
  --color-icon-active: #000000;

  --color-hover: #e8e8e8;
  --color-selection: #0060c0;
  --color-dirty: #b8860b;
  --color-error-bg: #f8d7da;
  --color-error-text: #a94442;

  --color-scrollbar-thumb: rgba(100, 100, 100, 0.35);
  --color-scrollbar-thumb-hover: rgba(100, 100, 100, 0.6);

  --color-link: #0066bf;

  --color-menubar-bg: #dddddd;
  --color-menubar-hover: #c4c4c4;
  --color-menu-bg: #ffffff;
  --color-menu-border: #c8c8c8;
}

html,
body,
#app {
  height: 100%;
}

body {
  margin: 0;
  overflow: hidden;
  font-size: 13px;
  line-height: 1.5;
}

/* ============ 滚动条 ============
   为什么原生滚动条不跟主题走：
     WebView2 就是 Chromium，它的默认滚动条由 compositor 直接绘制，
     根本不读我们的 CSS 变量。它只认上面那个 color-scheme，
     默认取 light —— 所以深色主题下那条依旧是白的。

   两条腿走路：
     ① color-scheme：让原生控件知道当前是深是浅
     ② ::-webkit-scrollbar：干脆自己把滚动条画成 VS Code 那种细的、半透明的

   这里不写选择器 = 全局生效，侧栏、标签栏、下拉菜单一次覆盖。
   Monaco 的滚动条不受影响 —— 那是它自己用 div 画的，本来就跟主题走。

   ⚠ 不要同时写 scrollbar-width / scrollbar-color：
     Chromium 一旦看到它们，就会完全忽略 ::-webkit-scrollbar。
     两套机制互斥，混用会出现「样式明明写了却不生效」 */
::-webkit-scrollbar {
  width: 10px;    /* 纵向滚动条的宽度 */
  height: 10px;   /* 横向滚动条的高度，标签栏用的就是这个 */
}

/* 轨道画成透明，只留拇指 —— 这是 VS Code 观感的关键 */
::-webkit-scrollbar-track {
  background: transparent;
}

::-webkit-scrollbar-thumb {
  background-color: var(--color-scrollbar-thumb);
  /* 透明边框 + background-clip: padding-box：
     背景只画进 padding 区，外面那圈 border 保持透明。
     于是 10px 的滚动条看起来只有 6px 宽 —— 有留白，更像 VS Code。
     border 的作用是「撑开留白」而不是画线，所以颜色必须是 transparent */
  border: 2px solid transparent;
  background-clip: padding-box;
  border-radius: 5px;
}

/* 这里用 background-color 而不是 background 简写：
   简写会把 background-clip 重置回 border-box，
   拇指就会突然「变宽」（padding-box 的裁剪没了）。
   属性被简写悄悄重置，是 CSS 里很常见的一类坑 */
::-webkit-scrollbar-thumb:hover {
  background-color: var(--color-scrollbar-thumb-hover);
}

/* 横竖两条滚动条交叉处的小方块，默认也是白的 */
::-webkit-scrollbar-corner {
  background: transparent;
}

/* 两端的箭头顶，Windows 上默认会显示 */
::-webkit-scrollbar-button {
  display: none;
}
</style>