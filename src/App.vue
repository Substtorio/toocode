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
import { registerInjections, registerTextMateLanguage } from "./textmate";
import { registerSnippetCompletions } from "./snippets";
import { parseSnippetFile, type ParsedSnippet } from "./snippetParser";
import {
  acceptChange,
  pendingChanges,
  rejectChange,
  reviewChange,
  reviewingChange,
  setWriteListener,
} from "./agentChanges";
// 把 VS Code 的主题文件翻译成 Monaco 主题的地方（插件机制的第二块）
import { loadVscodeTheme } from "./vscodeTheme";
import { isInside, normalizePath, relativePath, samePath } from "./pathUtils";
// 编辑器状态桥：把「用户此刻在编辑什么」暴露给 Agent
import {
  MAX_SELECTION_CHARS,
  provideEditorBridge,
  type EditorContext,
  type OpenDocument,
} from "./editorBridge";
import QuickOpen, { type QuickOpenEntry } from "./components/QuickOpen.vue";
import FileTreeNode from "./components/FileTreeNode.vue";
import MenuList from "./components/MenuList.vue";
import ChatPanel from "./components/ChatPanel.vue";
import TerminalPanel from "./components/TerminalPanel.vue";
import { fileTreeExpansionKey, fileTreeSelectionKey } from "./injectionKeys";
// 语言服务器客户端。★ 它不知道 `models` 表长什么样 —— 以两个函数的
// 形式把口子开给它（和 editorBridge 一个套路）
import {
  changeDocument,
  closeDocument,
  configureLsp,
  disposeLsp,
  openDocument,
} from "./lsp";
// 语言服务器 → Monaco 的三个 provider（补全 / 悬停 / 跳转定义）
import { registerLspLanguage, registerLspOpenHandler } from "./lspFeatures";
// 应用图标：直接拿 Tauri 打包用的那张 128×128 —— 不用再维护第二份图。
// ⚠ 以前用的是 32x32 那张，因为当时只显示 16px。logo 放大到 22px 之后，
//   高分屏下 32px 的源会被拉大（2x 屏需要 44 物理像素）⇒ 发糊，所以换大的
import appIconUrl from "../src-tauri/icons/128x128.png";
// 应用版本号。★ 直接读 tauri.conf.json —— 它是**唯一权威**的那份：
//   打包出来的安装包名、git tag 都是照着它来的。
//   写死一个字符串就等于多了第二份真相，迟早对不上
import appConf from "../src-tauri/tauri.conf.json";
import type { FileNode, Menu, MenuItem, SearchMatch, SearchResponse } from "./types";

/**
 * 当前窗口。
 *
 * ★ 为什么在顶层求值一次、而不是每次现调 `getCurrentWindow()`：
 *   在 Tauri 环境之外（比如直接用浏览器打开 dev server 看 UI），
 *   `getCurrentWindow()` 会**同步抛异常** —— 不是返回 rejected promise，
 *   所以 `.catch()` 根本接不住。
 *   而 `applyTheme` 是在 setup 顶层跑的，一抛就整个应用白屏。
 *   这里统一兜住：拿不到就是 null，各处用 `appWindow?.xxx()` 跳过即可
 */
const appWindow = (() => {
  try {
    return getCurrentWindow();
  } catch {
    return null;
  }
})();

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

/**
 * 当前激活的侧栏视图。
 *
 * ★ 为什么不能继续拿 `sidebarVisible` 兼着：
 *   之前只有一个视图，「侧栏开着」和「资源管理器是激活的」恰好等价。
 *   有了搜索之后就不等价了 —— 搜索视图开着时，资源管理器那个图标应该是**不亮**的。
 *   用一个布尔去表达两件事，迟早会在加第二个视图时错位
 */
type ViewId = "explorer" | "search" | "scm";
const activeView = ref<ViewId>("explorer");

/**
 * 活动栏上的图标。加一个视图只要往这里追一项 + 一个渲染分支。
 *
 * ★ Topilot **不在**这里 —— 它是右侧的独立面板，入口在标题栏命令中心的右边
 *  （VS Code 的 Copilot 也是这么放的）。
 *   理由不是「怕挤」：它是**常驻的对话区**，和资源管理器 / 搜索这种
 *   「在左边翻文件的工作视图」根本不是一类东西。
 *   而且活动栏的图标天生是**单选**的（activeView 只能有一个值）——
 *   把它塞进来，就变成「开着 Topilot 就没法同时看资源管理器」
 */
/**
 * 活动栏上的图标。加一个视图只要往这里追一项 + 一个渲染分支。
 *
 * ★★ 三个图标**都是 VS Code 原生的 codicon 路径**，从
 *   `raw.githubusercontent.com/microsoft/vscode-codicons/main/src/icons/<名字>.svg`
 *   一字不改地抄来的 —— 形状和原生完全一致，不是「照着感觉画的」。
 *   想要新图标：`Invoke-WebRequest` 下下来，把 `<path d="...">` 抄进 `icon` 字段。
 *
 * ★★ **codicon 是「填充」图形，不是描边图形**（`fill="currentColor"`，没有 `stroke`）。
 *   图上那些圆之所以看着像空心环，靠的不是描边，而是子路径**绕向相反** ——
 *   在 nonzero 填充规则下把内圈那块**挖掉**。
 *   ⇒ 所以它们只能走 `fill`（见下面 `.activity-item svg` 那条规则），
 *     按描边去画会得到「粗描边轮廓图」（孔的边被填小、连接线变成粗条），
 *     和原生完全不是一个东西。
 *
 * ⚠ **抄之前先看 svg 上的 `viewBox`，网格尺寸不一样**：
 *   `files` / `search-large` / `source-control` 都是 24×24，
 *   但 `search`（不带 -large）是 **16×16** —— 那个是给菜单 / 按钮用的，
 *   活动栏要的是 `search-large`。
 *
 * ⚠ ★ **codicon 是「满格」设计**：内容从 x/y 的 0 一路铺到 24。
 *   所以它们比之前手画的那版（16~17 高）**明显大一圈** ——
 *   而原生 VS Code 里所有活动栏图标都是这个尺寸，是手画的那版偏小了。
 *
 * ★ Topilot **不在**这里 —— 它是右侧的独立面板，入口在标题栏命令中心的右边
 *  （VS Code 的 Copilot 也是这么放的）。
 *   理由不是「怕挤」：它是**常驻的对话区**，和资源管理器 / 搜索这种
 *   「在左边翻文件的工作视图」根本不是一类东西。
 *   而且活动栏的图标天生是**单选**的（activeView 只能有一个值）——
 *   把它塞进来，就变成「开着 Topilot 就没法同时看资源管理器」
 */
const ACTIVITY_VIEWS: Array<{ id: ViewId; label: string; icon: string }> = [
  {
    id: "explorer",
    label: "资源管理器",
    // codicon: files —— 两份叠起来的文档（后面那张只露出一角，所以整体略偏左）
    icon:
      "M7.5 22.5H17.595C17.07 23.4 16.11 24 15 24H7.5C4.185 24 1.5 21.315 1.5 18V6C1.5 4.89 2.1 3.93 3 3.405V18C3 20.475 5.025 22.5 7.5 22.5ZM21 8.121V18C21 19.6545 19.6545 21 18 21H7.5C5.8455 21 4.5 19.6545 4.5 18V3C4.5 1.3455 5.8455 0 7.5 0H12.879C13.4715 0 14.0505 0.24 14.4705 0.6585L20.3415 6.5295C20.766 6.954 21 7.5195 21 8.121ZM13.5 6.75C13.5 7.164 13.8375 7.5 14.25 7.5H19.1895L13.5 1.8105V6.75ZM19.5 18V9H14.25C13.0095 9 12 7.9905 12 6.75V1.5H7.5C6.672 1.5 6 2.1735 6 3V18C6 18.8265 6.672 19.5 7.5 19.5H18C18.828 19.5 19.5 18.8265 19.5 18Z",
  },
  {
    id: "search",
    label: "搜索",
    // codicon: **search-large**（不是 search！那个是 16×16 网格的）
    icon:
      "M22.281 21.219L16.0875 15.0255C17.2815 13.5945 18 11.754 18 9.74854C18 5.19904 14.298 1.49854 9.75 1.49854C5.202 1.49854 1.5 5.20054 1.5 9.75004C1.5 14.2995 5.202 18 9.75 18C11.7555 18 13.5945 17.28 15.027 16.0875L21.2205 22.281C21.3675 22.428 21.5595 22.5 21.7515 22.5C21.9435 22.5 22.1355 22.4265 22.2825 22.281C22.575 21.9885 22.575 21.513 22.2825 21.2205L22.281 21.219ZM9.75 16.5C6.0285 16.5 3 13.4715 3 9.75004C3 6.02853 6.0285 3.00004 9.75 3.00004C13.4715 3.00004 16.5 6.02853 16.5 9.75004C16.5 13.4715 13.4715 16.5 9.75 16.5Z",
  },
  {
    id: "scm",
    label: "源代码管理",
    // codicon: source-control —— 左边两个节点串成一条线，中间往右上方引出一根旁支
    icon:
      "M21 8.25C21 6.1815 19.3185 4.5 17.25 4.5C15.1815 4.5 13.5 6.1815 13.5 8.25C13.5 10.023 14.739 11.5035 16.395 11.892C16.116 12.819 15.2655 13.5 14.25 13.5H9.75C8.9025 13.5 8.1285 13.7925 7.5 14.268V7.4235C9.21 7.0755 10.5 5.5605 10.5 3.75C10.5 1.6815 8.8185 0 6.75 0C4.6815 0 3 1.6815 3 3.75C3 5.562 4.29 7.0755 6 7.4235V16.575C4.29 16.923 3 18.438 3 20.2485C3 22.317 4.6815 23.9985 6.75 23.9985C8.8185 23.9985 10.5 22.317 10.5 20.2485C10.5 18.4755 9.261 16.995 7.605 16.6065C7.884 15.6795 8.7345 14.9985 9.75 14.9985H14.25C16.0845 14.9985 17.61 13.6725 17.931 11.9295C19.674 11.607 21 10.0845 21 8.25ZM4.5 3.75C4.5 2.5095 5.5095 1.5 6.75 1.5C7.9905 1.5 9 2.5095 9 3.75C9 4.9905 7.9905 6 6.75 6C5.5095 6 4.5 4.9905 4.5 3.75ZM9 20.25C9 21.4905 7.9905 22.5 6.75 22.5C5.5095 22.5 4.5 21.4905 4.5 20.25C4.5 19.0095 5.5095 18 6.75 18C7.9905 18 9 19.0095 9 20.25ZM17.25 10.5C16.0095 10.5 15 9.4905 15 8.25C15 7.0095 16.0095 6 17.25 6C18.4905 6 19.5 7.0095 19.5 8.25C19.5 9.4905 18.4905 10.5 17.25 10.5Z",
  },
];

/** Topilot 那个气泡图标。标题栏的入口按钮也用它，所以提出来共用 */
const CHAT_ICON =
  "M4 5a2 2 0 0 1 2-2h12a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2h-8l-4 4v-4a2 2 0 0 1-2-2z";

/**
 * 点活动栏图标。
 *
 * ★ 和 `toggleSidebar` 的语义不同：那是「开关侧栏」，这是「切视图」——
 *   点**已经激活**的那个才等价于关侧栏（VS Code 就是这个行为）
 */
function toggleView(id: ViewId) {
  if (activeView.value === id && sidebarVisible.value) {
    sidebarVisible.value = false;
    return;
  }
  activeView.value = id;
  sidebarVisible.value = true;
}

// ---------- Topilot 面板（右侧独立的一块）----------
//
// ★ 为什么不复用 activeView / sidebarVisible：Topilot 在**右边**，
//   和左边栏是两张互不相干的卡片，**可以同时开着**。
//   用一个变量去管两个独立的东西，就一定会在「同时开」的时候错位
const chatVisible = ref(false);

function toggleChat() {
  chatVisible.value = !chatVisible.value;
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

/**
 * 清空最近列表。
 *
 * ★ 必须**连 LAST_FOLDER_KEY 一起清** —— 那是「下次启动自动打开哪个」。
 *   只清列表的话，下次启动还是会自动打开那个文件夹，而列表却说是空的，
 *   两边对不上。这两个 key 职责不同，但清空时得一起动
 */
function clearRecentFolders() {
  saveRecentFolders([]);
  window.localStorage.removeItem(LAST_FOLDER_KEY);
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
  /* ★ 这里**可以**映射 tab.hoverBackground。
     ⚠ 之前记的「它在 Dark Modern / Dark+ / Dark VS 三层里都不存在、是一条死映射」
       是**错的** —— 它确实存在，定义在 dark_modern.json 里。
       当初之所以取不到值，是因为读主题时**没有递归合并 include 链**：
       手上只有最终那一层（dark_vs / dark_plus），而这条在更上面那层。
     ★ 而它一接上，就顺带推翻了「悬停统一压暗」那个策略：
       深色 #1F1F1F（比非活动标签 #2D2D2D 更暗）、浅色 #FFFFFF（比 #ECECEC 更亮）——
       两边的方向是**相反**的。这种事本来就该让主题决定，不该自己定规则 */
  ["--color-tab-bg-hover", "tab.hoverBackground"],
  ["--color-tab-hover-fg", "editor.foreground"],
  /* ★ 下面这几条是「UI 配色补丁」补上的：原来它们在 <style> 里写死，
     深浅各写一份，而且和 VS Code 的真实观感对不上。
     ⚠ 挑键时必须**实测两边都有** —— 见 scripts/check-theme-colors.mjs
       （dark_modern 152 条 / light_modern 161 条，确实有些键只有一边有。
        比如 list.hoverBackground 只有浅色有，所以 --color-hover 至今映射不了） */
  ["--color-selection", "menu.selectionBackground"],
  ["--color-menu-bg", "editorWidget.background"],
  ["--color-menu-border", "menu.border"],
  /* ⚠ statusBarItem.hoverBackground **故意不映射** ——
     状态栏是固定蓝、本来就不跟主题走（见下面 THEME_VARS_CLEAR_ONLY），
     而主题里那个值是浅灰半透明（深色 #F1F1F133），铺在蓝底上是错的。
     保持 <style> 里的半透明白 */
  /* ⚠ 状态栏那两个**故意不映射** —— 见下面 THEME_VARS_CLEAR_ONLY 的说明 */
  ["--color-menubar-bg", "titleBar.activeBackground"],
  ["--color-text", "sideBar.foreground"],
  ["--color-text-dim", "tab.inactiveForeground"],
  ["--color-text-emphasis", "tab.activeForeground"],
  ["--color-icon", "activityBar.inactiveForeground"],
  /* 悬停时的图标颜色。★ 原来叫 --color-icon-active，改名了 ——
     那个名字现在留给「当前视图那个蓝色图标」（见本表最后两行） */
  ["--color-icon-hover", "activityBar.foreground"],
  /* ★★ 悬停方块 + 当前视图的图标颜色。
     · hoverBackground（深 #FFFFFF11 / 浅 #F2F2F2）和 activeBorder
       （深 #0078D4 / 浅 #005FB8）主题文件里真有，所以能正常映射
     · activeBorder 以前是给「激活项左侧那条竖线」用的（那个变量叫
       --color-activitybar-active-border）—— 竖线改成方块之后，
       **同一个键换了个去处**：从画线转去给图标上色
     ⚠ ★ `modernActivityBarItem.activeBackground` **故意不映射**：
       它在深色下是 `#FFFFFF22`，也就是**比活动栏底更亮** —— 而我们要的恰恰相反，
       选中项要**压得更深**。方向不对，所以接不上。
       而且主题里也**没有**比 `activityBar.background`(#181818) 更深的键
       （和 `--color-shell-bg` 是同一个处境），只能写死两套兜底。
       见下面 `--color-icon-active-bg` */
  ["--color-icon-hover-bg", "modernActivityBarItem.hoverBackground"],
  ["--color-icon-active", "activityBar.activeBorder"],
  ["--color-link", "textLink.foreground"],
];

/**
 * 这些变量**不跟主题走**，但 `applyTheme` 仍然要把它们从 <html> 的 inline style 上清掉。
 *
 * ★ 为什么必须单独列出来：`applyTheme` 是**遍历 `CSS_VAR_BY_COLOR`** 来清内联变量的。
 *   如果只是把它们从映射表里删掉，那么**上一次设过的内联值会永远赖着不走** ——
 *   内联 style 的优先级最高，之后换什么主题都盖不掉它。
 *
 * ★ 状态栏为什么固定成蓝色：VS Code 现在的默认（Dark / Light Modern）是深灰 / 近白，
 *   但经典的 `#007acc` 蓝辨识度高得多。想让它跟随主题的话，
 *   把上面那两行的映射加回来、并从这个数组里删掉即可。
 */
const THEME_VARS_CLEAR_ONLY = [
  "--color-statusbar-bg",
  "--color-statusbar-fg",
  // ★── 它**曾经**映射过 `modernActivityBarItem.activeBackground`，后来改成了写死的兜底值
  //     （因为那个键在深色下是「更亮」，而我们要「更深」）。
  //     从映射表里删掉之后，**上一次设过的内联值会永远赖着不走** ——
  //     内联 style 优先级最高，换什么主题都盖不掉它，
  //     于是活动栏里那个方框的颜色会一直卡在 #FFFFFF22，而且不报错。
  //     所以必须靠这个数组把它清掉
  "--color-icon-active-bg",
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

/**
 * Monaco 的滚动条尺寸。主编辑器和 diff 浮层共用这一份 —— 两处写两套迟早会不一致。
 *
 * ★★ 为什么必须自己给：Monaco 的滚动条**不归 `::-webkit-scrollbar` 管**，
 *   它是拿 div 自己画的。默认是 **14px 满宽、方角**，
 *   而侧栏 / 面板 / 欢迎页那些走全局 CSS（10px 轨道 + 2px 内缩 ⇒ 6px 视觉宽），
 *   两者并排放着就是两个应用的样子。
 *
 * ⚠ ★★ **这是命名空间选项，必须挂在 `scrollbar: {}` 下面**。
 *   写成顶层的 `verticalScrollbarSize: 10` 会被 **静默忽略** ——
 *   不报错、不警告，轨道就是不变（我第一版就是这么写的，还量了半天才反应过来）。
 *   它在源码里长这样：`super(EditorOption.scrollbar, 'scrollbar', defaults, …)`
 *
 * ★ `…ScrollbarSize` 是**轨道**宽，`…SliderSize` 是**滑块**宽 —— 是两个选项。
 *   我们只写前者的话，滑块会被撑满整个轨道（10px 实心条），
 *   和 CSS 那套「10px 轨道里一条 6px 的滑块」还是不一样（10 - 2×2 = 6）。
 */
const MONACO_SCROLLBAR = {
  verticalScrollbarSize: 10,
  verticalSliderSize: 6,
  horizontalScrollbarSize: 10,
  horizontalSliderSize: 6,
};

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
  // 清的范围 = 所有可能被内联设过的变量：映射过的 + 只清除的。
  // 后者见 THEME_VARS_CLEAR_ONLY 的说明 —— 漏掉它们的话旧值会赖着不走
  for (const cssVar of [
    ...CSS_VAR_BY_COLOR.map(([name]) => name),
    ...THEME_VARS_CLEAR_ONLY,
  ]) {
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

  // ★ 最外面那圈窗口边框是**系统画的**（Tauri 默认 decorations: true），
  //   它不会自己跟着应用走 —— 得主动告诉系统该用深色还是浅色。
  //   Windows 上这相当于设置 DWMWA_USE_IMMERSIVE_DARK_MODE，标题栏会变成暗色。
  //   ★ 传明确的 dark/light，**不是 null** —— null 的语义是「跟随系统」，
  //     可我们有自己的主题开关，不能让它跟系统跑偏
  appWindow
    ?.setTheme(id === "dark" ? "dark" : "light")
    .catch(() => {
      // 没给权限 / 平台不支持都无所谓：只是标题栏颜色不对，不该影响别的功能
    });

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
    // 这条日志放在**前端**而不是 Rust 里：Rust 那边用 println! 只会打到 dev 终端，
    // 前端的「输出」面板收不到 —— 而面板才是用户看日志的地方。
    // （下面 `wanted` 只挑了两个用，所以这里特别说明扫到的总数）
    console.info(`[主题扩展] 扫到 ${entries.length} 个可用主题，本次使用 Dark Modern / Light Modern`);
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
 * ★ 参数是**路径**而不是 FileNode：函数体本来就只用到了 path，
 *   而「跳到定义」那边手里只有一条路径（甚至可能是工作区外面的文件）——
 *   要是收 FileNode，那边就得在调之前先拼一个假节点出来，纯属多余
 */
function openFile(path: string) {
  if (!openTabs.value.includes(path)) {
    openTabs.value.push(path);
  }
  activeTabPath.value = path;
  // 用户主动打开 ⇒ 记一笔导航历史
  recordVisit(path);
}

/** 点击标签栏上的标签 */
function activateTab(path: string) {
  activeTabPath.value = path;
  // 点标签栏也是「切换到了一个位置」，VS Code 里同样会进导航历史
  recordVisit(path);
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

// ---------- 导航历史（标题栏那对 ← →）----------
//
// ★ 这是个**简化版**：VS Code 记的是「文件 + 光标位置」，我们只记文件 ——
//   够用，而且避开了「打开后等 model 加载完再 setPosition」那套异步时序问题。
//
// ★ 为什么是两个状态而不是一个栈：停在历史中间时再打开新文件，
//   要把「未来」截掉（和浏览器的前进后退一个道理），
//   光有一个栈没法表达「我现在站在哪儿」。

const visitHistory = ref<string[]>([]);
/** 当前停在历史里的第几项。-1 = 还没访问过任何文件 */
const historyIndex = ref(-1);

const canGoBack = computed(() => historyIndex.value > 0);
const canGoForward = computed(() => historyIndex.value < visitHistory.value.length - 1);

/** 用户**主动**打开文件时记一笔。历史导航本身不走这里，否则历史会自己滚起来 */
function recordVisit(path: string) {
  // 停在历史中间时打开新文件 ⇒ 截掉后面的「未来」
  visitHistory.value = visitHistory.value.slice(0, historyIndex.value + 1);
  // 连续打开同一个文件不重复记
  if (visitHistory.value[visitHistory.value.length - 1] !== path) {
    visitHistory.value.push(path);
  }
  historyIndex.value = visitHistory.value.length - 1;
}

function navigateHistory(delta: number) {
  const next = historyIndex.value + delta;
  if (next < 0 || next >= visitHistory.value.length) return;

  const path = visitHistory.value[next];
  // 未命名文档关掉之后 model 就销毁了，历史里那条已经没意义 —— 跳过。
  // ⚠ 必须在移动 index **之前**判，不然后退键按下去会「什么都不发生但位置变了」
  if (isUntitledPath(path) && !models.has(path)) return;

  historyIndex.value = next;
  // ⚠ 这里不能调 openFile —— 它会再记一笔历史，于是前进后退再也动不了
  if (!openTabs.value.includes(path)) openTabs.value.push(path);
  activeTabPath.value = path;
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
  // 新建文件也进导航历史 —— VS Code 里它同样是编辑器里的一个位置
  recordVisit(path);
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

/**
 * 侧栏顶部那一行的文字。
 *
 * ★ 资源管理器特殊：它显示的是**当前文件夹名**（拿 sidebarTitle）而不是视图名。
 *   其余视图直接用它自己的 label
 *   ⚠ 别再回到「三元表达式」的写法：两个视图时还能看，
 *     三个以上就没法维护了
 */
const sidebarHeading = computed(() => {
  if (activeView.value === "explorer") return sidebarTitle.value;
  return ACTIVITY_VIEWS.find((view) => view.id === activeView.value)?.label ?? "";
});

// provide 出去之后，任意深度的后代都能 inject 到，不需要逐层 emit 转发
// 展开状态提升到这一层，而不是留在 FileTreeNode 里。
// ★ 用 ref 包 Set：Vue 会给它套一层响应式代理，所以 `.add()` / `.delete()`
//   都能被追踪（而且只有真正看了这个 key 的节点会重算）。
//   不用 shallowRef + 换新 Set —— 那样每次展开都会让全树的 computed 重算
const expandedPaths = ref<Set<string>>(new Set());

function toggleExpanded(path: string) {
  if (expandedPaths.value.has(path)) expandedPaths.value.delete(path);
  else expandedPaths.value.add(path);
}

provide(fileTreeSelectionKey, { activePath: activeTabPath, openFile });
provide(fileTreeExpansionKey, { expandedPaths, toggle: toggleExpanded });

const editorContainer = ref<HTMLElement | null>(null);

// ---------- 改动审阅（diff 浮层）----------
//
// ★ 浮层只负责「显示」；真正写盘发生在 `acceptChange` 里（agentChanges.ts）。
//   这里做两件事：把当前审阅的那条渲染成并排 diff、写完之后同步编辑器里的文档
//
// ★★ 这个浮层现在有两种**来源**，但只有一套渲染代码：
//    · `review`   —— Topilot 提议的改动待审，底下有「保留 / 撤销」两个按钮
//    · `readonly` —— 比如源代码管理里点一个文件看 diff，纯看，没有按钮
//    ⇒ 所以 `viewingDiff` 是个 computed：**两个来源折算成同一种形状**
//      （路径 + 原内容 + 新内容 + 模式），模板只认这个形状。
//      不这么做的话，「显示 diff」这件事会被抄成两套，两边迟早不一致

/** 浮层要显示的东西 —— 两种来源共用这一种形状 */
interface ViewingDiff {
  path: string;
  original: string;
  updated: string;
  /** review = 待审（能保留/撤销）；readonly = 只是看 */
  mode: "review" | "readonly";
  /** 只在 review 模式下有：对应 agentChanges 里那条改动 */
  changeId?: string;
}

const diffContainer = ref<HTMLElement | null>(null);

// 这两个是**模块级**的响应式状态，直接拿引用 —— 所以不用 provide/inject
const reviewingId = reviewingChange();
const pending = pendingChanges();

/** 「只读 diff」的来源 —— 源代码管理点文件时填进来（不填就是 null） */
const externalDiff = ref<{ path: string; original: string; updated: string } | null>(null);

/** 当前浮层要显示什么（没在显示就是 null）。两个来源的优先级：审阅 > 只读 */
const viewingDiff = computed<ViewingDiff | null>(() => {
  const id = reviewingId.value;
  if (id !== null) {
    const change = pending.value.find((item) => item.id === id);
    if (change) {
      return {
        path: change.path,
        original: change.original,
        updated: change.updated,
        mode: "review",
        changeId: id,
      };
    }
  }

  const external = externalDiff.value;
  if (external) return { ...external, mode: "readonly" };

  return null;
});

let diffEditor: monaco.editor.IStandaloneDiffEditor | null = null;
let diffOriginalModel: monaco.editor.ITextModel | null = null;
let diffUpdatedModel: monaco.editor.ITextModel | null = null;

/**
 * Monaco 的 model **不会**跟着 editor 一起释放，得自己关。
 * 审阅下一个文件时旧的那两个就没人用了 —— 不释放会一直堆在全局注册表里
 *
 * ⚠ 调用**时机**有讲究：必须先让编辑器换成新 model，再回头销毁旧的。
 *   反过来（先 dispose、再 setModel）Monaco 会当场报
 *   `TextModel got disposed before DiffEditorWidget model got reset` ——
 *   因为销毁的那一刻，编辑器还指着这两个 model。
 *   只在「第一个 diff → 第二个 diff」时才会触发：第一个进来时旧的是 null，没事
 */
function disposeDiffModels() {
  diffOriginalModel?.dispose();
  diffUpdatedModel?.dispose();
  diffOriginalModel = null;
  diffUpdatedModel = null;
}

watch(viewingDiff, async (diff) => {
  if (diff === null) return;

  // 浮层是 v-if 的，刚渲染出来时 DOM 还没有 —— 必须等一拍
  await nextTick();

  const host = diffContainer.value;
  if (!host) return;

  if (diffEditor === null) {
    diffEditor = monaco.editor.createDiffEditor(host, {
      // ★ 只读：这里是**审阅**，不是编辑。
      //   想改应该让 Topilot 改，否则「diff 里显示的内容」
      //   和「实际会写进去的内容」就对不上了
      readOnly: true,
      originalEditable: false,
      renderSideBySide: true,
      automaticLayout: true,
      scrollbar: MONACO_SCROLLBAR,
    });
  }

  // ★ 顺序：先建新的、换上去，最后才销毁旧的。
  //   中间不留「编辑器没有 model」的空档，也不会有「编辑器指着已销毁的 model」
  const previousOriginal = diffOriginalModel;
  const previousUpdated = diffUpdatedModel;

  diffOriginalModel = monaco.editor.createModel(diff.original, languageFromPath(diff.path));
  diffUpdatedModel = monaco.editor.createModel(diff.updated, languageFromPath(diff.path));
  diffEditor.setModel({ original: diffOriginalModel, modified: diffUpdatedModel });

  previousOriginal?.dispose();
  previousUpdated?.dispose();
});

/**
 * 写盘之后，把编辑器里已打开的那份文档同步成新内容。
 *
 * ★★ 不做这一步的后果很具体：编辑器里还是旧内容，而磁盘上已经是新的 ——
 *   用户随手一保存，就把 Topilot 的改动**盖回去了**，而且他完全不知道发生了什么
 *
 * ★ 「不脏才覆盖」是必须的判断：脏说明用户自己也改了，
 *   那种情况下悄悄冲掉他的改动比不同步还槽
 */
async function syncAfterWrite(path: string): Promise<void> {
  const model = models.get(path);
  if (!model) return;
  if (isUntitledPath(path)) return;

  // 脏就不动它
  const saved = savedVersions.get(path);
  if (saved !== undefined && model.getAlternativeVersionId() !== saved) return;

  try {
    const content = await invoke<string>("read_file", { path });
    if (content !== model.getValue()) {
      model.setValue(content);
      savedVersions.set(path, model.getAlternativeVersionId());
      markClean(path);
    }
  } catch {
    // 读不回来就算了 —— 为此弹一个错不值得
  }
}

// 登记回调：acceptChange 写完盘就会叫它
setWriteListener((path) => void syncAfterWrite(path));

/** 保留当前审阅的改动 */
async function acceptReview() {
  const id = reviewingId.value;
  if (id === null) return;
  try {
    // 写盘和同步都在 acceptChange 里（通过上面那个回调）
    await acceptChange(id);
  } catch (error) {
    flashSaveNotice(`写入失败：${String(error)}`);
  }
}

/** 撤销当前审阅的改动。磁盘上什么都没变 */
function rejectReview() {
  const id = reviewingId.value;
  if (id !== null) rejectChange(id);
}

/** 关掉浮层 —— 两种来源都要清干净，不然关掉之后它会立刻又弹回来 */
function closeReview() {
  externalDiff.value = null;
  reviewChange(null);
}

/**
 * 打开一个**只读** diff（源代码管理点文件时用）。
 *
 * ★ 不进 `openTabs`、也不碰 Monaco 主编辑器 —— 它只是一个浮层，
 *   和图片预览同一套路。所以标签栏不会多出一个「diff 标签」
 */
function showReadonlyDiff(path: string, original: string, updated: string) {
  externalDiff.value = { path, original, updated };
}

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
/** marker 变化的订阅（「问题」面板用） */
let markersSubscription: monaco.IDisposable | null = null;

// ---------- 文档模型（model）管理 ----------
//
// Monaco 里「编辑器」和「文档」是两回事：
//   editor = 视图（谁在显示）
//   model  = 文档（文本 + 语言 + 撤销栈 + 光标/滚动位置）
//
// 一个文件对应一个 model。切文件时换 model，而不是换文本 ——
// 这样每个文件的撤销栈和视图状态都能各自保留下来。
const models = new Map<string, monaco.editor.ITextModel>();

/**
 * 从一个 model 反查它的磁盘路径。
 *
 * ⚠ **只能遍历着找，不能读 `model.uri`**：我们的 model 是匿名建的
 *   （`createModel(content, language)` 没传 uri ⇒ `inmemory://model/N`），
 *   路径只存在 `models` 这张表的 key 上。
 *   之所以不建反向索引表：索引得跟着 openFile / 另存为 / 关标签 /
 *   resetWorkspace 一起维护，总有漏的地方，而漏掉的表现是
 *   「文件明明开着却读不到」，不报错。标签最多十几个，遍历不会漏
 *
 * ★ 使用者有两处：「跳转到定义」的 host，和「问题」面板
 *   （marker 只带 uri，也得反查回路径才知道是哪个文件）
 */
function pathOfModel(model: monaco.editor.ITextModel): string | null {
  for (const [path, candidate] of models) {
    if (candidate === model) return path;
  }
  return null;
}

/**
 * 内置语言服务的 defaults 对象（每套都有 modeConfiguration + setModeConfiguration）。
 *
 * ⚠ ★ 0.56 里它们挂在 **`monaco.json` / `monaco.css` / `monaco.html`** 上，
 *   而**不是**旧版的 `monaco.languages.json` —— 后者在新版里根本不存在
 *   （实测：`monaco.languages.json === undefined`）
 */
function builtinDefaultsFor(languageId: string) {
  switch (languageId) {
    case "json":
    case "jsonc":
      return monaco.json.jsonDefaults;
    case "css":
      return monaco.css.cssDefaults;
    case "scss":
      return monaco.css.scssDefaults;
    case "less":
      return monaco.css.lessDefaults;
    case "html":
      return monaco.html.htmlDefaults;
    default:
      // 没有内置服务的语言（rust / python …）本来就没什么可冲突的
      return null;
  }
}

/**
 * 把一个语言**完全交给真服务器**：关掉 Monaco 内置那套语言服务。
 *
 * ★★ 为什么非关不可：内置的 json / css / html 服务和真服务器**跑的是同一份代码**
 *   （vscode-json-languageservice 那一套），于是同一件事被做了两遍，
 *   而且两遍的结果**都会显示出来**：
 *   · 诊断 → 「问题」面板里每条错出现两次（实测：broken.json 2 行、
 *     style.css 4 行，两行的位置和消息完全一样）
 *   · 补全 → 建议列表里 `margin` / `margin-top` 各出现两遍
 *   VS Code 自己**不带**内置语言服务，只用真服务器那一套 ——
 *   所以「有真服务器就把它关掉」是往 VS Code 靠，不是特例
 *
 * ⚠ 反过来说：**没有**真服务器的语言（这台机器上的 TypeScript）绝不能动 ——
 *   内置那份是唯一的语言特性来源，关掉就什么都没有了。
 *   这正是这件事要挂在 `onServerReady` 上、而不是启动时无脑关一遍的原因
 */
function handLanguageToServer(languageId: string) {
  const defaults = builtinDefaultsFor(languageId);
  if (defaults === null) return;

  // ★ 十三个能力全部显式写 false（键名是从运行时的 modeConfiguration 里读出来的，
  //   不是拄的）。不写全的话，`setModeConfiguration` 是**整体替换**而非合并，
  //   没写的项会变成 undefined —— 效果一样是关掉，但读的人不知道这是有意的
  defaults.setModeConfiguration({
    completionItems: false,
    hovers: false,
    documentSymbols: false,
    definitions: false,
    references: false,
    documentHighlights: false,
    rename: false,
    colors: false,
    foldingRanges: false,
    diagnostics: false,
    selectionRanges: false,
    documentFormattingEdits: false,
    documentRangeFormattingEdits: false,
  });

  console.info(`[LSP] ${languageId}：内置语言服务已交出（只由真服务器负责，避免同一件事做两遍）`);
  handedOverLanguages.add(languageId);
  dropForeignMarkers();
}

/**
 * 已经交给真服务器的语言。
 *
 * ★ 用普通 Set 而不是 ref：它只在回调里读，不参与渲染
 */
const handedOverLanguages = new Set<string>();

/**
 * 把某个语言交给真服务器时，清掉**其它 owner** 的 marker。
 *
 * ★★ 为什么不能「只清一次」：内置语言服务是走 **worker** 校验的，
 *   而 worker 是异步的 —— 实测踩到的顺序是：
 *     关掉内置服务 → 清 marker（此刻还没有）→ **worker 的回复才落地** →
 *     于是「问题」面板里依旧每条出现两次，看起来就像「关了没生效」。
 *   ⇒ 所以这条规则要**持续生效**：只要这个语言已经交出去了，
 *     非 `lsp` 的 marker 就不能留。
 *
 * ⚠ 按 owner 筛而不是按名字写死：owner 叫什么名字是 Monaco 内部的事
 */
function dropForeignMarkers() {
  if (handedOverLanguages.size === 0) return;

  for (const marker of monaco.editor.getModelMarkers({})) {
    if (marker.owner === "lsp") continue;
    const model = monaco.editor.getModel(marker.resource);
    if (model === null) continue;
    // 语言直接问 model —— 比按路径查扩展名表更准（路径可能来自别的磁盘）
    if (!handedOverLanguages.has(model.getLanguageId())) continue;
    monaco.editor.setModelMarkers(model, marker.owner, []);
  }
}

// ---------- 语言特性的两个口子 ----------
//
// 补全 / 悬停 / 跳转定义这三件事在 lspFeatures.ts 里注册，但它不该知道
// `models` 这张表和编辑器实例长什么样 —— 所以还是开两个函数当窗口
//
// ★ 为什么第二个口子非有不可：跳转定义的目标在**别的文件**里时，
//   Monaco 的 standalone 版自己不会去开那个文件（见 lspFeatures 里的说明），
//   得由我们把它接到自己的标签栏 / model 体系上
const lspFeatureHost = {
  pathOfModel,

  /**
   * 打开别的文件并**等到它真的成了编辑器上的 model**，再返回编辑器。
   *
   * ⚠ 「等到」这一步不能省：Monaco 拿到编辑器之后会立刻用目标位置
   *   `setSelection`。要是那一刻编辑器上还挂着上一个 model，
   *   目标行号在新 model 里完全可能越界，而 Monaco 遇到越界会**抛异常**
   *
   * ⚠ ★ 光返回编辑器还不够：**选区得自己设**。Monaco 默认那套之所以会挪光标，
   *   是它在里面调了 `setSelection`；而我们一旦返回非 null，默认那套
   *   就再也没机会跑了（`openCodeEditor` 是「谁先返回就用谁」）。
   *   漏了这一步的症状很好认：**文件确实开了，但光标停在 1:1**
   */
  async openLocation(
    path: string,
    selection: monaco.IRange | monaco.IPosition | null,
  ): Promise<monaco.editor.ICodeEditor | null> {
    const editor = editorInstance.value;
    if (editor === null || path === "") return null;

    // 已经在标签栏里就只切过去，没有才新开一个
    if (!openTabs.value.includes(path)) openFile(path);
    else activateTab(path);

    let model: monaco.editor.ITextModel;
    try {
      model = await modelForPath(path);
    } catch {
      // 读不出来（文件没了 / 不是文本）—— 返回 null，让 Monaco 那边当无事发生
      return null;
    }

    // ★ 这里不等 activeTabPath 那个 watch：它是异步的，而我们要的是
    //   「返回之前 model 已经挂上去了」。直接挂一次最快，watch 那边随后
    //   会再设一次同一个 model，没有副作用
    if (editor.getModel() !== model) editor.setModel(model);

    // 和 Monaco 默认实现一样的两种写法：带 endLineNumber 的是范围，
    // 否则是单个位置
    if (selection !== null) {
      if ("endLineNumber" in selection) {
        editor.setSelection(selection);
        editor.revealRangeInCenter(selection, monaco.editor.ScrollType.Immediate);
      } else {
        editor.setPosition(selection);
        editor.revealPositionInCenter(selection, monaco.editor.ScrollType.Immediate);
      }
    }

    return editor;
  },
};

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
      // ★ 内容变了就同步给语言服务器，让它重算诊断。
      //   放在这里而不是「保存时」—— LSP 要的就是实时，
      //   等存盘才报错那和编译没区别了。
      //   （changeDocument 自己会挡掉「没 didOpen 过的路径」）
      changeDocument(path);

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

/** 欢迎页快捷键表里的一行 */
type WelcomeShortcut = {
  /** 逐个渲染成键帽。★ 写成数组而不是 "Ctrl+P" 一整串 ——
      这样才能把「+」画成普通文字、把每个键画成 <kbd> */
  keys: readonly string[];
  label: string;
  /**
   * ★ true = 常显；不写 = 折叠在「更多快捷键」下面。
   *
   * 为什么要有这个：这张表只会一直变长（每加一个功能就多一条），
   * 全部铺开会变成一堵墙，**最关键的那几条反而找不到了**。
   * ⇒ 常显的只留「最常用的关键操作」，其余全折叠。
   *   加新快捷键时先想清楚：它够不够格进常显那一组
   */
  primary?: boolean;
};

// 欢迎页右栏的快捷键表。
// 用数据 + v-for 生成，而不是手写一堆 HTML ——
// 以后加快捷键只要往数组里追一项，不用碰模板。
//
// ★ 只列**真实生效**的：写上去一条按了没反应的，比不写还糟。
//   每一条都要能在 handleKeydown / handlePanelShortcut / handleQuickOpenShortcut /
//   handleNavigateShortcut / handleSearchShortcut / handleSourceControlShortcut /
//   handleChatShortcut，或 Monaco 内置 action 里找到出处
//
// ★★ **以后加新功能就顺手在这里加一条** —— 快捷键、菜单项、说明三者要一起出生
const WELCOME_SHORTCUTS: readonly WelcomeShortcut[] = [
  // ---- 常显：最常用的那几条 ----
  { keys: ["Ctrl", "Shift", "P"], label: "命令面板", primary: true },
  { keys: ["Ctrl", "P"], label: "转到文件", primary: true },
  { keys: ["Ctrl", "S"], label: "保存当前文件", primary: true },
  { keys: ["Ctrl", "Shift", "F"], label: "在文件夹中搜索", primary: true },
  { keys: ["Ctrl", "Alt", "I"], label: "显示 / 隐藏 Topilot", primary: true },
  { keys: ["Ctrl", "J"], label: "显示 / 隐藏面板", primary: true },

  // ---- 折叠区：下面这些默认收起 ----
  { keys: ["Ctrl", "Shift", "M"], label: "问题面板" },
  // 文件。这几条是全局的 —— 焦点在侧栏或标签栏时也生效
  { keys: ["Ctrl", "N"], label: "新建文件" },
  { keys: ["Ctrl", "Shift", "S"], label: "另存为…" },
  // 侧栏视图
  { keys: ["Ctrl", "Shift", "G"], label: "源代码管理" },
  // 导航（在标题栏那对箭头上也有）
  { keys: ["Alt", "←"], label: "后退到上一个位置" },
  { keys: ["Alt", "→"], label: "前进到下一个位置" },
  // 编辑器。这几条走 Monaco 内置 action，焦点在编辑器里才生效
  { keys: ["Ctrl", "F"], label: "在当前文件中查找" },
  { keys: ["Ctrl", "H"], label: "查找并替换" },
  { keys: ["Ctrl", "/"], label: "切换行注释" },
  // 字号
  { keys: ["Ctrl", "滚轮"], label: "缩放编辑器字号" },
  { keys: ["Ctrl", "="], label: "放大字号" },
  { keys: ["Ctrl", "-"], label: "缩小字号" },
  { keys: ["Ctrl", "0"], label: "重置字号（回到默认值）" },
];

const PRIMARY_SHORTCUTS = WELCOME_SHORTCUTS.filter((item) => item.primary);
const MORE_SHORTCUTS = WELCOME_SHORTCUTS.filter((item) => !item.primary);

/**
 * 「更多快捷键」展开没有。
 *
 * ★ 默认**收起** —— 这张表只会越来越长，默认展开等于把「一堵墙」当成默认状态
 */
const shortcutsExpanded = ref(false);

/**
 * 当前要显示的那几行。
 *
 * ★ 用「算出一份列表」而不是把 <li> 写两遍：一个 v-for 管所有情况，
 *   折叠开关只影响这份数据的长度 —— 模板里不会出现第二份一模一样的行
 */
const visibleShortcuts = computed<readonly WelcomeShortcut[]>(() =>
  shortcutsExpanded.value ? WELCOME_SHORTCUTS : PRIMARY_SHORTCUTS,
);

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
  if (existing) {
    // ★★ 已经建过 model 的文件也要**交给语言服务器一次**。
    //   最典型的来源是 hot exit：它自己就把 model 建好了（见 restoreHotExit），
    //   于是这里走的是提前返回，`openDocument` 永远轮不到 ——
    //   而语言服务器那边的规矩是「没见过 didOpen 的文档根本不管」，
    //   结果是**恢复出来的文件一条诊断都没有**，还一声不响。
    //   同一个文件重复调用是安全的：会话那边会挡掉重复的 didOpen。
    if (!isUntitledPath(path)) void openDocument(path, languageFromPath(path));
    return existing;
  }

  const content = await invoke<string>("read_file", { path });
  const model = createModelFor(path, content, languageFromPath(path));

  // 刚从磁盘读回来，内容和磁盘一致 → 这就是最初的「干净」基线
  savedVersions.set(path, model.getAlternativeVersionId());
  markClean(path);

  // 内容一变就重新判断脏状态（订阅细节见 watchDirtyState）
  watchDirtyState(path, model);

  // 交给语言服务器。★ 没装对应服务器、或这个语言没服务器时它自己就退了 ——
  //   调用方不用先问「有没有」，那反而会多一次 IPC
  if (!isUntitledPath(path)) void openDocument(path, languageFromPath(path));

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
  /**
   * 这个语法要**注入到**哪些 scope 里。空数组 = 不是注入语法。
   * ⚠ 这里是清单里的原始写法（可能带 `L:` 前缀），反转和剥前缀在 textmate.ts 里做
   */
  injectTo: string[];
}

/**
 * 启动时扫一遍本机的 VS Code 扩展，把它们贡献的代码片段（`contributes.snippets`）接成补全。
 *
 * ★ 和语法一样是**两步**：Rust 只告诉「哪个语言的片段在哪个文件」，
 *   内容由这里读 —— 一个扩展往往在 snippets/ 下摆十几个语言的文件，
 *   启动时全读进来解析一遍纯属浪费
 *
 * ★ 它和前三块（grammars / themes / injectTo）的关系：
 *   那三块管「代码长什么样」，这块管「打字时弹什么」——
 *   是插件机制里第一块**交互**能力，而不是纯展示
 */
interface SnippetEntry {
  /** 这份片段适用于哪些 Monaco 语言 id */
  languages: string[];
  path: string;
  /** 来自哪个扩展 —— 片段重名时靠它分辨 */
  source: string;
}

async function loadSnippets() {
  try {
    const entries = await invoke<SnippetEntry[]>("scan_snippet_extensions");
    if (entries.length === 0) return;

    // ★ 同一份文件可能服务多个语言，所以按**路径**缓存读取结果，
    //   而不是每个语言都去读一遍盘
    const byPath = new Map<string, ParsedSnippet[]>();

    // 按语言归拢 —— 一个语言可能有好几份文件（多个扩展各贡献一份）
    const byLanguage = new Map<string, ParsedSnippet[]>();

    for (const entry of entries) {
      if (!byPath.has(entry.path)) {
        try {
          const text = await invoke<string>("read_file", { path: entry.path });
          byPath.set(entry.path, parseSnippetFile(text));
        } catch {
          // 单个文件读不了（被删、不是 UTF-8）只影响那一份
          byPath.set(entry.path, []);
        }
      }

      const parsed = byPath.get(entry.path) ?? [];

      for (const language of entry.languages) {
        const list = byLanguage.get(language) ?? [];

        for (const snippet of parsed) {
          // ★ 片段**自己**的 scope 优先于「文件声明的语言」：
          //   `"scope": "javascriptreact"` 的条目不该出现在普通 js 里。
          //   不判这一下的话，补全列表里会多出用户根本不该看到的条目
          if (snippet.languages !== null && !snippet.languages.includes(language)) continue;
          list.push(snippet);
        }

        byLanguage.set(language, list);
      }
    }

    let total = 0;
    for (const [language, snippets] of byLanguage) {
      registerSnippetCompletions(language, snippets);
      total += snippets.length;
    }

    console.info(
      `[代码片段] ${entries.length} 份文件、${total} 条片段，覆盖 ${byLanguage.size} 个语言`,
    );
  } catch (error) {
    // 和语法 / 主题一样：这一块失败不影响其它功能
    console.warn("[代码片段] 加载失败（不影响其它功能）：", error);
  }
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

    // ★ 登记「注入语法」的关系表（比如 Volar 的 vue.directives 要注到 source.vue）。
    //   不做这一步的话，注入语法永远拉不起来 ——
    //   它们不挂任何语言，平时没人会主动加载它们，只有这张表能叫醒它们。
    //   后果很具体：Vue 里的 `v-if` / `@click` 是纯白字，没有颜色
    //
    // ⚠★ 单独包一层 try：它只是「锦上添花」的一步，
    //   但它在整个大 try 里面、而且排在**降级判断之前** ——
    //   一旦它抛异常，后面「该不该退回手写表」的判断和接管循环
    //   就全都不执行了，症状是「语言从 vue 变成 html」这种莫名其妙的降级。
    //   实测踩过：前端更新了、Rust 还是旧二进制，IPC 回来的对象少一个字段就中招
    try {
      registerInjections(entries);
    } catch (error) {
      console.warn("[语法扩展] 注入关系登记失败（只影响指令上的颜色）：", error);
    }

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

  // ★ 语言服务器那边也得搬：旧路径 didClose、新路径 didOpen。
  //   只改一边的话，服务器会一直拿旧 URI 报错 —— 而那个文件已经不在了
  if (!isUntitledPath(path)) closeDocument(path);
  void openDocument(target, languageFromPath(target));

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

  // 存进当前工作区的文件要立刻出现在侧栏树里，「另存为」之后看不见它很奇怪。
  // 存到工作区外面的就不刷 —— 刷了也不会出现在树里。
  // ★ 展开状态在 App 层（见 expandedPaths），所以这里重建树不会把目录折起来
  const root = workspaceRoot.value;
  if (root && isInside(target, root)) {
    await loadTree();
  }

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

  // ★ 下面这一段只管「Ctrl + 无修饰键」的字号快捷键。
  //
  // ⚠ Ctrl+Alt+- 和 Ctrl+Shift+- 是导航快捷键（后退 / 前进），这里必须避开 ——
  //   不避的话按一次「后退」会顺手把字号也缩小一档（实测踩到过：
  //   验证导航时发现状态栏的字号从 14 变成了 12）。
  //
  // ⚠ 但不能简单写 `if (event.shiftKey) return` —— 放大用的就是 Ctrl+Shift+=（`+`），
  //   那一按的 shiftKey 也是 true。所以只排除 altKey，`-` 再单独挡一下 shift。
  if (event.altKey) return;

  // Ctrl+= / Ctrl++ 放大。
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

  if (event.key === "-" && !event.shiftKey) {
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

  // 换了工作区，原来的访问历史全部失效（那些路径已经不在当前工作区里了）
  visitHistory.value = [];
  historyIndex.value = -1;

  // 释放上一批文件文档。
  // 跳过欢迎页那个 model（编辑器正用着它），也跳过未命名文档
  for (const [key, model] of models) {
    if (key === WELCOME_KEY || isUntitledPath(key)) continue;
    // 告诉语言服务器这个文件关了 —— 不然它会一直攥着旧内容，
    // 下次打开同一个文件时报的错可能来自上一次的版本
    closeDocument(key);
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
//
// ★ MenuItem / Menu 的形状定义搬到了 types.ts ——
//   因为 MenuList.vue（递归渲染的那个组件）也要用它，
//   而 <script setup> 里不能 export 任何东西，包括类型

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
// ---------- 底部面板 ----------
//
// 就是 VS Code 底部那块：终端 / 输出 / 调试控制台。
// 标签是**数据驱动**的 —— 以后加一个面板只是在 PANEL_TABS 里添一项 +
// 在 .panel-body 里添一个分支，不用动布局

const PANEL_TABS = [
  { id: "problems", label: "问题" },
  { id: "output", label: "输出" },
  { id: "debug", label: "调试控制台" },
  { id: "terminal", label: "终端" },
] as const;

type PanelTabId = (typeof PANEL_TABS)[number]["id"];

interface LogEntry {
  level: "info" | "warn" | "error";
  text: string;
  time: string;
}

/** 日志封顶。不限的话，长时间挂着会把内存吃掉 */
const MAX_LOG_LINES = 500;

const panelVisible = ref(false);
const activePanelTab = ref<PanelTabId>("output");
const panelLogs = ref<LogEntry[]>([]);
const panelBodyRef = ref<HTMLElement | null>(null);

function togglePanel() {
  panelVisible.value = !panelVisible.value;
}

/**
 * 把 console 的日志顺带收一份到「输出」面板。
 *
 * ★ 为什么走这个路子：我们现有的日志已经到处都是 `console.info` / `console.warn`
 *   （语法扩展、主题、保存……）。拦 console 等于**一行都不用改**就全进面板了。
 *   而且照旧转发给原始 console —— 开发时 F12 里还能看到，行为不变
 */
function startLogCapture() {
  const original = { info: console.info, warn: console.warn, error: console.error };

  for (const level of ["info", "warn", "error"] as const) {
    console[level] = (...args: unknown[]) => {
      original[level](...args);

      panelLogs.value.push({
        level,
        text: args.map((item) => (typeof item === "string" ? item : String(item))).join(" "),
        time: new Date().toLocaleTimeString("zh-CN", { hour12: false }),
      });

      if (panelLogs.value.length > MAX_LOG_LINES) {
        panelLogs.value.splice(0, panelLogs.value.length - MAX_LOG_LINES);
      }
    };
  }
}

// 尽早开始拦 —— 这样后面（语法扩展 / 主题）产生的日志一条不漏
startLogCapture();

/** 日志变了就把面板滚到底 —— 不然最新的几条得自己往下拖才看得到 */
watch(panelLogs, async () => {
  if (!panelVisible.value) return;
  await nextTick();
  const el = panelBodyRef.value;
  if (el) el.scrollTop = el.scrollHeight;
});

/**
 * 面板的快捷键。
 *
 * ★ 这个用**捕获阶段**（第三个参数 true），和其它快捷键不一样：
 *   Monaco 自己把 Ctrl+J 绑成了「合并行」，冒泡阶段才监听的话根本轮不到我们。
 *   捕获是从 window 往下走，所以能先拦住
 * ★ 同时认 Ctrl+` —— VS Code 里这个也是切换面板，而且没人和它抢
 */
function handlePanelShortcut(event: KeyboardEvent) {
  if (!event.ctrlKey || event.altKey || event.shiftKey) return;

  const key = event.key.toLowerCase();
  if (key !== "j" && key !== "`") return;

  event.preventDefault();
  togglePanel();
}

// ---------- 「问题」面板 ----------
//
// ★ 数据来源是 **Monaco 自己那张 marker 表**，不是我们另存一份：
//   `lsp.ts` 把 LSP 诊断写进去、Monaco 内置 worker 也写进去 ——
//   从 markers 读就自动把所有来源都收进来了，不用挨个去接

/** 「问题」面板里的一行 */
interface ProblemRow {
  path: string;
  line: number;
  column: number;
  endLine: number;
  endColumn: number;
  severity: monaco.MarkerSeverity;
  message: string;
  /** 谁报的（json / lsp / …）。空字符串表示没写 */
  source: string;
}

const problemRows = ref<ProblemRow[]>([]);

/**
 * 三种严重程度的图标：原生 codicon 的 path 原样抄的（16×16 网格）。
 *
 * ⚠ ★ codicon 是**填充**图形（`fill="currentColor"`，没有 stroke）：
 *   图上那个「空心圆」靠的是子路径**绕向相反**，在 nonzero 填充规则下把内圈挖掉。
 *   所以模板里**不写** fill / stroke，统一由 CSS 给 `fill: currentColor` ——
 *   写成 `fill:none; stroke:…` 得到的是「粗描边轮廓图」，和原生不是一个东西
 *   （活动栏那三个图标踩过同一个坑）
 */
const PROBLEM_ICON: Record<string, string> = {
  error:
    "M8 1C4.14 1 1 4.14 1 8C1 11.86 4.14 15 8 15C11.86 15 15 11.86 15 8C15 4.14 11.86 1 8 1ZM8 14C4.691 14 2 11.309 2 8C2 4.691 4.691 2 8 2C11.309 2 14 4.691 14 8C14 11.309 11.309 14 8 14ZM10.854 5.854L8.708 8L10.854 10.146C11.049 10.341 11.049 10.658 10.854 10.853C10.756 10.951 10.628 10.999 10.5 10.999C10.372 10.999 10.244 10.95 10.146 10.853L8 8.707L5.854 10.853C5.756 10.951 5.628 10.999 5.5 10.999C5.372 10.999 5.244 10.95 5.146 10.853C4.951 10.658 4.951 10.341 5.146 10.146L7.292 8L5.146 5.854C4.951 5.659 4.951 5.342 5.146 5.147C5.341 4.952 5.658 4.952 5.853 5.147L7.999 7.293L10.145 5.147C10.34 4.952 10.657 4.952 10.852 5.147C11.047 5.342 11.047 5.659 10.852 5.854H10.854Z",
  warning:
    "M14.831 11.965L9.206 1.714C8.965 1.274 8.503 1 8 1C7.497 1 7.035 1.274 6.794 1.714L1.169 11.965C1.059 12.167 1 12.395 1 12.625C1 13.383 1.617 14 2.375 14H13.625C14.383 14 15 13.383 15 12.625C15 12.395 14.941 12.167 14.831 11.965ZM13.625 13H2.375C2.168 13 2 12.832 2 12.625C2 12.561 2.016 12.5 2.046 12.445L7.671 2.195C7.736 2.075 7.863 2 8 2C8.137 2 8.264 2.075 8.329 2.195L13.954 12.445C13.984 12.501 14 12.561 14 12.625C14 12.832 13.832 13 13.625 13ZM8.75 11.25C8.75 11.664 8.414 12 8 12C7.586 12 7.25 11.664 7.25 11.25C7.25 10.836 7.586 10.5 8 10.5C8.414 10.5 8.75 10.836 8.75 11.25ZM7.5 9V5.5C7.5 5.224 7.724 5 8 5C8.276 5 8.5 5.224 8.5 5.5V9C8.5 9.276 8.276 9.5 8 9.5C7.724 9.5 7.5 9.276 7.5 9Z",
  info:
    "M8.49902 7.49998C8.49902 7.22384 8.27517 6.99998 7.99902 6.99998C7.72288 6.99998 7.49902 7.22384 7.49902 7.49998V10.5C7.49902 10.7761 7.72288 11 7.99902 11C8.27517 11 8.49902 10.7761 8.49902 10.5V7.49998ZM8.74807 5.50001C8.74807 5.91369 8.41271 6.24905 7.99903 6.24905C7.58535 6.24905 7.25 5.91369 7.25 5.50001C7.25 5.08633 7.58535 4.75098 7.99903 4.75098C8.41271 4.75098 8.74807 5.08633 8.74807 5.50001ZM8 1C4.13401 1 1 4.13401 1 8C1 11.866 4.13401 15 8 15C11.866 15 15 11.866 15 8C15 4.13401 11.866 1 8 1ZM2 8C2 4.68629 4.68629 2 8 2C11.3137 2 14 4.68629 14 8C14 11.3137 11.3137 14 8 14C4.68629 14 2 11.3137 2 8Z",
  hint: "M8 1C5.419 1 2.75 2.964 2.75 6.25C2.75 8.167 3.637 9.184 4.224 9.856C4.408 10.067 4.598 10.285 4.628 10.398L5.568 13.889C5.744 14.543 6.34 14.999 7.017 14.999H8.984C9.662 14.999 10.257 14.542 10.432 13.889L11.372 10.397C11.402 10.285 11.593 10.067 11.776 9.856C12.363 9.183 13.25 8.167 13.25 6.25C13.25 3.355 10.895 1 8 1ZM9.467 13.63C9.408 13.848 9.209 14 8.984 14H7.017C6.791 14 6.593 13.848 6.534 13.63L6.095 12H9.906L9.467 13.63ZM11.022 9.199C10.741 9.522 10.497 9.802 10.407 10.137L10.175 10.999H5.826L5.594 10.138C5.503 9.801 5.26 9.522 4.977 9.199C4.43 8.572 3.75 7.792 3.75 6.25C3.75 3.59 5.911 2 8 2C10.344 2 12.25 3.907 12.25 6.25C12.25 7.792 11.569 8.572 11.022 9.199Z",
};

/** 严重程度 → 类名 / 图标名。用名字而不是内联色值：颜色归 CSS 主题管 */
function problemTone(severity: monaco.MarkerSeverity): string {
  switch (severity) {
    case monaco.MarkerSeverity.Warning:
      return "warning";
    case monaco.MarkerSeverity.Info:
      return "info";
    case monaco.MarkerSeverity.Hint:
      return "hint";
    default:
      return "error";
  }
}

/** 重排整张问题列表 */
function refreshProblems() {
  const rows: ProblemRow[] = [];

  for (const marker of monaco.editor.getModelMarkers({})) {
    // ⚠ marker 只带 **uri**，而我们的 model 是匿名建的（inmemory://model/N）——
    //   路径只能拿 model 回 `models` 表里反查（见 pathOfModel）
    const model = monaco.editor.getModel(marker.resource);
    const path = model === null ? null : pathOfModel(model);
    // 查不到的是欢迎页 / diff 浮层那些临时 model —— 它们不该算「问题」。
    // ★ 这一条顺带挡住了「同一个文件出现两遍」：diff 浮层会为同一个文件
    //   再建一对 model，而 Monaco 自己那套 worker 也会往它们身上写 marker
    if (path === null) continue;

    rows.push({
      path,
      line: marker.startLineNumber,
      column: marker.startColumn,
      endLine: marker.endLineNumber,
      endColumn: marker.endColumn,
      severity: marker.severity,
      message: marker.message,
      source: marker.source ?? "",
    });
  }

  // 排序：严重程度优先（错误在警告前面），再按文件、行列。
  // ★ 不按「发现顺序」排 —— 那个顺序跟着服务器推送的先后变，
  //   用户扫一眼列表时位置会跳来跳去
  const rank: Record<number, number> = {
    [monaco.MarkerSeverity.Error]: 0,
    [monaco.MarkerSeverity.Warning]: 1,
    [monaco.MarkerSeverity.Info]: 2,
    [monaco.MarkerSeverity.Hint]: 3,
  };
  rows.sort(
    (a, b) =>
      (rank[a.severity] ?? 4) - (rank[b.severity] ?? 4) ||
      a.path.localeCompare(b.path) ||
      a.line - b.line ||
      a.column - b.column,
  );

  problemRows.value = rows;
}

let problemRefreshTimer: ReturnType<typeof setTimeout> | null = null;

/**
 * 让重排等一等再跑。
 *
 * ⚠ 打字时诊断会**连续**更新（每敲一个字服务器都推一次），
 *   不防抖的话每一条都要重排 + 重渲染一遍列表 —— 问题多了就看得出来卡
 */
function scheduleRefreshProblems() {
  if (problemRefreshTimer !== null) clearTimeout(problemRefreshTimer);
  problemRefreshTimer = setTimeout(() => {
    problemRefreshTimer = null;
    refreshProblems();
  }, 200);
}

/**
 * 按文件分组。用「和前一条比」而不是 Map：
 * 列表已经按 path 排过序了，相邻即同组 —— 少建一张表，也保持顺序
 */
const problemGroups = computed(() => {
  const groups: Array<{ path: string; name: string; rows: ProblemRow[] }> = [];

  for (const row of problemRows.value) {
    const last = groups[groups.length - 1];
    if (last !== undefined && last.path === row.path) last.rows.push(row);
    else groups.push({ path: row.path, name: fileNameOf(row.path), rows: [row] });
  }

  return groups;
});

/** 点一条问题：跳过去，并把它标的那一段选中 */
function openProblem(row: ProblemRow) {
  revealAt({
    path: row.path,
    line: row.line,
    column: row.column,
    endLine: row.endLine,
    endColumn: row.endColumn,
  });
}

/** 打开「问题」面板 —— 快捷键和菜单共用这一个入口 */
function showProblemsPanel() {
  activePanelTab.value = "problems";
  panelVisible.value = true;
}

/**
 * Ctrl+Shift+M：打开问题面板（VS Code 就是这个键）。
 * ★ 和 handlePanelShortcut 一样用**捕获阶段** —— Monaco 那边也可能绑了它，
 *   冒泡阶段才监听就轮不到我们了
 */
function handleProblemsShortcut(event: KeyboardEvent) {
  if (!event.ctrlKey || !event.shiftKey || event.altKey) return;
  if (event.key.toLowerCase() !== "m") return;

  event.preventDefault();
  showProblemsPanel();
}

// ---------- 全文搜索 ----------
//
// 数据流：输入框（防抖 300ms）→ invoke("search_in_folder")
//         → Rust 遍历整个文件夹并匹配 → 回填结果列表

const searchQuery = ref("");
const searchCaseSensitive = ref(false);
/** 搜索框的 DOM 引用 —— 用快捷键打开搜索视图后要把焦点送进去 */
const searchInputRef = ref<HTMLInputElement | null>(null);
const searchResults = ref<SearchMatch[]>([]);
/** 结果是不是被上限截断了 —— 得告诉用户，不然会以为「就这么多」 */
const searchTruncated = ref(false);
/** 实际扫了多少文件 —— 给用户一个「它确实在干活」的反馈 */
const searchFilesScanned = ref(0);
const searchBusy = ref(false);
const searchError = ref<string | null>(null);
/** 是否搜过至少一次：用来区分「还没搜」和「搜了但没结果」——这两种该说不同的话 */
const searchDone = ref(false);
/** 被折叠起来的文件组 */
const collapsedSearchFiles = ref<Set<string>>(new Set());

let searchTimer: number | undefined;
/**
 * 请求序号 —— 解决「先发的请求后返回」这种竞态。
 *
 * ★ 不能省。搜索是异步的：快速改关键词时，前一次搜索很可能**后**返回。
 *   不做判断的话，列表里显示的会是上一个关键词的结果，和输入框里的字对不上 ——
 *   用户看起来就是「搜索坏了」
 */
let searchSeq = 0;

/**
 * 待跳转的位置。
 *
 * ★ 为什么不能「打开文件后直接定位」：
 *   给编辑器换 model 发生在 `watch(activeTabPath)` 里，而那个 watch 是**异步**的
 *   （内部要 `await modelForPath` 读磁盘）。在 `openFile()` 后面直接设光标，
 *   此刻编辑器挂的还是**上一个**文件 —— 光标会设错地方。
 *   而且 `editor.setModel()` 本身就会把光标重置掉。
 *   所以只能把目的地存下来，由那个 watch 在真正 setModel 之后来消费。
 */
const pendingReveal = ref<{ path: string; line: number; column: number; endLine?: number; endColumn?: number } | null>(null);

/** 真正把光标 / 选区放过去。调用方必须保证 model 已经挂上了 */
function applyPendingReveal(path: string) {
  const editor = editorInstance.value;
  const pending = pendingReveal.value;
  if (editor === null || pending === null || pending.path !== path) return;
  pendingReveal.value = null;

  editor.revealLineInCenter(pending.line);
  if (pending.endLine === undefined || pending.endColumn === undefined) {
    editor.setPosition({ lineNumber: pending.line, column: pending.column });
  } else {
    // 有范围就选中它 —— 点问题列表那一条要的就是「把出错的那段框起来」
    editor.setSelection({
      startLineNumber: pending.line,
      startColumn: pending.column,
      endLineNumber: pending.endLine,
      endColumn: pending.endColumn,
    });
  }
  editor.focus();
}

/**
 * 跳到一个位置（必要时先打开文件）。
 *
 * ⚠ ★ 不能只是「登记 pendingReveal 再 openFile」：目标文件**已经就是当前标签**时
 *   `activeTabPath` 没有变化 ⇒ 那个 watch 根本不会跑 ⇒ 跳转被静默丢掉。
 *   （实测：点同一文件里的第二条问题，光标一动不动）
 *   ⇒ 所以先把 cursor 登记好，再看「文件是不是已经开着而且 model 已经挂上」：
 *     是就自己来，不是就交给 watch
 */
function revealAt(target: { path: string; line: number; column: number; endLine?: number; endColumn?: number }) {
  pendingReveal.value = target;

  const editor = editorInstance.value;
  const model = editor?.getModel() ?? null;
  const current = model === null ? null : pathOfModel(model);
  if (current !== null && samePath(current, target.path)) {
    applyPendingReveal(target.path);
    return;
  }

  // 文件没开、或者还挂着别的 model（watch 内部 await 过，还没跑完）——
  // 交给 watch，它会在 setModel 之后消费掉 pendingReveal
  openFile(target.path);
}

async function runSearch() {
  const root = workspaceRoot.value;
  const query = searchQuery.value.trim();

  if (!root || !query) {
    searchResults.value = [];
    searchTruncated.value = false;
    searchFilesScanned.value = 0;
    searchDone.value = false;
    searchError.value = null;
    return;
  }

  const seq = ++searchSeq;
  searchBusy.value = true;
  searchError.value = null;

  try {
    const res = await invoke<SearchResponse>("search_in_folder", {
      path: root,
      query,
      caseSensitive: searchCaseSensitive.value,
    });

    if (seq !== searchSeq) return; // 已经有更新的搜索在跑了，这份结果作废

    searchResults.value = res.matches;
    searchTruncated.value = res.truncated;
    searchFilesScanned.value = res.filesScanned;
    searchDone.value = true;
  } catch (err) {
    if (seq !== searchSeq) return;
    searchError.value = String(err);
    searchResults.value = [];
    searchDone.value = true;
  } finally {
    if (seq === searchSeq) searchBusy.value = false;
  }
}

// 关键词和「区分大小写」一变就重搜。
// ★ 防抖不只是「少跑几次」—— 搜索要读一堆文件，
//   每敲一个字就全盘扫一遍的话，敲十个字就是十遍全盘 IO
watch([searchQuery, searchCaseSensitive], () => {
  window.clearTimeout(searchTimer);
  searchTimer = window.setTimeout(() => void runSearch(), 300);
});

/** 按文件分组 —— 结果列表是「文件 → 它里面的若干行」，不是一长条平的 */
const searchGroups = computed(() => {
  const byFile = new Map<string, SearchMatch[]>();

  for (const match of searchResults.value) {
    const list = byFile.get(match.path);
    if (list) list.push(match);
    else byFile.set(match.path, [match]);
  }

  // Map 保持插入顺序，所以文件顺序 = Rust 的遍历顺序，是稳定的
  return [...byFile.entries()].map(([path, items]) => ({
    path,
    name: fileNameOf(path),
    dir: workspaceRoot.value ? relativePath(path, workspaceRoot.value).replace(/[^/\\]+$/, "") : "",
    items,
  }));
});

/** 结果摘要，显示在输入框下面 */
const searchSummary = computed(() => {
  if (searchError.value) return "搜索失败";
  if (searchBusy.value) return "搜索中…";
  if (!searchDone.value) return "";
  if (searchResults.value.length === 0) {
    return `在 ${searchFilesScanned.value} 个文件里没找到结果`;
  }
  const tail = searchTruncated.value ? "（已截断，只显示前一批）" : "";
  return `${searchResults.value.length} 条结果 · ${searchGroups.value.length} 个文件${tail}`;
});

function toggleSearchGroup(path: string) {
  if (collapsedSearchFiles.value.has(path)) collapsedSearchFiles.value.delete(path);
  else collapsedSearchFiles.value.add(path);
}

/** 点一条搜索结果：打开那个文件，并把光标放到匹配处 */
function openSearchResult(match: SearchMatch) {
  // ★ 走 revealAt 而不是自己登记 + openFile：
  //   目标文件已经就是当前标签时，openFile 不会触发那个 watch，
  //   跳转会被静默丢掉（点同一文件里的第二条结果最明显）
  revealAt({ path: match.path, line: match.line, column: match.column });
}

/** 打开搜索视图并把焦点送进输入框 —— 快捷键和菜单共用这一个入口 */
function showSearchView() {
  activeView.value = "search";
  sidebarVisible.value = true;
  // 展开搜索视图后把焦点送进输入框，用户可以立刻打字
  void nextTick(() => searchInputRef.value?.focus());
}

/**
 * 搜索快捷键 Ctrl+Shift+F（VS Code 里就是「在文件中查找」）。
 * 和面板/命令面板一样走**捕获阶段** —— Monaco 和 WebView2 都可能有自己的绑定
 */
function handleSearchShortcut(event: KeyboardEvent) {
  if (!event.ctrlKey || event.altKey || !event.shiftKey) return;
  if (event.key.toLowerCase() !== "f") return;

  event.preventDefault();
  showSearchView();
}

// ---------- 源代码管理 ----------
//
// 数据流：切到工作区 → 问 Rust `git_status` → 列表；
//         点一条 → 问 Rust `git_show_head`（HEAD 版本）+ `read_file`（磁盘版本）
//                → 塞进 `externalDiff` → 复用那个 diff 浮层
//
// ★ 全程**不引第三方 git 库**：Rust 侧就是跑 `git` 命令。
//   VS Code 自己也是这么干的（它内部调 git 可执行文件）——
//   引 libgit2 那类绑定要多背一个 C 库，还得处理证书 / 配置兼容，不值当

/**
 * ⚠ 这两个接口的字段名是 **snake_case**，和 `git_status` 的 Rust 结构体一一对应。
 *
 * Rust 侧没写 `#[serde(rename_all = "camelCase")]`，所以序列化出来的就是字段原名。
 * 平时 Tauri 会自动把**命令参数**从 camelCase 转成 snake_case，
 * 但**返回值**它不管 —— 那边序列化成什么，这边就得照着写。
 * 写错了不会报错，只会拿到 `undefined`（列表一个文件都没有，看着像「没有改动」）
 */
interface GitChange {
  path: string;
  relative: string;
  status: string;
  label: string;
}

interface GitStatus {
  is_repo: boolean;
  root: string;
  changes: GitChange[];
}

const gitChanges = ref<GitChange[]>([]);
const gitRoot = ref("");
const gitIsRepo = ref(false);
const gitError = ref("");
const gitBusy = ref(false);
/**
 * 有没有**问过** git。
 *
 * ★ 和 `gitIsRepo` 是两件事：刚开始也是 `isRepo: false`，
 *   但那时的正确文案是「正在检查…」而不是「这个文件夹不是 git 仓库」。
 *   用一个布尔表达两个状态，界面上就会先闪一句错的
 */
const gitChecked = ref(false);

/** 问一遍 git。切工作区、点刷新按钮都走这一个入口 */
async function refreshGitStatus() {
  const root = workspaceRoot.value;
  if (!root) {
    gitChanges.value = [];
    gitRoot.value = "";
    gitIsRepo.value = false;
    gitError.value = "";
    gitChecked.value = false;
    return;
  }

  gitBusy.value = true;
  try {
    const status = await invoke<GitStatus>("git_status", { path: root });
    gitIsRepo.value = status.is_repo;
    gitRoot.value = status.root;
    gitChanges.value = status.changes;
    gitError.value = "";
  } catch (error) {
    // 「不是仓库」不会走到这里（Rust 侧把它当成正常结果返回），
    // 能走到这儿的是真失败：git 没装、命令报错、IPC 挂了
    gitIsRepo.value = false;
    gitRoot.value = "";
    gitChanges.value = [];
    gitError.value = String(error);
  } finally {
    gitBusy.value = false;
    gitChecked.value = true;
  }
}

/**
 * 列表右边那个字母标记。VS Code 用的是**工作区那一列**的状态码。
 *
 * ★ `git status` 给的两位码分别是「暂存区」和「工作区」（` M` / `M ` / `MM`）——
 *   VS Code 列表里显示的是第二列，因为那才是「用户眼前这份文件是什么状态」。
 *   只有第二列是空格时才退回第一列（比如 `M ` 已经 `git add` 了，
 *   VS Code 依然显示 M —— 因为它关心的是「相对 HEAD 有改动」）
 */
function gitBadge(status: string): string {
  // 未跟踪是特例：`??` 两位都是问号，VS Code 显示成 U 而不是 ?
  if (status === "??") return "U";

  const worktree = status[1];
  if (worktree && worktree !== " " && worktree !== "?") return worktree;

  const staged = status[0];
  return !staged || staged === "?" ? "M" : staged;
}

/** 字母标记的色调：新增 / 删除 / 其余（修改、重命名…）各一种颜色 */
function gitTone(status: string): string {
  const badge = gitBadge(status);
  if (badge === "U" || badge === "A") return "is-new";
  if (badge === "D") return "is-deleted";
  return "is-modified";
}

/**
 * 列表里每一行要显示的东西：文件名 + 所在目录 + 状态字母。
 *
 * ★ 名字和目录都从 `relative`（相对仓库根的路径）切，不用绝对路径 ——
 *   绝对路径会把中间那一长串项目目录名也显示出来，全是噪音。
 *   而且切出来的目录短了，右边才腾得出地方给状态字母
 */
const gitRows = computed(() =>
  gitChanges.value.map((change) => {
    const slash = change.relative.lastIndexOf("/");
    return {
      change,
      name: slash < 0 ? change.relative : change.relative.slice(slash + 1),
      dir: slash < 0 ? "" : change.relative.slice(0, slash),
      badge: gitBadge(change.status),
      tone: gitTone(change.status),
    };
  }),
);

/**
 * 点一条改动 → 打开只读 diff。
 *
 * ★ `updated` 取的是**磁盘**上的内容，不是编辑器里那份。
 *   这是刻意的：`git_status` 比较的就是「HEAD ↔ 磁盘」——
 *   如果 diff 拿编辑器里没保存的内容，就会出现
 *   「列表说这个文件没改，点进去却一堆差异」这种自相矛盾的画面
 *   （和 Topilot 那边「model 优先」的规则不冲突 —— 那边问的是「用户现在屏幕上是什么」，
 *     这边问的是「git 眼里改了什么」，两码事）
 */
async function openGitDiff(change: GitChange) {
  if (!gitRoot.value) return;
  try {
    // 新文件在 HEAD 里不存在，Rust 侧会回一个空串（不是报错）
    const original = await invoke<string>("git_show_head", {
      path: gitRoot.value,
      relative: change.relative,
    });
    const updated = await invoke<string>("read_file", { path: change.path });
    showReadonlyDiff(change.path, original, updated);
  } catch (error) {
    // 二进制文件、超过大小上限的文件都会走到这儿 —— 直说，别静默
    flashSaveNotice(`打不开 diff：${String(error)}`);
  }
}

/** 打开源代码管理视图。快捷键和菜单共用这一个入口 */
function showSourceControlView() {
  activeView.value = "scm";
  sidebarVisible.value = true;
  // 每次打开都刷一遍：用户在别处 `git add` 过之后回来，看到的应该是最新的
  void refreshGitStatus();
}

/** 源代码管理快捷键 Ctrl+Shift+G（VS Code 里就是它）。同样走捕获阶段 */
function handleSourceControlShortcut(event: KeyboardEvent) {
  if (!event.ctrlKey || event.altKey || !event.shiftKey) return;
  if (event.key.toLowerCase() !== "g") return;

  event.preventDefault();
  showSourceControlView();
}

// 换工作区（打开 / 关闭 / 启动时恢复）就重新问一遍 git。
// ★ 用 watch 而不是在 openFolder 里手动调 —— 设置 workspaceRoot 的地方有好几处
//   （打开文件夹、恢复上次的、关闭置空、恢复失败时清掉），
//   挨个去加调用必然漏掉一处，而漏掉的表现是「列表还是上一个仓库的改动」
watch(workspaceRoot, () => void refreshGitStatus(), { immediate: true });

/**
 * Topilot 快捷键 **Ctrl+Alt+I**。
 *
 * ★ 选它是因为 **VS Code 的 Copilot Chat 就是这个键**
 *   （`workbench.action.chat.toggle`）—— 用户的手感一致，将来接真 Copilot 也不会撞车。
 * ★ 加它的理由：Topilot 的入口只在标题栏那一个小按钮上，
 *   不开面板就完全想不到它存在。快捷键是它的主要入口
 * ⚠ 和其他几个一样走**捕获阶段**：Monaco 和 WebView2 都可能有自己的绑定。
 *   而且 `Ctrl+Alt+<字母>` 在某些布局上是 AltGr，所以这里同时认 `event.key`
 *   和 `event.code` —— 只认 key 的话，AltGr 把 `i` 变成一个符号时就失配了
 */
function handleChatShortcut(event: KeyboardEvent) {
  if (!event.ctrlKey || !event.altKey || event.shiftKey) return;
  if (event.key.toLowerCase() !== "i" && event.code !== "KeyI") return;

  event.preventDefault();
  toggleChat();
}

// ---------- 可拖拽的分隔条 ----------
//
// VS Code 的布局是「嵌套的框 + 夹在相邻框之间的分隔条」（它把那缝叫 sash）：
// 拖侧栏右边那条改宽度，拖面板上边那条改高度。
// 活动栏（ 48px）和状态栏是固定的，VS Code 也不让拖

const LAYOUT_KEY = "new_vscode:layout";

const DEFAULT_SIDEBAR_WIDTH = 240;
const MIN_SIDEBAR_WIDTH = 150;
/** 拖侧栏时至少给编辑器留这么宽，不然拖到底就找不回来了 */
const MIN_EDITOR_WIDTH = 320;

// Topilot 面板比侧栏宽得多 —— 它里面是对话 + 代码块
const DEFAULT_CHAT_WIDTH = 400;
const MIN_CHAT_WIDTH = 240;

const DEFAULT_PANEL_HEIGHT = 220;
const MIN_PANEL_HEIGHT = 100;
/** 同理：面板拖到头也要给编辑区和状态栏留位置 */
const MIN_WORKSPACE_HEIGHT = 240;

/** 读上次拖出来的尺寸。没存过 / 存坏了就回落默认值 */
function readLayout(): { sidebarWidth?: number; panelHeight?: number; chatWidth?: number } {
  try {
    const raw = window.localStorage.getItem(LAYOUT_KEY);
    if (!raw) return {};

    const parsed = JSON.parse(raw) as {
      sidebarWidth?: unknown;
      panelHeight?: unknown;
      chatWidth?: unknown;
    };
    return {
      sidebarWidth: typeof parsed.sidebarWidth === "number" ? parsed.sidebarWidth : undefined,
      panelHeight: typeof parsed.panelHeight === "number" ? parsed.panelHeight : undefined,
      chatWidth: typeof parsed.chatWidth === "number" ? parsed.chatWidth : undefined,
    };
  } catch {
    // JSON 坏了（手改过、写到一半断电）就当没存过 —— 不能让它把启动搞崩
    return {};
  }
}

const savedLayout = readLayout();
const sidebarWidth = ref(savedLayout.sidebarWidth ?? DEFAULT_SIDEBAR_WIDTH);
const panelHeight = ref(savedLayout.panelHeight ?? DEFAULT_PANEL_HEIGHT);
const chatWidth = ref(savedLayout.chatWidth ?? DEFAULT_CHAT_WIDTH);

function saveLayout() {
  window.localStorage.setItem(
    LAYOUT_KEY,
    JSON.stringify({
      sidebarWidth: sidebarWidth.value,
      panelHeight: panelHeight.value,
      chatWidth: chatWidth.value,
    }),
  );
}

/**
 * 拖一条分隔条。
 *
 * ★ 关键在 `setPointerCapture`：把指针「锁」在这个元素上。
 *   不锁的话，鼠标一移出那条 4px 宽的细缝，pointermove 就收不到了 ——
 *   拖动会「断」，手还得追着那条缝跑。
 *   锁上之后所有 pointer 事件都送到这个元素，代码反而更简单
 *   （同一类坑在图片拖动平移那里也踩过，那次是被浏览器的原生图片拖拽抢走了）
 */
function startSashDrag(
  event: PointerEvent,
  options: {
    axis: "x" | "y";
    get: () => number;
    set: (value: number) => void;
    min: number;
    /** 上限和窗口尺寸有关，得现算 */
    max: () => number;
    /** 尺寸变大是不是往负方向拖（面板的 sash 在面板上面，就是这种情况） */
    invert?: boolean;
  },
) {
  const target = event.currentTarget as HTMLElement;
  const startPointer = options.axis === "x" ? event.clientX : event.clientY;
  const startSize = options.get();

  target.setPointerCapture(event.pointerId);

  const onMove = (moveEvent: PointerEvent) => {
    const current = options.axis === "x" ? moveEvent.clientX : moveEvent.clientY;
    let delta = current - startPointer;
    if (options.invert) delta = -delta;

    // ★ 先算上限，而且**上限至少不能低于下限** ——
    //   窗口很窄时 `max()` 会比 `min` 还小（比如「侧栏最小 150，
    //   但至少要给编辑器留 320」在 427px 宽的窗口里就是矛盾的），
    //   那样「夹一下」反而会把尺寸压到下限以下，出来一个荒唐值
    const max = Math.max(options.min, options.max());

    // 夹进 [min, max]。不夹的话拖过头会「飘」出去，松手时尺寸就荒唐了
    options.set(Math.min(max, Math.max(options.min, startSize + delta)));
  };

  const onEnd = () => {
    target.removeEventListener("pointermove", onMove);
    target.removeEventListener("pointerup", onEnd);
    target.removeEventListener("pointercancel", onEnd);
    target.releasePointerCapture(event.pointerId);
    saveLayout();
  };

  target.addEventListener("pointermove", onMove);
  target.addEventListener("pointerup", onEnd);
  target.addEventListener("pointercancel", onEnd);
}

function startSidebarDrag(event: PointerEvent) {
  startSashDrag(event, {
    axis: "x",
    // ★ 起点取**当前显示**的宽度（不是设定值）：窗口小的时候两者不一样，
    //   拿设定值当起点会「跳」一下
    get: () => effectiveSidebarWidth.value,
    set: (value) => (sidebarWidth.value = value),
    min: MIN_SIDEBAR_WIDTH,
    // ★ 上限要**扣掉对面那块面板**占的宽度。
    //   两个面板各算各的话，同时开着就会把编辑器挤到 0 —— 实测 510px 窗口下
    //   workspace 的宽度真的变成了 0
    max: () => window.innerWidth - MIN_EDITOR_WIDTH - (chatVisible.value ? chatWidth.value : 0),
  });
}

function startPanelDrag(event: PointerEvent) {
  startSashDrag(event, {
    axis: "y",
    // sash 在面板**上方**：往上拖 = 面板变高
    invert: true,
    get: () => effectivePanelHeight.value,
    set: (value) => (panelHeight.value = value),
    min: MIN_PANEL_HEIGHT,
    max: () => window.innerHeight - MIN_WORKSPACE_HEIGHT,
  });
}

function startChatDrag(event: PointerEvent) {
  startSashDrag(event, {
    axis: "x",
    // ★ 和侧栏相反：Topilot 的 sash 在它**左边**，所以往左拖 = 面板变宽
    invert: true,
    get: () => effectiveChatWidth.value,
    set: (value) => (chatWidth.value = value),
    min: MIN_CHAT_WIDTH,
    // 同上：也要扣掉左边那块
    max: () => window.innerWidth - MIN_EDITOR_WIDTH - (sidebarVisible.value ? sidebarWidth.value : 0),
  });
}

// ---------- 面板最大化 ----------

/**
 * 面板是否最大化。
 *
 * ★ VS Code 的面板标题栏右边有个 ^ 按钮切这个，双击标签栏也行。
 *   最大化时编辑器整个让位（连拖拽条一起藏），面板独占那块区域
 */
const panelMaximized = ref(false);

function togglePanelMaximize() {
  panelMaximized.value = !panelMaximized.value;

  if (!panelMaximized.value) {
    // ★ 还原时必须让 Monaco 重算尺寸：
    //   最大化期间 .editor-area 是 display: none，Monaco 量出来是 0×0，
    //   不重算的话还原后内容会挤在左上角一小块里
    nextTick(() => editorInstance.value?.layout());
  }
}

// 窗口尺寸的响应式副本。
// ★ 为什么要存一份，而不是在 computed 里直接读 window.innerWidth：
//   原始值**不是响应式的** —— computed 读它不会建立依赖，窗口变了也不会重算
const windowWidth = ref(window.innerWidth);
const windowHeight = ref(window.innerHeight);

function syncWindowSize() {
  windowWidth.value = window.innerWidth;
  windowHeight.value = window.innerHeight;
}

window.addEventListener("resize", syncWindowSize);
onUnmounted(() => window.removeEventListener("resize", syncWindowSize));

/** 对面那块占掉多少宽度（没开就是 0） */
const sidebarTakeWidth = computed(() => (sidebarVisible.value ? sidebarWidth.value : 0));
const chatTakeWidth = computed(() => (chatVisible.value ? chatWidth.value : 0));

/**
 * 实际用哪个尺寸：**夹过**的那个。
 *
 * ★★ 关键设计：夹取只影响「显示」，**不动「用户设定」**。
 *
 *   一开始我是把夹取的结果直接写回 `sidebarWidth` / `chatWidth` 的，
 *   结果窗口一缩小，面板宽度就被**永久改小**了 —— 窗口拉回来它也不会自己变宽，
 *   用户完全不知道发生了什么（这是实测踩到的：1400 → 700 → 1400 之后，
 *   Topilot 停在了 240，回不到 400）。
 *
 *   拆成「设定值（存 localStorage）+ 显示值（算出来的）」之后，
 *   缩小是**临时**的，拉回来自动恢复
 *
 * ★ 上限还要**扣掉对面那块面板**。各自为政的话，侧栏 + Topilot 同时开着
 *   会把编辑器压到 0 —— 实测 510px 窗口下 workspace 的宽度真的是 0
 */
const effectiveSidebarWidth = computed(() =>
  Math.min(
    sidebarWidth.value,
    Math.max(MIN_SIDEBAR_WIDTH, windowWidth.value - MIN_EDITOR_WIDTH - chatTakeWidth.value),
  ),
);

const effectiveChatWidth = computed(() =>
  Math.min(
    chatWidth.value,
    Math.max(MIN_CHAT_WIDTH, windowWidth.value - MIN_EDITOR_WIDTH - sidebarTakeWidth.value),
  ),
);

const effectivePanelHeight = computed(() =>
  Math.min(
    panelHeight.value,
    Math.max(MIN_PANEL_HEIGHT, windowHeight.value - MIN_WORKSPACE_HEIGHT),
  ),
);

// ---------- 窗口标题栏（自绘的）----------
//
// ★ 因为 tauri.conf.json 里关掉了系统边框（decorations: false），
//   右上角那三个按钮是**我们自己画的** —— 行为也得自己实现。
//   下面这一组就是它们的「身子」

const isWindowMaximized = ref(false);

/** 「窗口尺寸变了」的退订函数。和 unlistenClose 一样是异步注册的，得存起来 */
let unlistenResize: (() => void) | null = null;

/** 问一下窗口现在是不是最大化（按钮图标要跟着换） */
async function refreshMaximizedState() {
  try {
    // === true 是故意的：拿不到窗口时结果是 undefined，得归成布尔值
    isWindowMaximized.value = (await appWindow?.isMaximized()) === true;
  } catch {
    // 拿不到就算了 —— 图标显示不对不影响用
  }
}

function minimizeWindow() {
  appWindow?.minimize().catch(() => {});
}

function toggleMaximizeWindow() {
  appWindow?.toggleMaximize().catch(() => {});
}

/**
 * 关闭窗口。
 *
 * ★ 必须用 `close()` 而不是 `destroy()`：`close()` 会派发 `closeRequested` 事件，
 *   于是我们已经写好的「未保存确认」照常弹出来；
 *   `destroy()` 是「不打招呼地关」，会直接跨过那一步 —— 那是真丢数据。
 *   （Tauri 内部放行时用的才是 destroy —— 它不会再触发事件，避免无限弹框）
 */
function closeWindow() {
  appWindow?.close().catch(() => {});
}

const menus = computed<Menu[]>(() => {
  // 这两条在菜单里到处要用，先算出来。它们随着「有没有活动文件 / 工作区」变
  const noFile = !activeTabPath.value;
  const noFolder = !workspaceRoot.value;

  return [
    {
      label: "文件",
      items: [
        { label: "新建文件", shortcut: "Ctrl+N", run: newUntitledFile },
        { separator: true },
        { label: "打开文件夹…", run: () => void openFolder() },
        {
          label: "打开最近",
          // 列表空了就没东西可展开 —— 灰掉比弹出一个只有「清除」的子菜单好
          disabled: recentFolders.value.length === 0,
          items: [
            ...recentFolders.value.map((path) => ({
              label: fileNameOf(path),
              // 同名文件夹很常见，所以把完整路径当次要说明显示在右边
              hint: path,
              run: () => void openRecentFolder(path),
            })),
            { separator: true },
            { label: "清除最近打开的", run: clearRecentFolders },
          ],
        },
        { label: "关闭文件夹", disabled: noFolder, run: () => void closeFolder() },
        { separator: true },
        { label: "保存", shortcut: "Ctrl+S", disabled: noFile, run: () => void saveActiveFile() },
        {
          label: "另存为…",
          shortcut: "Ctrl+Shift+S",
          disabled: noFile,
          run: () => {
            const path = activeTabPath.value;
            if (path) void saveFileAs(path);
          },
        },
        {
          label: "关闭编辑器",
          disabled: noFile,
          run: () => {
            const path = activeTabPath.value;
            if (path) void closeTab(path);
          },
        },
        { separator: true },
        {
          label: "用系统默认程序打开",
          disabled: noFile,
          run: () => void openActiveWithSystem(),
        },
        {
          label: "在文件资源管理器中显示",
          disabled: noFile,
          run: () => void revealActiveInExplorer(),
        },
        { separator: true },
        { label: "退出", run: () => void quitApp() },
      ],
    },
  {
    label: "编辑",
    items: [
      { label: "撤销", shortcut: "Ctrl+Z", disabled: noFile, run: () => runEditorAction("undo") },
      { label: "重做", shortcut: "Ctrl+Y", disabled: noFile, run: () => runEditorAction("redo") },
      { separator: true },
      {
        label: "剪切",
        shortcut: "Ctrl+X",
        disabled: noFile,
        run: () => runEditorAction("editor.action.clipboardCutAction"),
      },
      {
        label: "复制",
        shortcut: "Ctrl+C",
        disabled: noFile,
        run: () => runEditorAction("editor.action.clipboardCopyAction"),
      },
      {
        label: "粘贴",
        shortcut: "Ctrl+V",
        disabled: noFile,
        run: () => runEditorAction("editor.action.clipboardPasteAction"),
      },
      { separator: true },
      { label: "查找", shortcut: "Ctrl+F", disabled: noFile, run: () => runEditorAction("actions.find") },
      {
        label: "替换",
        shortcut: "Ctrl+H",
        disabled: noFile,
        run: () => runEditorAction("editor.action.startFindReplaceAction"),
      },
      { separator: true },
      {
        label: "切换行注释",
        shortcut: "Ctrl+/",
        disabled: noFile,
        run: () => runEditorAction("editor.action.commentLine"),
      },
    ],
  },
  {
    label: "查看",
    items: [
      { label: `切换主题（当前：${currentThemeLabel.value}）`, run: cycleTheme },
      { separator: true },
      { label: sidebarVisible.value ? "隐藏侧栏" : "显示侧栏", run: toggleSidebar },
      { separator: true },
      {
        label: chatVisible.value ? "隐藏 Topilot" : "显示 Topilot",
        run: toggleChat,
      },
      {
        label: panelVisible.value ? "隐藏面板" : "显示面板",
        shortcut: "Ctrl+J",
        run: togglePanel,
      },
      { label: "问题", shortcut: "Ctrl+Shift+M", run: showProblemsPanel },
      { separator: true },
      { label: "命令面板…", shortcut: "Ctrl+Shift+P", run: () => openQuickOpen("commands") },
      { label: "转到文件…", shortcut: "Ctrl+P", run: () => openQuickOpen("files") },
      { label: "在文件中查找", shortcut: "Ctrl+Shift+F", run: showSearchView },
      { label: "源代码管理", shortcut: "Ctrl+Shift+G", run: showSourceControlView },
      { separator: true },
      {
        label: "后退",
        shortcut: "Alt+←",
        disabled: !canGoBack.value,
        run: () => navigateHistory(-1),
      },
      {
        label: "前进",
        shortcut: "Alt+→",
        disabled: !canGoForward.value,
        run: () => navigateHistory(1),
      },
      { separator: true },
      { label: `重置编辑器字号（当前 ${currentFontSize.value}px）`, run: resetFontSize },
    ],
  },
  {
    label: "帮助",
    items: [
      {
        label: "关于 Toocode",
        run: () => flashSaveNotice("Toocode · Tauri 2 + Vue 3 + Monaco"),
      },
    ],
  },
  ];
});

// ---------- 命令面板 / 快速打开 ----------
//
// ★ 两者共用同一个浮层组件，只是数据源和选中后的动作不同：
//   命令面板 → menus 拍平；快速打开 → fileTree 拍平。
//   转换都在这里做，组件只认 `{ key, text, hint }`

type QuickOpenMode = "commands" | "files" | "context";

const quickOpenMode = ref<QuickOpenMode | null>(null);

/** 命令列表：把 menus 里所有项拍平。分隔线和禁用项不进列表 */
const commandPalette = computed(() => {
  const entries: QuickOpenEntry[] = [];
  const actions = new Map<string, () => void>();

  for (const menu of menus.value) {
    for (const item of menu.items) {
      // ★ `!item.run` 这条**顺带**把「带子菜单的项」挡在外面 ——
      //   「打开最近」有 items 但没有 run，它不是一个可执行的命令，
      //   所以正好不会被收进命令面板。
      //   ⚠ 哪天要是给它也写了 run，得回来把这里改成显式判断 item.items，
      //     否则命令面板里会出现一条「点了没反应」的项
      if (item.separator || item.disabled || !item.label || !item.run) continue;
      // key 里带上菜单名 —— 不同菜单下可能有同名项
      const key = `${menu.label} › ${item.label}`;
      entries.push({ key, text: item.label, hint: item.shortcut });
      actions.set(key, item.run);
    }
  }

  return { entries, actions };
});

/** 把文件树递归拍平成文件列表（文件夹不进列表） */
function flattenFiles(nodes: readonly FileNode[], out: FileNode[] = []): FileNode[] {
  for (const node of nodes) {
    if (node.type === "file") out.push(node);
    else if (node.children) flattenFiles(node.children, out);
  }
  return out;
}

/** 快速打开的文件列表。右侧那行灰字是相对路径 —— 同名文件只能靠它区分 */
const filePalette = computed(() => {
  const root = workspaceRoot.value;
  const byPath = new Map<string, FileNode>();

  const entries = flattenFiles(fileTree.value).map((node) => {
    byPath.set(node.path, node);
    return {
      key: node.path,
      text: node.name,
      hint: root ? relativePath(node.path, root) : undefined,
    };
  });

  return { entries, byPath };
});

const quickOpenEntries = computed<QuickOpenEntry[]>(() => {
  if (quickOpenMode.value === "commands") return commandPalette.value.entries;
  // ★ 「给 Topilot 添加上下文」用的**也是**文件列表 —— 数据源一模一样，
  //   只是选中之后干的事不同（那边打开文件，这边收进输入区）
  if (quickOpenMode.value === "files" || quickOpenMode.value === "context") {
    return filePalette.value.entries;
  }
  return [];
});

const quickOpenPlaceholder = computed(() => {
  if (quickOpenMode.value === "commands") return "输入命令名…";
  if (quickOpenMode.value === "context") return "选一个文件附加给 Topilot…";
  return "输入文件名…";
});

const quickOpenEmptyText = computed(() => {
  // 「转到文件」/「添加上下文」在没打开文件夹时永远是空的，给一句能看懂的话
  if (quickOpenMode.value !== "commands" && !workspaceRoot.value) return "还没有打开文件夹";
  return undefined;
});

function openQuickOpen(mode: QuickOpenMode) {
  // 浮层会盖住菜单，先把菜单收起来
  closeMenu();
  quickOpenMode.value = mode;
}

function closeQuickOpen() {
  quickOpenMode.value = null;
}

/**
 * Topilot 面板的引用。
 * ★ 「添加上下文」选完之后要把路径交给它 —— 见 onQuickOpenSelect 里的说明
 */
const chatPanelRef = ref<InstanceType<typeof ChatPanel> | null>(null);

function onQuickOpenSelect(key: string) {
  const mode = quickOpenMode.value;
  closeQuickOpen();

  if (mode === "commands") {
    commandPalette.value.actions.get(key)?.();
    return;
  }

  // ★ 上下文模式：**不打开文件**，而是丢进 Topilot 的输入区。
  //   这里直接调子组件暴露出来的方法，而不是再走一遍 emit ——
  //   已经有一条「ChatPanel 请求 → App 开选择器」的 emit 了，
  //   结果再绕回去就会变成「子 ➝ 父 ➝ 子」的来回传，反而难跟
  if (mode === "context") {
    chatPanelRef.value?.addContext(key);
    return;
  }

  const node = filePalette.value.byPath.get(key);
  if (node) openFile(node.path);
}

/**
 * Ctrl+Shift+P 命令面板 / Ctrl+P 快速打开。
 *
 * ⚠ 走捕获阶段：Monaco 可能把 Ctrl+P 绑成别的东西，
 *   而且 WebView2 自己也认 Ctrl+P（打印）—— 必须 preventDefault
 */
function handleQuickOpenShortcut(event: KeyboardEvent) {
  if (!event.ctrlKey || event.altKey) return;
  if (event.key.toLowerCase() !== "p") return;

  event.preventDefault();

  const wanted: QuickOpenMode = event.shiftKey ? "commands" : "files";
  // 已经是打开状态时再按同一条快捷键，就当「关掉」
  if (quickOpenMode.value === wanted) closeQuickOpen();
  else openQuickOpen(wanted);
}

/**
 * 导航快捷键。和 VS Code 一致，每个方向都绑两套：
 *
 *   Alt+← / Alt+→              和浏览器、大多数 IDE 的习惯一致
 *   Ctrl+Alt+- / Ctrl+Shift+-  VS Code 文档里写的那一对
 *
 * ⚠ 必须 preventDefault：**Alt+← / Alt+→ 是 WebView2 的「后退/前进页面」**，
 *   不拦的话整个界面会被换掉（而我们是个单页应用，退回去就什么也没有了）
 */
function handleNavigateShortcut(event: KeyboardEvent) {
  // 第一套：Alt + 左右方向键
  if (event.altKey && !event.ctrlKey && !event.shiftKey) {
    if (event.key === "ArrowLeft") {
      event.preventDefault();
      navigateHistory(-1);
    } else if (event.key === "ArrowRight") {
      event.preventDefault();
      navigateHistory(1);
    }
    return;
  }

  // 第二套：Ctrl+Alt+- 后退 / Ctrl+Shift+- 前进
  if (!event.ctrlKey || event.key !== "-") return;
  if (event.altKey && !event.shiftKey) {
    event.preventDefault();
    navigateHistory(-1);
  } else if (event.shiftKey && !event.altKey) {
    event.preventDefault();
    navigateHistory(1);
  }
}

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
  await appWindow?.destroy();
}

// ---------- 编辑器状态桥 ----------
//
// 把「用户此刻在编辑什么」交给 Agent（原理见 editorBridge.ts）。
// 这里只负责**提供数据**，怎么用是 Agent 那边的事

/** 读取用户此刻的编辑状态 */
function readEditorContext(): EditorContext {
  const path = activeTabPath.value;
  const editor = editorInstance.value;

  const context: EditorContext = {
    path,
    line: cursor.value.line,
    column: cursor.value.column,
    selection: "",
    selectionStartLine: 0,
    selectionEndLine: 0,
    truncated: false,
    dirty: path !== null && dirtyPaths.value.includes(path),
    openPaths: [...openTabs.value],
  };

  if (path === null || editor === null) return context;

  const model = editor.getModel();

  // ★ 必须确认「编辑器上挂的正是这个文件」。
  //   切换标签那个 watch 是**异步**的（内部要读磁盘），
  //   中间有一段「activeTabPath 已经变了、model 还是上一个文件」的空窗期。
  //   那时候去取选区，拿到的是**别的文件**里选中的内容 ——
  //   而且没有任何迹象表明它取错了
  if (model === null || model !== models.get(path)) return context;

  const selection = editor.getSelection();
  if (selection === null || selection.isEmpty()) return context;

  const text = model.getValueInRange(selection);
  context.selectionStartLine = selection.startLineNumber;
  context.selectionEndLine = selection.endLineNumber;

  // 截断：用户可能一口气选中整个文件，不截会把上下文直接撑爆
  if (text.length > MAX_SELECTION_CHARS) {
    context.selection = text.slice(0, MAX_SELECTION_CHARS);
    context.truncated = true;
  } else {
    context.selection = text;
  }

  return context;
}

/**
 * 按归一化路径找编辑器里那份文档。
 *
 * ★ 这里用**遍历**，而不是维护一张「归一化路径 → 原始路径」的索引表：
 *   索引表得跟着 openFile / 另存为 / 关标签 / resetWorkspace 一起更新，
 *   总有漏掉的地方 —— 而漏掉的表现是「文件明明开着却读不到」，不报错。
 *   打开的标签最多十几个，遍历的开销可以忽略，而且**不会漏**
 */
function findOpenDocument(normalized: string): OpenDocument | null {
  for (const [key, model] of models) {
    if (normalizePath(key) !== normalized) continue;
    return { text: model.getValue(), dirty: dirtyPaths.value.includes(key) };
  }
  return null;
}

onMounted(async () => {
  if (!editorContainer.value) return;//安全检查，如果容器还没准备好就退出

  window.addEventListener("keydown", handleKeydown);

  // ★ 发布版禁掉页面刷新（Ctrl+R / F5）。
  //
  // 为什么只禁发布版：开发时刷新是常用操作（改完想立刻看效果），禁掉会很难受。
  // 而发布版里它只会让用户看到一次白屏 + 所有未保存内容丢失 —— hot exit 能救回来，
  // 但那个闪一下的过程本身就是「这软件坏了」的观感。
  //
  // ⚠ 必须用捕获阶段（第三个参数 true）：刷新是 WebView2 的内建行为，
  //   冒泡阶段才拦的话它已经执行了 —— 和面板快捷键 Ctrl+J 是同一类问题
  if (import.meta.env.PROD) {
    window.addEventListener(
      "keydown",
      (event) => {
        const key = event.key.toLowerCase();
        if (event.ctrlKey && (key === "r" || key === "f5")) {
          event.preventDefault();
          flashSaveNotice("发布版已禁用页面刷新");
        }
      },
      true,
    );
  }

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

    // 滚动条：和侧栏 / 面板那套 `::-webkit-scrollbar` 对齐（见 MONACO_SCROLLBAR）
    scrollbar: MONACO_SCROLLBAR,
  });

  // 订阅「光标位置变化」，同步给状态栏
  cursorSubscription = editorInstance.value.onDidChangeCursorPosition((event) => {
    cursor.value = {
      line: event.position.lineNumber,
      column: event.position.column,
    };
  });

  // 「问题」面板跟着 marker 走。
  // ★ 用 Monaco 自己的 `onDidChangeMarkers`，而不是自己搭一套通知：
  //   不管是 LSP 推诊断、还是内置 worker 改了 marker，都会走到这里
  markersSubscription = monaco.editor.onDidChangeMarkers(() => {
    // ★ 先扫一遍「不该存在的 marker」再重排：内置 worker 是异步的，
    //   它的结果可能晚于「交出内置服务」那一刻才落地（见 dropForeignMarkers）
    dropForeignMarkers();
    scheduleRefreshProblems();
  });
  refreshProblems();

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

  // 把语言服务器客户端的两个口子接上。★ 放在恢复标签之前 ——
  // 恢复出来的标签一走 modelForPath 就会 openDocument
  configureLsp(
    {
      findModel: (path) => {
        const direct = models.get(path);
        if (direct) return direct;
        // ★★ 兜底走一遍归一化比对。为什么需要：
        //   语言服务器回传的 URI 是**归一化过**的（分隔符统一成 `/`、盘符统一大写），
        //   而 `models` 的 key 来自 Rust 的 `read_dir` —— 盘符大小写跟着用户
        //   选的文件夹走，完全可能是小写。
        //   只做精确匹配的话会 miss，症状是「诊断一条都不显示」而且**不报错**。
        //   （和 editorBridge 那边同一个理由，所以那边用的也是遍历）
        const wanted = normalizePath(path);
        for (const [key, model] of models) {
          if (normalizePath(key) === wanted) return model;
        }
        return null;
      },
      workspaceRoot: () => workspaceRoot.value,

      // ★ 服务器一握完手，就按它声明的能力给对应的语言注册补全 / 悬停 / 跳转定义。
      //   为什么得等到这一刻：`triggerCharacters` 只有服务器自己知道，
      //   而它必须写在注册那一行里
      onServerReady: (languageIds, capabilities) => {
        for (const languageId of languageIds) {
          // ★ 顺序：先把内置那套交出去（否则两套一起应答，
          //   补全列表里同一项出现两遍），再注册我们自己的
          handLanguageToServer(languageId);
          registerLspLanguage(languageId, capabilities.triggerCharacters, lspFeatureHost);
        }
        // 内置服务可能已经校验过当前打开的文件，顺手清一下它的存货
        dropForeignMarkers();
      },
    },
    (line) => console.info(line),
  );

  // 「跳到别的文件」也得有人接手 —— 见 registerLspOpenHandler 里的说明。
  // ★ 放在编辑器创建**之后**：它靠「后注册的先跑」抢在 Monaco 默认实现前面
  registerLspOpenHandler(lspFeatureHost);
  // 再把 VS Code 自带的主题接过来。它和语法扩展是两件事，各自独立失败
  await loadVscodeThemes();

  // 最后接代码片段（contributes.snippets）。
  // ★ 放在语言注册**之后**：片段是挂在语言 id 上的，
  //   语言得先存在，注册上去的补全才有地方生效
  await loadSnippets();

  // 把编辑器状态桥接给 Agent（Topilot 靠它知道用户在编辑什么）。
  // ★ 这里只是**登记函数**，不是把此刻的状态快照出去 ——
  //   状态是每秒都在变的，快照一下就固定了
  provideEditorBridge({ context: readEditorContext, document: findOpenDocument });

  // 把上次没保存完的内容接回来。
  // 放在这里（而不是放到最后）是为了：即使下面注册关闭监听失败了，恢复也已经做完
  restoreHotExit();

  // Ctrl+R 刷新 / 关窗前浏览器会派发 beforeunload ——
  // 在这里**同步**补一次备份，免得「刚敲完就刷新」丢掉最后那几百毫秒
  window.addEventListener("beforeunload", writeHotExit);
  // 面板快捷键要用捕获阶段（Monaco 把 Ctrl+J 抢走了）
  window.addEventListener("keydown", handlePanelShortcut, true);
  window.addEventListener("keydown", handleProblemsShortcut, true);

  // 命令面板 / 快速打开。同样用捕获阶段 —— 见 handleQuickOpenShortcut 上面的说明
  window.addEventListener("keydown", handleQuickOpenShortcut, true);

  // 前进后退。捕获阶段同样必要 —— Alt+← 是 WebView2 的内建后退
  window.addEventListener("keydown", handleNavigateShortcut, true);

  // 在文件中查找（Ctrl+Shift+F）。捕获阶段同样必要
  window.addEventListener("keydown", handleSearchShortcut, true);

  // 源代码管理（Ctrl+Shift+G）。同理
  window.addEventListener("keydown", handleSourceControlShortcut, true);

  // Topilot（Ctrl+Alt+I）。同理
  window.addEventListener("keydown", handleChatShortcut, true);

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
  unlistenClose = (await appWindow?.onCloseRequested(async (event) => {
    if ((await askAboutUnsaved("退出前确认")) === "cancel") {
      event.preventDefault();
    }
  })) ?? null;

  // 标题栏的「最大化 / 还原」图标得跟着窗口状态变 ——
  // 光靠点按钮不够：拖窗口边缘改大小、双击标题栏、Win+↑ 都会改变它
  unlistenResize = (await appWindow?.onResized(refreshMaximizedState)) ?? null;
  await refreshMaximizedState();

  // 恢复上次打开的文件夹。
  // 如果它已经被删了 / 读不了，openFolderPath 会把它从最近列表里剔掉，
  // 并回到「没有打开文件夹」的状态
  const lastFolder = window.localStorage.getItem(LAST_FOLDER_KEY);
  if (lastFolder) await openFolderPath(lastFolder);
});

/**
 * 退订时把异常吃掉。
 *
 * ⚠ ★★ `try/catch` 在这里**不够**：Tauri 的 `unlisten` 是 **async 函数**，
 *   里面报错会变成「被拒绝的 Promise」—— 而那是个 unhandled rejection，
 *   同步的 catch 根本接不住（实测：注了 try/catch 之后控制台照旧红）。
 *   ⇒ 所以要把返回值也当成 Promise 接一手
 *
 * ⚠ 为什么会报错：开发时 Vue 的 HMR 会走一遍 `onUnmounted`，
 *   而那一刻 `window.__TAURI_EVENT_PLUGIN_INTERNALS__` 可能已经不在了
 *   （`Cannot read properties of undefined (reading 'unregisterListener')`）。
 *   反正退订失败本来就不是大事，不该在控制台里刷屏
 */
function safeUnlisten(unlisten: (() => unknown) | null | undefined) {
  try {
    void Promise.resolve(unlisten?.()).catch(() => {});
  } catch {
    // 同步就抛的（比如 unlisten 本身是 undefined）也兜住
  }
}

onUnmounted(() => {
  window.removeEventListener("keydown", handleKeydown);
  window.removeEventListener("click", closeMenu);
  window.removeEventListener("beforeunload", writeHotExit);
  window.removeEventListener("keydown", handlePanelShortcut, true);
  window.removeEventListener("keydown", handleProblemsShortcut, true);
  window.removeEventListener("keydown", handleQuickOpenShortcut, true);
  window.removeEventListener("keydown", handleNavigateShortcut, true);
  window.removeEventListener("keydown", handleSearchShortcut, true);
  window.removeEventListener("keydown", handleSourceControlShortcut, true);
  window.removeEventListener("keydown", handleChatShortcut, true);
  // 拆掉状态桥：它抓着编辑器实例和 model，留着就是泄漏
  provideEditorBridge(null);
  // 关掉所有语言服务器进程。不关的话它们会活到系统重启
  void disposeLsp();
  safeUnlisten(unlistenClose);
  safeUnlisten(unlistenResize);
  // 定时器也是需要释放的外部资源，否则回调可能在组件销毁后才触发
  if (saveNoticeTimer) clearTimeout(saveNoticeTimer);
  if (hotExitTimer) clearTimeout(hotExitTimer);

  // 对象 URL 同理：不释放，那份图片数据会一直压在内存里
  swapPreviewUrl(null);

  // 顺序不能反：先退订事件，再销毁编辑器
  cursorSubscription?.dispose();
  fontSizeSubscription?.dispose();
  markersSubscription?.dispose();
  editorInstance.value?.dispose();

  // diff 浮层的编辑器和它的两个 model 也要收 ——
  // 它们不走 `models` 那张表（那是主编辑器用的），漏掉就是纯泄漏
  diffEditor?.dispose();
  diffEditor = null;
  disposeDiffModels();

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

  // ★ 如果这次打开是「从搜索结果跳过来」的，现在才把光标放到位。
  //   必须在这里消费，不能在 openFile() 后面 ——
  //   那里 model 还没绑上（这个 watch 内部 await 过），而且 setModel 会把光标重置掉
  const pending = pendingReveal.value;
  if (pending && pending.path === path) applyPendingReveal(path);

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
    <!-- 标题栏：因为 tauri.conf.json 里关掉了系统边框（decorations: false），
         这一整条 —— 包括右上角那三个窗口按钮 —— 都是我们画的。

         ★ 拖动区单独占中间那段空白（.titlebar-drag），不能盖住菜单或按钮：
           data-tauri-drag-region 会让里面的子元素也跟着变成拖动区，盖住就点不动了 -->
    <div class="titlebar">
      <!-- 应用图标。alt 留空是有意的：旁边的菜单和标题已经说明了这是谁。
           title 显示版本号 —— 悬停就能看到装的是哪一版 -->
      <img
        class="titlebar-icon"
        :src="appIconUrl"
        :title="`${appConf.productName} ${appConf.version}`"
        alt=""
      />

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
            <!-- 递归组件：子菜单和顶层用的是同一个渲染器。
                 它只往上冒 run 事件，不关心自己嵌套了几层 -->
            <MenuList :items="menu.items" @run="runMenuItem" />
          </div>
        </div>
      </div>

      <!-- Tauri 内置的拖动区标记：能拖窗口、双击能最大化。
           ★ 拆成左右两段 —— 中间要放命令中心，而 data-tauri-drag-region
             会让里面的子元素也变成拖动区，盖住就点不动了 -->
      <div class="titlebar-drag" data-tauri-drag-region />

      <!-- 命令中心：左边一对前进后退，右边一个「看起来像输入框」的按钮。
           ⚠ 它本身不是输入框 —— 点一下打开「快速打开」面板，真正的输入在那里
           （VS Code 也是这个行为） -->
      <div class="command-center">
        <button
          class="nav-button"
          type="button"
          title="后退"
          :disabled="!canGoBack"
          @click="navigateHistory(-1)"
        >
          <svg width="12" height="12" viewBox="0 0 16 16" aria-hidden="true">
            <path
              d="M10 3 L5 8 L10 13"
              fill="none"
              stroke="currentColor"
              stroke-width="1.3"
              stroke-linecap="round"
              stroke-linejoin="round"
            />
          </svg>
        </button>

        <button
          class="nav-button"
          type="button"
          title="前进"
          :disabled="!canGoForward"
          @click="navigateHistory(1)"
        >
          <svg width="12" height="12" viewBox="0 0 16 16" aria-hidden="true">
            <path
              d="M6 3 L11 8 L6 13"
              fill="none"
              stroke="currentColor"
              stroke-width="1.3"
              stroke-linecap="round"
              stroke-linejoin="round"
            />
          </svg>
        </button>

        <button class="command-center-box" type="button" @click="openQuickOpen('files')">
          <svg class="command-center-icon" width="12" height="12" viewBox="0 0 16 16" aria-hidden="true">
            <circle cx="7" cy="7" r="4.2" fill="none" stroke="currentColor" stroke-width="1.3" />
            <path
              d="M10.2 10.2 L14 14"
              fill="none"
              stroke="currentColor"
              stroke-width="1.3"
              stroke-linecap="round"
            />
          </svg>
          <span class="command-center-text">
            {{ activeTabPath ? fileNameOf(activeTabPath) : "搜索文件" }}
          </span>
        </button>
      </div>

      <!-- Topilot 入口：紧挨着命令中心的右边。
           ★ 为什么不放活动栏 —— 见 ACTIVITY_VIEWS 上面那段注释。
             active 态表示面板正开着，和活动栏图标「亮起来」是同一套视觉语言 -->
      <button
        class="titlebar-action"
        type="button"
        :class="{ active: chatVisible }"
        :title="chatVisible ? '隐藏 Topilot' : '显示 Topilot'"
        @click="toggleChat"
      >
        <svg
          viewBox="0 0 24 24"
          width="15"
          height="15"
          fill="none"
          stroke="currentColor"
          stroke-width="1.6"
          stroke-linecap="round"
          stroke-linejoin="round"
          aria-hidden="true"
        >
          <path :d="CHAT_ICON" />
        </svg>
      </button>

      <div class="titlebar-drag" data-tauri-drag-region />

      <!-- 三个窗口按钮。图标都是 1px 细线（stroke），和 VS Code 一样 -->
      <div class="titlebar-buttons">
        <button class="titlebar-button" type="button" title="最小化" @click="minimizeWindow">
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <path d="M0 5h10" stroke="currentColor" />
          </svg>
        </button>

        <button
          class="titlebar-button"
          type="button"
          :title="isWindowMaximized ? '向下还原' : '最大化'"
          @click="toggleMaximizeWindow"
        >
          <!-- 最大化时换成「两个叠起来的框」 -->
          <svg v-if="isWindowMaximized" width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <path d="M2.5 2.5v-2h7v7h-2" fill="none" stroke="currentColor" />
            <rect x="0.5" y="2.5" width="7" height="7" fill="none" stroke="currentColor" />
          </svg>
          <svg v-else width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <rect x="0.5" y="0.5" width="9" height="9" fill="none" stroke="currentColor" />
          </svg>
        </button>

        <button class="titlebar-button close" type="button" title="关闭" @click="closeWindow">
          <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
            <path d="M0 0l10 10M10 0l-10 10" stroke="currentColor" />
          </svg>
        </button>
      </div>
    </div>

    <div class="main"> <!-- 中间区域：横向排列-->
      <!-- 活动栏：最左边那条窄图标栏。
           图标同时承担两个作用：标识当前视图 + 点击切换侧栏显隐（和 VS Code 一致） -->
      <nav class="activitybar">
        <button
          v-for="view in ACTIVITY_VIEWS"
          :key="view.id"
          class="activity-item"
          type="button"
          :class="{ active: activeView === view.id && sidebarVisible }"
          :title="view.label"
          @click="toggleView(view.id)"
        >
          <!-- ★ 不写 fill / stroke：三个都是 codicon，而 codicon 是**填充**图形。
               统一在 CSS 的 `.activity-item svg` 里给 fill，模板里不重复三遍 -->
          <svg viewBox="0 0 24 24" width="24" height="24">
            <path :d="view.icon" />
          </svg>
        </button>
      </nav>

      <aside class="sidebar" v-show="sidebarVisible" :style="{ width: `${effectiveSidebarWidth}px` }"> <!--左侧栏-->
        <div class="sidebar-header">
          <span class="sidebar-title" :title="activeView === 'explorer' ? (workspaceRoot ?? '') : sidebarHeading">
            {{ sidebarHeading }}
          </span>
          <button
            v-if="activeView === 'explorer'"
            class="sidebar-action"
            type="button"
            title="打开文件夹"
            @click="openFolder"
          >
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

          <!-- 源代码管理：刷新按钮。
               ★ 和文件树那个「打开文件夹」同一个位置、同一套 .sidebar-action ——
                 视图不同、动作不同，但「标题右边那个操作」这个角色是一样的 -->
          <button
            v-else-if="activeView === 'scm'"
            class="sidebar-action"
            type="button"
            :disabled="gitBusy || !workspaceRoot"
            title="刷新"
            @click="refreshGitStatus"
          >
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
              <path d="M20 12a8 8 0 1 1-2.34-5.66" />
              <path d="M20 4v4h-4" />
            </svg>
          </button>
        </div>

        <!-- 树单独包一层滚动容器，否则标题栏会跟着一起滚走 -->
        <div v-if="activeView === 'explorer'" class="sidebar-tree">
          <p v-if="treeError" class="tree-error">{{ treeError }}</p>
          <p v-else-if="!workspaceRoot" class="sidebar-empty">还没有打开文件夹</p>
          <!-- 只遍历「顶层」节点；每个节点内部的子节点由 FileTreeNode 自己递归展开 -->
          <FileTreeNode v-for="node in fileTree" :key="node.path" :node="node" />
        </div>

        <!-- 搜索视图。
             和文件树共用 .sidebar-header 那一行，所以它从 header 下面开始 -->
        <div v-else-if="activeView === 'search'" class="sidebar-search">
          <div class="search-row">
            <input
              ref="searchInputRef"
              v-model="searchQuery"
              class="search-input"
              type="text"
              placeholder="搜索"
              spellcheck="false"
            />
            <button
              class="search-case"
              type="button"
              title="区分大小写"
              :class="{ active: searchCaseSensitive }"
              @click="searchCaseSensitive = !searchCaseSensitive"
            >
              Aa
            </button>
          </div>

          <p v-if="searchError" class="tree-error">{{ searchError }}</p>
          <p v-else-if="searchSummary" class="search-summary">{{ searchSummary }}</p>

          <div class="search-results">
            <p v-if="!workspaceRoot" class="sidebar-empty">还没有打开文件夹</p>
            <p v-else-if="!searchQuery.trim()" class="sidebar-empty">输入关键词开始搜索</p>

            <template v-else>
              <!-- 按文件分组：先一个文件标题，再列出它命中的每一行 -->
              <div v-for="group in searchGroups" :key="group.path" class="search-group">
                <button class="search-file" type="button" @click="toggleSearchGroup(group.path)">
                  <span class="search-caret">{{
                    collapsedSearchFiles.has(group.path) ? "▸" : "▾"
                  }}</span>
                  <span class="search-file-name">{{ group.name }}</span>
                  <span class="search-file-dir">{{ group.dir }}</span>
                  <span class="search-file-count">{{ group.items.length }}</span>
                </button>

                <div v-show="!collapsedSearchFiles.has(group.path)">
                  <button
                    v-for="(item, index) in group.items"
                    :key="`${item.line}:${item.column}:${index}`"
                    class="search-hit"
                    type="button"
                    @click="openSearchResult(item)"
                  >
                    <span class="search-hit-line">{{ item.line }}</span>
                    <!-- 高亮：直接按 start / end 切三段。
                         ★ 能这么直接用，是因为 Rust 侧给的下标就是 UTF-16 口径 ——
                           和 JS 的 slice 完全一致，中间不用任何换算 -->
                    <span class="search-hit-text"
                      ><span>{{ item.text.slice(0, item.start) }}</span
                      ><span class="search-hit-mark">{{ item.text.slice(item.start, item.end) }}</span
                      ><span>{{ item.text.slice(item.end) }}</span></span
                    >
                  </button>
                </div>
              </div>
            </template>
          </div>
        </div>

        <!-- 源代码管理视图。
             ★ 五条分支的**顺序**有讲究：先排除「没打开文件夹」，
               再排除「真出错」，最后才是三种「正常但没内容」的情况 ——
               顺序反了会出现「没打开文件夹却提示不是 git 仓库」 -->
        <div v-else-if="activeView === 'scm'" class="sidebar-scm">
          <p v-if="!workspaceRoot" class="sidebar-empty">还没有打开文件夹</p>
          <p v-else-if="gitError" class="tree-error">{{ gitError }}</p>
          <!-- ★ gitChecked 是关键：没查过时 isRepo 也是 false，
               不加这个判断就会先闪一句「不是 git 仓库」再变成文件列表 -->
          <p v-else-if="!gitChecked" class="sidebar-empty">正在检查…</p>
          <p v-else-if="!gitIsRepo" class="sidebar-empty">这个文件夹不是 git 仓库</p>
          <p v-else-if="gitRows.length === 0" class="sidebar-empty">没有改动</p>

          <div v-else class="scm-list">
            <button
              v-for="row in gitRows"
              :key="row.change.relative"
              class="scm-item"
              type="button"
              :title="`${row.change.relative} — ${row.change.label}`"
              @click="openGitDiff(row.change)"
            >
              <span class="scm-name">{{ row.name }}</span>
              <!-- 目录名要能收缩：长路径不能把右边的状态字母挤出侧栏 -->
              <span class="scm-dir">{{ row.dir }}</span>
              <span class="scm-badge" :class="row.tone">{{ row.badge }}</span>
            </button>
          </div>
        </div>

      </aside>

      <!-- 侧栏和编辑区之间的分隔条：拖它改侧栏宽度。
           ★ 它就是 VS Code 里那条「看不见但能拖」的缝 -->
      <div v-show="sidebarVisible" class="sash sash-vertical" @pointerdown="startSidebarDrag" />

      <!-- 工作区：编辑区在上、面板在下。
           ★ 面板为什么包在这里、而不是直接放 .main 里：
             VS Code 的面板**不横跨侧栏** —— 它只占编辑器下方那一块 -->
      <div class="workspace">
      <!-- 编辑区：纵向 = 标签栏在上，Monaco 在下 -->
      <!-- has-tabbar 给浮层算「要让出多少顶部高度」用，见 CSS 里 --tabbar-offset 的说明。
           必须写在 DOM 上而不是用 CSS 的 :has(.tabbar) —— 这样不依赖 :has() 的支持度 -->
      <div
        class="editor-area"
        :class="{ 'has-tabbar': openTabs.length > 0, 'is-hidden': panelMaximized }"
      >
        <!-- 一个标签都没有时（也就是欢迎页）整条藏起来 ——
             空着还占 35px，纯属浪费地方 -->
        <div v-if="openTabs.length > 0" class="tabbar">
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

        <!-- 改动审阅层：并排 diff + 保留 / 撤销。
             ★ 和图片预览同一套路 —— 盖在 Monaco 上面，定位基准是 .editor-area。
             ★ 用 Monaco 自己的 `createDiffEditor` 而不是自己画 diff：
               它本来就是干这个的，而且看到的东西和 VS Code 里一致 -->
        <div v-if="viewingDiff" class="diff-review">
          <div class="diff-review-bar">
            <span class="diff-review-path" :title="viewingDiff.path">
              {{ viewingDiff.path }}
            </span>
            <div class="diff-review-actions">
              <!-- 只有「待审」的改动才有保留 / 撤销；
                   只读 diff（比如看某个文件的改动）纯看不带按钮 -->
              <template v-if="viewingDiff.mode === 'review'">
                <button class="diff-review-accept" type="button" @click="acceptReview">
                  保留
                </button>
                <button class="diff-review-reject" type="button" @click="rejectReview">
                  撤销
                </button>
              </template>
              <button class="diff-review-close" type="button" title="关闭" @click="closeReview">
                ×
              </button>
            </div>
          </div>
          <div ref="diffContainer" class="diff-review-body"></div>
        </div>

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
            <h1 class="welcome-title">Toocode</h1>
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
                  <li v-for="folder in recentFolders" :key="folder" class="recent-item">
                    <button
                      type="button"
                      class="welcome-link recent-folder"
                      :title="folder"
                      @click="openRecentFolder(folder)"
                    >
                      <span class="recent-name">{{ fileNameOf(folder) }}</span>
                      <span class="recent-path">{{ folder }}</span>
                    </button>
                    <!-- 从列表里剔除。这个操作无破坏性（文件夹还在，只是不列在这里），
                         所以不用确认框 -->
                    <button
                      type="button"
                      class="recent-remove"
                      title="从最近列表中移除"
                      @click="forgetFolder(folder)"
                    >
                      ×
                    </button>
                  </li>
                </ul>
              </section>

              <section>
                <h2 class="welcome-heading">键盘快捷键</h2>
                <ul class="welcome-list">
                  <li v-for="item in visibleShortcuts" :key="item.label" class="welcome-row">
                    <span class="welcome-keys">
                      <template v-for="(key, index) in item.keys" :key="key">
                        <span v-if="index > 0" class="welcome-plus">+</span>
                        <kbd>{{ key }}</kbd>
                      </template>
                    </span>
                    <span class="welcome-row-label">{{ item.label }}</span>
                  </li>
                </ul>

                <!-- 折叠开关。
                     ★ 放在列表**下面**而不是上面：它管的是「下面还有多少」——
                       放在上面会把「常显那几条」和开关的关系说反
                     ★ 显示条数而不是只写「更多」：让人不用展开就知道里面有多少东西 -->
                <button
                  type="button"
                  class="welcome-more"
                  :aria-expanded="shortcutsExpanded"
                  @click="shortcutsExpanded = !shortcutsExpanded"
                >
                  <span class="welcome-more-caret">{{ shortcutsExpanded ? "▾" : "▸" }}</span>
                  {{ shortcutsExpanded ? "收起" : `更多快捷键（${MORE_SHORTCUTS.length}）` }}
                </button>
              </section>
            </div>
          </div>
        </div>
      </div>

      <!-- 编辑区和面板之间的分隔条：拖它改面板高度。
           面板最大化时藏起来 —— 那时没有「编辑区」可分了 -->
      <div
        v-if="panelVisible && !panelMaximized"
        class="sash sash-horizontal"
        @pointerdown="startPanelDrag"
      />

      <!-- 底部面板：终端 / 输出 / 调试控制台…… 和 VS Code 一样躺在编辑区下方。
           ★ 用 v-show 而不是 v-if —— 关掉面板时组件不能销毁，
             否则终端会话（xterm 实例 + PTY 子进程）会一起断掉 -->
      <div
        v-show="panelVisible"
        class="panel"
        :class="{ maximized: panelMaximized }"
        :style="panelMaximized ? undefined : { height: `${effectivePanelHeight}px` }"
      >
        <!-- 双击标签栏 = 最大化 / 还原，和 VS Code 一致 -->
        <div class="panel-tabs" @dblclick="togglePanelMaximize">
          <button
            v-for="tab in PANEL_TABS"
            :key="tab.id"
            class="panel-tab"
            type="button"
            :class="{ active: activePanelTab === tab.id }"
            @click="activePanelTab = tab.id"
          >
            {{ tab.label }}
            <!-- 问题数量做成徽标。只在有问题时出现 —— 一个永远写着的 0
                 除了占地方，还会让「现在到底有错吗」变得需要多看一眼 -->
            <span v-if="tab.id === 'problems' && problemRows.length > 0" class="panel-tab-badge">
              {{ problemRows.length }}
            </span>
          </button>

          <div class="panel-tabs-fill" />

          <!-- ⚠ .stop 不能省：不拦的话双击它会「先关闭、再最大化」 -->
          <button
            class="panel-action"
            type="button"
            :title="panelMaximized ? '还原面板大小' : '最大化面板'"
            @click="togglePanelMaximize"
            @dblclick.stop
          >
            <svg
              width="16"
              height="16"
              viewBox="0 0 16 16"
              fill="none"
              stroke="currentColor"
              stroke-width="1.1"
              stroke-linecap="round"
            >
              <path v-if="!panelMaximized" d="M4.5 9.5 L8 6 L11.5 9.5" />
              <path v-else d="M4.5 6.5 L8 10 L11.5 6.5" />
            </svg>
          </button>

          <button
            class="panel-action"
            type="button"
            title="收起面板"
            @click="togglePanel"
            @dblclick.stop
          >
            ×
          </button>
        </div>

        <div ref="panelBodyRef" class="panel-body">
          <!-- 输出：把我们自己的日志收在这里。
               内容是拦 console 得到的，所以语法扩展 / 主题 / 保存那些现成的日志都会自动进来 -->
          <div v-if="activePanelTab === 'output'" class="panel-output">
            <p v-if="panelLogs.length === 0" class="panel-empty">暂无输出</p>
            <p v-for="(log, index) in panelLogs" :key="index" class="panel-log" :class="log.level">
              <span class="panel-log-time">{{ log.time }}</span>
              <span class="panel-log-text">{{ log.text }}</span>
            </p>
          </div>

          <!-- 问题：数据来自 Monaco 的 marker 表。
               ★ 注意「同一条错可能出现两行」——Owner 不同的两份（内置 worker
                 和真语言服务器），它们本来就都是 marker，面板并不去猜谁重复了 -->
          <div v-else-if="activePanelTab === 'problems'" class="panel-problems">
            <p v-if="problemRows.length === 0" class="panel-empty">没有发现问题</p>

            <template v-else>
              <div v-for="group in problemGroups" :key="group.path" class="problem-group">
                <div class="problem-file" :title="group.path">
                  <span class="problem-file-name">{{ group.name }}</span>
                  <span class="problem-file-count">{{ group.rows.length }}</span>
                </div>

                <button
                  v-for="row in group.rows"
                  :key="`${row.line}:${row.column}:${row.source}:${row.message}`"
                  class="problem-row"
                  type="button"
                  :title="row.message"
                  @click="openProblem(row)"
                >
                  <svg
                    class="problem-icon"
                    :class="problemTone(row.severity)"
                    viewBox="0 0 16 16"
                    width="14"
                    height="14"
                    aria-hidden="true"
                  >
                    <path :d="PROBLEM_ICON[problemTone(row.severity)]" />
                  </svg>
                  <span class="problem-message">{{ row.message }}</span>
                  <span v-if="row.source !== ''" class="problem-source">{{ row.source }}</span>
                  <span class="problem-location">行 {{ row.line }}，列 {{ row.column }}</span>
                </button>
              </div>
            </template>
          </div>

          <p v-if="activePanelTab === 'debug'" class="panel-empty">调试控制台还没接上</p>

          <!-- 终端：常驻 + v-show。切走切回会话还在（详见组件里的说明） -->
          <TerminalPanel
            v-show="activePanelTab === 'terminal'"
            :visible="activePanelTab === 'terminal'"
            :cwd="workspaceRoot"
            :theme="currentThemeId"
          />
        </div>
      </div>
      </div>

      <!-- Topilot 面板。它和 .workspace **平级**，所以占的是整条高度
           （从标题栏下到状态栏上）——和左侧栏一样是「一整列的卡片」。
           ★ 为什么不管放进 .workspace：那里面是「编辑区 + 底部面板」，
             而右边这块不应该受底部面板开关的影响（VS Code 的 Copilot 侧栏也是全高） -->
      <div v-show="chatVisible" class="sash sash-vertical" @pointerdown="startChatDrag" />

      <aside v-show="chatVisible" class="chat-side" :style="{ width: `${effectiveChatWidth}px` }">
        <!-- ★ 这里不给 ChatPanel 加 v-show —— 外面这层已经管住了显隐。
             但 :visible 还是要传：它控制「第一次显示时去问密钥状态」
             ★ request-context 是「我要选个文件当上下文」的信号：
               ChatPanel 只管发信号，选择器由 App 开，选完再调它的 addContext 送回去 -->
        <ChatPanel
          ref="chatPanelRef"
          :visible="chatVisible"
          :workspace-root="workspaceRoot"
          @request-context="openQuickOpen('context')"
        />
      </aside>
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

    <!-- 命令面板 / 快速打开。放在最外层 —— 它要盖住包括标题栏在内的一切 -->
    <QuickOpen
      :visible="quickOpenMode !== null"
      :placeholder="quickOpenPlaceholder"
      :items="quickOpenEntries"
      :empty-text="quickOpenEmptyText"
      @select="onQuickOpenSelect"
      @close="closeQuickOpen"
    />
  </div>
</template>

<style scoped>
.app {
  display: flex;
  flex-direction: column;
  height: 100%;
  background: var(--color-shell-bg);
  color: var(--color-text);
}

/* ---------- 标题栏（自绘的）---------- */

/* 关掉系统边框之后，这一整条 —— 含右上角那三个窗口按钮 —— 都是我们画的 */
.titlebar {
  display: flex;
  flex: 0 0 var(--size-titlebar);
  align-items: stretch;
  background: var(--color-menubar-bg);
  font-size: 13px;
  user-select: none;
}

/* 应用图标。128×128 的原图缩到 22px 显示 —— 不用再维护第二份小图。
   ★ 22px 是标题栏（35px）里看着舒服的上限：上下各留 6.5px，再大就开始挤了 */
.titlebar-icon {
  flex: 0 0 auto;
  align-self: center;
  width: 22px;
  height: 22px;
  /* 右边留得比左边多一点：旁边就是「文件」菜单，两个不能粘在一起 */
  margin: 0 8px 0 10px;
}

/* 菜单栏现在只是标题栏里的一段，不再自己占一整行 */
.menubar {
  display: flex;
  flex: 0 0 auto;
  align-items: center;
  padding: 0 4px;
}

/* ★ 拖动区：占满菜单和按钮之间的空白。
   它的职责只有「拖窗口 + 双击最大化」，所以**不能**盖住菜单或按钮 ——
   data-tauri-drag-region 会让里面的子元素也跟着变成拖动区，盖住就点不动了。
   命令中心出现之后这条被拆成了左右两段，中间那段留给命令中心 */
.titlebar-drag {
  flex: 1 1 auto;
  min-width: 16px; /* 窗口很窄时也留一点能拖的地方 */
}

/* 命令中心：中间那组「导航箭头 + 搜索框」。
   ★ 用 flex: 0 1 auto + min-width: 0，让它能收缩 ——
     窗口变窄时先压它，而不是把两边的拖动区挤没（那样窗口就拖不动了） */
.command-center {
  display: flex;
  flex: 0 1 auto;
  align-items: center;
  gap: 2px;
  min-width: 0;
  max-width: 420px;
}

/* 窄窗口下整组藏起来 —— VS Code 也是宽度不够就不显示 */
@media (max-width: 760px) {
  .command-center {
    display: none;
  }
}

/* 标题栏上的功能按钮（目前只有 Topilot 入口）。
   ★ 比右上角那三个窗口按钮小巧一点 —— 它不是「系统级」按钮，
     混在菜单 / 命令中心这一带更协调 */
.titlebar-action {
  display: flex;
  flex: 0 0 auto;
  align-self: center;
  align-items: center;
  justify-content: center;
  width: 26px;
  /* ★ 和命令中心那个「假输入框」一样高（24px）。
     差 2px 肉眼说不出差在哪，但就是会觉得没对齐 */
  height: 24px;
  /* ★ 和命令中心隔开一点。它俩不是一组控件：
     左边是「导航 + 快速打开」，右边是「开关某一侧面板」。
     挨紧了会被当成同一组，功能上却毫无关系 */
  margin-left: 8px;
  padding: 0;
  border: 0;
  border-radius: 4px;
  background: none;
  color: var(--color-text);
  cursor: pointer;
}

.titlebar-action:hover {
  background: var(--color-titlebar-button-hover);
}

/* 面板开着时打底高亮 —— 和侧栏列表项选中用的是同一组颜色 */
.titlebar-action.active {
  background: var(--color-selection);
  color: var(--color-text-on-accent);
}

.nav-button {
  display: flex;
  flex: 0 0 auto;
  align-items: center;
  justify-content: center;
  width: 24px;
  height: 24px;
  border: 0;
  border-radius: 4px;
  background: transparent;
  color: var(--color-text);
  cursor: pointer;
  /* 箭头粗细：不可用时细线 */
  --nav-stroke: 1;
}

/* ★ 可用时把箭头加粗。
   光靠变灰/变亮区分不够明显 —— 两个箭头都那么细，扫一眼看不出哪个能按。
   粗细 + 颜色两个维度一起变，才一眼能认。
   （CSS 优先级高于 SVG 的 stroke-width 表现属性，所以能直接盖掉） */
.nav-button:not(:disabled) {
  --nav-stroke: 1.8;
}

.nav-button svg {
  stroke-width: var(--nav-stroke);
}

.nav-button:hover:not(:disabled) {
  background: var(--color-hover);
}

/* 到底了（没有可后退/前进的历史）就灰掉、点不动 */
.nav-button:disabled {
  color: var(--color-text-dim);
  cursor: default;
}

/* 那个「看起来像输入框」的按钮。点下去实际是打开快速打开面板 */
.command-center-box {
  display: flex;
  flex: 1 1 auto;
  align-items: center;
  justify-content: center;
  gap: 6px;
  min-width: 0;
  height: 24px;
  padding: 0 10px;
  border: 1px solid var(--color-menu-border);
  border-radius: 4px;
  /* 比标题栏底色深一档，看起来才像「凹进去的输入框」 */
  background: var(--color-editor-bg);
  color: var(--color-text-dim);
  font: inherit;
  font-size: 12px;
  cursor: pointer;
}

.command-center-box:hover {
  color: var(--color-text);
}

.command-center-icon {
  flex: 0 0 auto;
}

.command-center-text {
  min-width: 0;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}

.titlebar-buttons {
  display: flex;
  flex: 0 0 auto;
  align-items: stretch;
}

.titlebar-button {
  width: 46px; /* Windows 窗口按钮的标准宽度 */
  display: flex;
  align-items: center;
  justify-content: center;
  border: 0;
  background: transparent;
  color: var(--color-text);
  cursor: default; /* 窗口按钮不是链接，不用 pointer */
}

.titlebar-button:hover {
  background: var(--color-titlebar-button-hover);
}

/* 关闭按钮悬停变红 —— VS Code 在 Windows 上也是这样（省得点错） */
.titlebar-button.close:hover {
  background: #e81123;
  color: #ffffff;
}

.menu {
  position: relative;   /* 下拉面板的定位基准 */
  /* ★ 垂直居中必须在这一层做，不能指望 .menubar ——
     .menu 是 .menubar 的直接子项，但 .menu-title 是「孙子辈」，
     父级的 align-items 只管直接子元素。之前 .menu 是块级，
     按钮在里面就是 inline-block 贴顶，圆角上下露尖角。
     另外 .menu 变成 flex 后，绝对定位的 .menu-dropdown 不受影响 */
  display: flex;
  align-items: center;
  height: 100%;
}

.menu-title {
  /* 不需要 height: 100%（满高的话上、下各会露出一个尖角），
     26px 交给 .menu 的 align-items: center 居中。
     .menubar 左右那 4px padding 现在有了新用途：给首尾两项的圆角让位 */
  height: 26px;
  padding: 0 8px;
  border: 0;
  border-radius: 4px;
  background: transparent;
  color: var(--color-text);
  font: inherit;
  cursor: pointer;
}

.menu-title:hover,
.menu-title.open {
  background: var(--color-menubar-hover);
}

/* ⚠ 菜单**里面**的样式（.menu-dropdown / .menu-item / .menu-separator …）不在这里，
   它们搬去了 MenuList.vue。
   ★ 为什么必须搬：这个 <style> 是 scoped，Vue 会给选择器加上 [data-v-xxx]，
     而那个 xxx 是**本组件**的 id。MenuList 渲染出来的元素带的是它自己的 id，
     这边的规则**一条都匹配不上**（而且不会报错，只是样式静默失效）。
     scoped 是跟着「模板里的元素」走的，不跟着「组件」走
   上面那两条 .menu / .menu-title 留在这里是对的 —— 它们是菜单**栏**，
   由 App.vue 自己的模板渲染 */

/* ★ 「浮起」布局：padding 是卡片离窗口边缘的距离，background 是那个「底」——
   所有缝隙里露出的都是它。
   ⚠ 这里**不用 gap**：gap 会在**每一对**相邻 item 之间都插一次，
     而 .sash 自己也是一个 item，于是「侧栏|sash」和「sash|编辑器」各加一次，
     缝隙会变成两倍。让 .sash 自己当那条缝隙宽就行（活动栏那条缝用 margin 补） */
.main {
  display: flex;
  flex: 1;
  min-height: 0;
  padding: var(--size-gap);
  background: var(--color-shell-bg);
}

/* 工作区：编辑区和面板上下排。面板只占编辑器下方那块 ——
   不横跨侧栏，和 VS Code 一致。
   ⚠ 不加 padding：左右已经由 .main 的 padding 处理过了，
   再加一层面板就会比编辑器窄 12px */
.workspace {
  display: flex;
  flex: 1;
  flex-direction: column;
  min-width: 0;
}

/* ★ min-height: 0 是必须的：不然 Monaco 那层会把容器撑破，面板就被挤出去了 */
.workspace > .editor-area {
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
  border-radius: var(--radius-card);
  /* 裁掉伸出圆角的子元素 —— 「浮起」风格里这一步不能省：
     不裁的话子元素的直角背景会从圆角外面溢出来 */
  overflow: hidden;
  /* 活动栏和侧栏之间没有 sash（活动栏宽度固定，不可拖），
     所以那条缝只能用它自己的 margin 让出来 */
  margin-right: var(--size-gap);
}

.activity-item {
  position: relative;   /* 给下面那个 ::after 方块当定位基准 */
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

.activity-item:hover {
  color: var(--color-icon-hover);
}

/* 当前视图：图标变成强调蓝。
   ⚠ 必须写在 :hover **后面** —— 两者优先级一样，后写的赢。
     顺序反了的话，「悬停的正好是当前视图」时会退回灰色 */
.activity-item.active {
  color: var(--color-icon-active);
}

/* ★★ 活动栏的图标**全部**是 VS Code 原生 codicon，而 codicon 是「填充」图形
   （`fill="currentColor"`，没有 `stroke`），不是描边图形。

   图上那些圆之所以看着像空心环，靠的不是描边，而是子路径**绕向相反** ——
   在 nonzero 填充规则下把内圈那块**挖掉**。
   ⇒ 所以这里只能是 `fill: currentColor`：
     · 给它 `fill="none"` 是一片空白（这条路径本来就没有「描边」这回事）
     · 给它描边则得到「粗描边轮廓图」—— 圆的孔被描边填小、连接线变成粗条，
       和原生完全不是一个东西

   ★ 写在 CSS 而不是 svg 的属性上：「图标是填充的」这件事只有一个出处，
     模板里不用重复三遍。将来真要加一个描边图标，再按需开例外 */
/* ⚠ `position: relative` + `z-index: 1` 是为了**压在下面那个悬停方块上面**，不只是好看 */
.activity-item svg {
  position: relative;
  /* ★★ 这个 1 不能省，而且**不能只写 position: relative**。
     按钮里的三个东西（::before 竖线、svg、::after 悬停方块）都是「定位元素」：
     ::before / svg 是 `z-index: auto`，::after 是 `z-index: 0` ——
     而 auto 和 0 在绘制时是**同一档**，同档里按 DOM 顺序画。
     ::after 排在 svg **后面**，所以方块会盖在图标上面（截出来看就是图标发灰）。
     只有把 svg 抬到 1（真正比 0 大）才分得开。
     ★ 验证办法：把方块临时改成不透明红 `#c0392b !important` 截图 ——
       图标还能看见就说明层级对了 */
  z-index: 1;
  fill: currentColor;
}

/* ★ 悬停时图标背后浮出来的那个圆角方块。
   ---- 为什么要有 ::after 这一层，不能直接把背景刷在 .activity-item 上 ----
   因为按钮是满宽的（48×48），给它加背景得到的是「一条顶到两边的色带」，
   而不是「围着图标的一个方块」。

   ---- 尺寸和颜色是 VS Code 的 modern UI 给的，但**内缩量我们故意加大了** ----
   · 颜色 `modernActivityBarItem.hoverBackground`（见 CSS_VAR_BY_COLOR）
   · 圆角 `--vscode-cornerRadius-small` = 4px，VS Code 的边长是 `action高度 - 4`
     ⇒ 48 - 4 = 44，也就是 `inset: 2px`

   ⚠ ★ **我们改成 `inset: 6px`（36×36）+ 圆角 6px**，没有照抄那 2px。
     原因有两个，第二个是实打实的 bug：
     ① 2px 几乎是贴着活动栏两边的，看着像「一条色带被切短了」，不像一个独立的块。
        内缩到 6px、圆角放大到 6px，才有「浮起来」的感觉
     ② ★★ **活动栏自己有 `border-radius: var(--radius-card)`（8px）+ `overflow: hidden`**。
        方块贴在 2px 处时，**上面两个角会被活动栏的圆角切掉** ——
        因为活动栏那个圆角弧在 x=2 处才升到 y≈2.7，方块左上角 (2,2) 正好在弧外面。
        缩到 6px 就完全落在直边范围内了，一点都不会被切
     （想让方块更小/更大就调这个 inset；它和「活动栏圆角是 8px」是绑在一起的，
       改半径的时候记得回头看一眼这里） */
.activity-item::after {
  content: "";
  position: absolute;
  inset: 6px;
  z-index: 0;
  border-radius: 6px;
  background: transparent;
}

/* 只在**非激活**的项上亮 —— VS Code 也是这么写的（`:not(.checked):hover`）：
   当前视图有它自己那档更明显的背景（见下面一条），不用再叠悬停色 */
.activity-item:not(.active):hover::after {
  background: var(--color-icon-hover-bg);
}

/* ★ 当前视图：同一个方块，但底色**更深一档** ——
   `modernActivityBarItem.activeBackground`（深 #FFFFFF22 / 浅 #e4e6f1）
   正好是 hoverBackground 那个值的加重版。再加上图标变蓝，两个信号一起表达「你在这里」

   ★ 原来这里画的是一条 2px 的蓝竖线（`activityBar.activeBorder`）。
     换成方块之后那个键没浪费 —— 它现在给图标上色（见 --color-icon-active）。
     为什么换成方块：竖线贴在活动栏最左边，和「图标身上的方块」不是一个视觉系统，
     两个同时上反而分不清哪个才是选中 */
.activity-item.active::after {
  background: var(--color-icon-active-bg);
}

.sidebar {
  display: flex;
  /* ★ 宽度不在这里写死 —— 它可以被拖拽，由 JS 的 sidebarWidth 给 inline style。
     这里写 flex-basis 的话会盖过 width（flex-basis 优先于 width） */
  flex: 0 0 auto;
  flex-direction: column;
  background: var(--color-sidebar-bg);
  color: var(--color-text);
  border-radius: var(--radius-card);
  overflow: hidden;
}

/* Topilot 面板：右侧独立的一张卡片。
   和 .sidebar 是同一套观感，只是宽度由 chatWidth 管。
   ★ 它和左侧栏**互不相干**（两张卡片，可以同时开着）——
     所以左栏的宽窄、开关，都不影响它 */
.chat-side {
  display: flex;
  /* 同上：宽度交给 inline style，这里不能写 flex-basis */
  flex: 0 0 auto;
  flex-direction: column;
  background: var(--color-sidebar-bg);
  color: var(--color-text);
  border-radius: var(--radius-card);
  overflow: hidden;
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
/* ---------- 搜索视图 ---------- */

/* 和 .sidebar-tree 同一套职责：吃掉剩余高度，自己滚 */
.sidebar-search {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
}

.search-row {
  display: flex;
  align-items: center;
  gap: 4px;
  padding: 6px 8px 4px;
}

.search-input {
  flex: 1;
  min-width: 0;
  height: 26px;
  padding: 0 8px;
  border: 1px solid var(--color-menu-border);
  /* 底色用编辑器的那个 —— 输入框和编辑器同色，一眼能看出「往这里打字」 */
  background: var(--color-editor-bg);
  color: var(--color-text);
  font-family: inherit;
  font-size: 12px;
  outline: none;
  border-radius: 3px;
}

.search-input:focus {
  border-color: var(--color-link);
}

.search-input::placeholder {
  color: var(--color-text-dim);
}

/* 区分大小写开关。
   ★ 只有「开」的时候才亮 —— 常亮的按钮分不清现在是开还是关 */
.search-case {
  flex: 0 0 auto;
  width: 26px;
  height: 26px;
  border: 1px solid transparent;
  background: transparent;
  color: var(--color-text-dim);
  font-size: 11px;
  font-weight: 600;
  cursor: pointer;
  border-radius: 3px;
}

.search-case:hover {
  background: var(--color-hover);
}

.search-case.active {
  border-color: var(--color-link);
  background: var(--color-selection);
  color: var(--color-text-on-accent);
}

.search-summary {
  margin: 2px 12px 6px;
  color: var(--color-text-dim);
  font-size: 11px;
}

.search-results {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
}

.search-group {
  margin-bottom: 2px;
}

/* 文件标题行：箭头 + 文件名 + 目录 + 命中数 */
.search-file {
  display: flex;
  align-items: center;
  gap: 4px;
  width: 100%;
  padding: 3px 8px;
  border: none;
  background: transparent;
  color: var(--color-text);
  font-family: inherit;
  font-size: 12px;
  text-align: left;
  cursor: pointer;
  border-radius: 4px;
}

.search-file:hover {
  background: var(--color-hover);
}

.search-caret {
  flex: 0 0 auto;
  width: 10px;
  color: var(--color-text-dim);
  font-size: 10px;
}

.search-file-name {
  flex: 0 0 auto;
  font-weight: 600;
}

/* 目录名要能收缩 —— 不能收缩的话长路径会把右边的计数挤出侧栏 */
.search-file-dir {
  flex: 0 1 auto;
  min-width: 0;
  overflow: hidden;
  color: var(--color-text-dim);
  font-size: 11px;
  white-space: nowrap;
  text-overflow: ellipsis;
}

.search-file-count {
  flex: 0 0 auto;
  /* margin-left: auto 把它顶到最右边（不靠 .search-file-dir 撑开）*/
  margin-left: auto;
  padding: 0 5px;
  background: var(--color-sash-dot);
  color: var(--color-text-emphasis);
  font-size: 10px;
  border-radius: 8px;
}

/* 一行命中：左边行号，右边那行代码 */
.search-hit {
  display: flex;
  gap: 6px;
  width: 100%;
  /* 左边多缩进一点，和文件标题形成层级 */
  padding: 2px 8px 2px 22px;
  border: none;
  background: transparent;
  color: var(--color-text);
  font-family: inherit;
  font-size: 11px;
  text-align: left;
  cursor: pointer;
  border-radius: 4px;
}

.search-hit:hover {
  background: var(--color-hover);
}

.search-hit-line {
  flex: 0 0 auto;
  min-width: 26px;
  color: var(--color-text-dim);
  /* 等宽数字：行号从 1 位变 2 位时不会左右抖 */
  font-variant-numeric: tabular-nums;
  text-align: right;
}

/* ★ 代码行不换行、超出截断。
   允许换行的话文件一多，列表会长得没法看（一段代码占五六行）*/
.search-hit-text {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  white-space: pre;
  text-overflow: ellipsis;
}

/* 命中高亮：用**底色**而不是改字色。
   改字色会和「这段代码本该是什么颜色」冲突，底色只表达「这段是你要找的」 */
.search-hit-mark {
  background: var(--color-search-hit);
  border-radius: 2px;
}

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

/* ---------- 源代码管理视图 ---------- */

/* 和 .sidebar-tree / .sidebar-search 同一套职责：吃掉剩余高度，自己滚 */
.sidebar-scm {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
}

.scm-list {
  padding: 0 4px 8px;
}

/* 一行：文件名 + 目录 + 状态字母。
   ★ 结构上和 .search-file 几乎一样，但**不共用类名** ——
     搜索那边整行是「标题」，这边整行是「可点的条目」，
     将来改一边不该牵动另一边 */
.scm-item {
  display: flex;
  align-items: center;
  gap: 6px;
  width: 100%;
  padding: 3px 8px;
  border: none;
  background: transparent;
  color: var(--color-text);
  font-family: inherit;
  font-size: 12px;
  text-align: left;
  cursor: pointer;
  border-radius: 4px;
}

.scm-item:hover {
  background: var(--color-hover);
}

/* 文件名不收缩也不截断？—— 要截断。超长文件名得让位给目录和状态字母 */
.scm-name {
  flex: 0 1 auto;
  min-width: 0;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}

/* 目录能收缩，而且比文件名更愿意让位（flex-shrink 默认都是 1，
   但文件名有 min-width: 0，谁先缩看谁的基础宽度大，不用额外设权重）*/
.scm-dir {
  flex: 0 1 auto;
  min-width: 0;
  overflow: hidden;
  color: var(--color-text-dim);
  font-size: 11px;
  white-space: nowrap;
  text-overflow: ellipsis;
}

/* 状态字母顶到最右边 —— margin-left: auto 比靠前面的元素撑开可靠 */
.scm-badge {
  flex: 0 0 auto;
  margin-left: auto;
  min-width: 12px;
  font-size: 11px;
  font-weight: 600;
  text-align: right;
}

.scm-badge.is-modified {
  color: var(--color-git-modified);
}

.scm-badge.is-new {
  color: var(--color-git-new);
}

.scm-badge.is-deleted {
  color: var(--color-git-deleted);
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

  /* 浮层（欢迎页 / 图片预览）要让出的顶部高度 —— 它们不能盖住标签栏。
     ⚠ 不能直接写死 var(--size-tabbar)：标签栏是 v-if 的，**没标签时它整个不渲染**，
       .editor 就占满整个 .editor-area（从 0 开始），而浮层还从 35px 起
       ⇒ 顶上那 35px 会露出一条 Monaco 的内容，看起来像「两个界面重叠了」。
     ⇒ 让这个值跟着「标签栏到底在不在」走（.has-tabbar 由模板的 :class 打上） */
  --tabbar-offset: 0px;
  border-radius: var(--radius-card);
  /* 裁掉溢出：Monaco、欢迎页、图片预览都是它的子元素，
     不裁的话它们的直角会从卡片圆角外面冒出来 */
  overflow: hidden;
}

.editor-area.has-tabbar {
  --tabbar-offset: var(--size-tabbar);
}

/* 面板最大化时编辑器整个让位。
   ★ 用 display: none 而**不是**把它压成 0 高 —— 后者会让 Monaco
     去布局一个 0×0 的容器，各种计算都会变得很奇怪 */
.editor-area.is-hidden {
  display: none;
}

.tabbar {
  display: flex;
  /* ⚠ 这里不能用 align-items: center —— 标签必须**满高**。
     满高时它的底边和编辑器严丝合缝，活动标签（背景 = 编辑器色）
     才能和编辑器「连成一体」；一旦居中留出下面那道缝，
     标签就成了浮在标签栏上的一张卡片，那道缝很显眼 */
  /* 标签之间留缝 —— 不留的话相邻标签顶部的圆角会紧贴在一起 */
  gap: 2px;
  /* ⚠ 不加左右 padding：第一个标签必须**顶格**，VS Code 就是这样。
     当初加 `padding: 0 4px` 是为了让「四角全圆」的卡片左边圆角露出来，
     但标签后来改成只圆上面两角、下边贴紧编辑器，那个让位就没意义了 ——
     留着只会让第一个标签看起来「没对齐」 */
  flex: 0 0 var(--size-tabbar);
  overflow-x: auto;    /* 标签多了横向滚动，不换行 */
  background: var(--color-tabbar-bg);
}

.tab {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 0 10px;
  /* ★ 只圆上面两个角，下边两个角保持方角 —— 下边是要「贴着编辑器」的。
     四角全圆的话下沿会翘起来，和 VS Code 的观感不符。
     高度也不写死，交给 .tabbar 的默认 stretch 拉满
     ⚠ 不用 border-right 做分隔线：圆角之后它会从顶部的圆角处露出来一小截，
       标签之间的间隔改由 .tabbar 的 gap 承担 */
  border-radius: 4px 4px 0 0;
  background: var(--color-tab-bg);
  color: var(--color-text-dim);
  font-size: 13px;
  white-space: nowrap;
  cursor: pointer;
  user-select: none;
}

/* 未激活的标签悬停时给一点反馈，否则看不出可点。
   ★ 反馈由**两件事**组成：底色压暗 + 文字提亮。
     ⚠ 不能让底色变亮 —— 深色主题里「已打开」的标签只比标签栏底亮 5 级，
       悬停一旦更亮就会盖过它，看起来像「鼠标划过的比真正打开的还重要」。
     活动标签不参与：它的底色是编辑器色，改掉会断开和编辑器之间的连接 */
.tab:not(.active):hover {
  background: var(--color-tab-bg-hover);
  color: var(--color-tab-hover-fg);
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

/* 关闭按钮平时透明，只有鼠标停在这个标签上时才显现 —— 否则标签栏太吵。
   ⚠ 用 opacity 而不是 display / visibility：它一直占着位置，
     所以 × 的出现和消失不会把旁边的标签名挤来挤去。
     活动标签也不特殊对待 —— 默认同样不显示 × */
.tab-close {
  padding: 0 4px;
  font-size: 14px;
  line-height: 1;
  border-radius: 3px;
  opacity: 0;
}

.tab:hover .tab-close {
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

/* 改动审阅浮层。和 .image-preview 同一套路：盖住标签栏以下的整个编辑区。
   ★★ z-index 是 **2**，比欢迎页 / 图片预览（都是 1）高一层。
     为什么它非得更高：**它不依赖「有没有打开文件」** ——
     源代码管理里点一条改动就要弹 diff，而那时可能一个标签都没开、欢迎页正盖着。
     三层的 z-index 相同的话，按 CSS 绘制顺序**后面 DOM 里的赢**，
     于是 diff 会被欢迎页整个盖住（症状：点了没反应，其实早就渲染好了）。
     欢迎页和图片预览之间不会打架：图片预览要 `activeTabPath` 是张图，
     那时候欢迎页（`!activeTabPath`）必然不在 */
.diff-review {
  position: absolute;
  inset: var(--tabbar-offset, 0px) 0 0 0;
  z-index: 2;
  display: flex;
  flex-direction: column;
  background: var(--color-editor-bg);
}

.diff-review-bar {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 4px 8px;
  border-bottom: 1px solid var(--color-menu-border);
  background: var(--color-tabbar-bg);
}

.diff-review-path {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  color: var(--color-text);
  font-size: 12px;
  white-space: nowrap;
  text-overflow: ellipsis;
}

.diff-review-actions {
  display: flex;
  align-items: center;
  gap: 6px;
}

.diff-review-accept,
.diff-review-reject,
.diff-review-close {
  height: 22px;
  padding: 0 10px;
  border: 1px solid var(--color-menu-border);
  background: transparent;
  color: var(--color-text);
  font-family: inherit;
  font-size: 12px;
  cursor: pointer;
  border-radius: 3px;
}

.diff-review-accept {
  border-color: transparent;
  background: var(--color-selection);
  color: var(--color-text-on-accent);
}

.diff-review-close {
  padding: 0 8px;
  font-size: 14px;
  line-height: 1;
}

.diff-review-body {
  flex: 1;
  min-height: 0;
}

/* ---------- 图片预览层 ---------- */

/* inset 的四个值对应 上 / 右 / 下 / 左。
   上边空出标签栏的高度，所以它正好盖住 Monaco 那一块 */
.image-preview {
  position: absolute;
  inset: var(--tabbar-offset) 0 0 0;
  /* 只要比 .editor 那个「盒子」（z-index: 0）大就行 ——
     不用也没必要去追 Monaco 内部的 z-index 到底用了多少。
     和欢迎页同层：两者互斥（要有标签才有图，没标签才显示欢迎页），不会打架 */
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
  /* ⚠ 用 --tabbar-offset 而**不是** var(--size-tabbar)：
     没打开文件时标签栏不渲染，写死 35px 会露出一条 Monaco 的内容
     （详见 .editor-area 里那段说明） */
  inset: var(--tabbar-offset) 0 0 0;
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

/* 「更多快捷键 / 收起」的折叠开关。
   ★ 它是个 button，但长得像链接 —— 和上面那些 .welcome-link 视觉上同类。
     不复用那个类：那是「跳转」，这是「切换」，语义不同 */
.welcome-more {
  display: flex;
  align-items: center;
  gap: 4px;
  margin-top: 4px;
  padding: 3px 8px;
  border: 0;
  border-radius: 4px;
  background: transparent;
  /* font: inherit 不能省 —— button 有自己的默认字体 */
  font: inherit;
  color: var(--color-text-dim);
  cursor: pointer;
}

.welcome-more:hover {
  background: var(--color-hover);
  color: var(--color-text);
}

/* 箭头定宽：▸ 和 ▾ 不一样宽，不定宽的话切换时后面的字会左右跳一下 */
.welcome-more-caret {
  display: inline-block;
  width: 10px;
  font-size: 10px;
}

/* 最近打开的文件夹：一行里左边是名字、右边是路径。
   覆盖面比 .welcome-link 的 display: block，要改成 flex 才能一行放两个 */
.recent-folder {
  display: flex;
  align-items: baseline;
  gap: 8px;
}

/* 一行 = 「打开这个文件夹」的主按钮 + 一个移除按钮。
   主按钮吃掉剩余宽度，把 × 挤到最右边 */
.recent-item {
  display: flex;
  align-items: center;
  gap: 4px;
}

.recent-item .recent-folder {
  flex: 1 1 auto;
  min-width: 0;
}

/* 移除按钮：平时不显示，悬停到这一行才出现 —— 常亮会让列表很吵。
   和标签栏的关闭 × 同一套路
   ⚠ 用 opacity 而不是 display：不占位变化就不会把名字挤来挤去 */
.recent-remove {
  flex: 0 0 auto;
  padding: 0 4px;
  border: 0;
  border-radius: 3px;
  background: transparent;
  color: var(--color-text-dim);
  font-size: 14px;
  line-height: 1;
  opacity: 0;
  cursor: pointer;
}

.recent-item:hover .recent-remove,
/* ⚠ 键盘用户也得能摸到它 —— 只靠 :hover 的话，Tab 上来会看不到焦点在哪 */
.recent-remove:focus-visible {
  opacity: 1;
}

.recent-remove:hover {
  background: var(--color-hover);
  color: var(--color-text);
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

/* ---------- 分隔条（可拖拽调整相邻框的尺寸）---------- */

/* VS Code 里那两条「看不见但能拖」的缝。
   平时几乎不可见，鼠标靠近时光标变成 col-resize / row-resize */
/* ★ 分隔条**就是**那条缝隙：宽度 = --size-gap，背景透明。
   这样既保住了拖拽热区，又不用外加 gap（用了 gap 缝隙会翻倍，见 .main 的注释） */
.sash {
  flex: 0 0 var(--size-gap);
  /* 给中间那三个点当定位基准 */
  position: relative;
  /* 平时完全透明 —— 它站在缝隙里，有底色反而会把它变成一条多余的带子 */
  background: transparent;
  border-radius: 3px;
  user-select: none;
  /* ★ 不「马上」高亮：用**延迟**，不是**渐入** ——
     鼠标停住 0.2s 才亮，而且一亮就到位（0.1s），快速扫过完全看不见。
     ⚠ transition 简写的四个值顺序是「属性 | 时长 | 缓动 | **延迟**」，
       延迟是最容易写错位置的那个（写成 0.2s 时长就变成慢慢浮上来了） */
  transition: background 0.1s ease 0.2s;
}

/* 中间那三个点：鼠标一进入窗口就能看到「这里能拖」的暗示。
   只写一个点，另外两个用 box-shadow 复制 —— 比开两个伪元素省事 */
.sash::before {
  content: "";
  position: absolute;
  top: 50%;
  left: 50%;
  width: 2px;
  height: 2px;
  /* -1px 是把 2px 的点拉到正中心（left/top 给的是左上角） */
  margin: -1px 0 0 -1px;
  border-radius: 50%;
  background: var(--color-sash-dot);
}

.sash-vertical {
  cursor: col-resize;
}

/* ⚠ 三个点要沿着缝隙的**长边**排：
   竖缝竖排、横缝横排。横缝只有 6px 高，竖排的话点会溢出到卡片上去 */
.sash-vertical::before {
  box-shadow: 0 -5px 0 var(--color-sash-dot), 0 5px 0 var(--color-sash-dot);
}

.sash-horizontal {
  cursor: row-resize;
}

.sash-horizontal::before {
  box-shadow: -5px 0 0 var(--color-sash-dot), 5px 0 0 var(--color-sash-dot);
}

/* 悬停 / 拖动时高亮一下，否则不知道这里能拖。实心色，和 VS Code 一致 */
.sash:hover {
  background: var(--color-selection);
}

/* ---------- 底部面板 ---------- */

.panel {
  display: flex;
  /* ★ 高度不在这里写死 —— 它可以被拖拽，由 JS 的 panelHeight 给 inline style */
  flex: 0 0 auto;
  flex-direction: column;
  min-height: 0;
  background: var(--color-editor-bg);
  border-radius: var(--radius-card);
  overflow: hidden;
}

/* 最大化：不再靠 inline height，直接吃掉所有剩余空间
   （那时 .editor-area 已经 display: none 了，没有人和它抢） */
.panel.maximized {
  flex: 1 1 auto;
}

.panel-tabs {
  display: flex;
  flex: 0 0 var(--size-tabbar);
  align-items: stretch;
  padding: 0 4px;
  background: var(--color-tabbar-bg);
}

/* 标签用**底部一条线**表示选中，而不是换背景 —— 和 VS Code 一致 */
.panel-tab {
  padding: 0 10px;
  border: 0;
  border-bottom: 1px solid transparent;
  background: transparent;
  color: var(--color-text-dim);
  font: inherit;
  font-size: 11px;
  letter-spacing: 0.4px;
  text-transform: uppercase;
  cursor: pointer;
}

.panel-tab:hover {
  color: var(--color-text);
}

.panel-tab.active {
  color: var(--color-text-emphasis);
  border-bottom-color: var(--color-text-emphasis);
}

.panel-tabs-fill {
  flex: 1 1 auto;
}

/* 标题栏右边那两个按钮（最大化 / 收起）共用一套样式 */
.panel-action {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 28px;
  border: 0;
  border-radius: 4px;
  background: transparent;
  color: var(--color-text-dim);
  font-size: 15px;
  line-height: 1;
  cursor: pointer;
}

.panel-action:hover {
  background: var(--color-hover);
  color: var(--color-text);
}

.panel-body {
  /* ★ 给绝对定位的终端当定位基准（终端不能用 height: 100%，
     在 overflow: auto 的父里它会陷进「100% 指谁」的循环） */
  position: relative;
  flex: 1 1 auto;
  min-height: 0;
  overflow: auto;
}

.panel-empty {
  margin: 12px 16px;
  color: var(--color-text-dim);
  font-size: 12px;
}

/* 输出面板：等宽字体，一行一条日志 */
.panel-output {
  padding: 4px 0;
  font-family: Consolas, "Courier New", monospace;
  font-size: 12px;
  line-height: 1.6;
}

.panel-log {
  display: flex;
  gap: 10px;
  margin: 0;
  padding: 0 12px;
  white-space: pre-wrap;
  word-break: break-word;
}

.panel-log-time {
  flex: 0 0 auto;
  color: var(--color-text-dim);
}

/* 警告用「脏标签」那个橙、错误用错误文字色 —— 不另造新颜色 */
.panel-log.warn .panel-log-text {
  color: var(--color-dirty);
}

.panel-log.error .panel-log-text {
  color: var(--color-error-text);
}

/* ---------- 问题面板 ---------- */

/* 标签上的数量徽标。用半透明底色 —— 标签栏底色会跟主题变，
   实色块很容易在某一套主题下硌出来 */
.panel-tab-badge {
  margin-left: 5px;
  padding: 0 5px;
  border-radius: 8px;
  background: var(--color-kbd-bg);
  font-size: 10px;
  letter-spacing: 0;
}

.panel-problems {
  padding: 4px 0 8px;
  font-size: 12px;
}

/* 文件分组：一条分隔线 + 文件名，和 VS Code 的问题面板一个形状 */
.problem-file {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 3px 12px;
  border-bottom: 1px solid var(--color-menu-border);
  color: var(--color-text-emphasis);
  font-weight: 600;
}

.problem-file-count {
  color: var(--color-text-dim);
  font-weight: 400;
}

.problem-row {
  display: flex;
  align-items: center;
  gap: 6px;
  width: 100%;
  padding: 3px 12px 3px 22px;
  border: 0;
  background: transparent;
  color: var(--color-text);
  font: inherit;
  font-size: 12px;
  text-align: left;
  cursor: pointer;
}

.problem-row:hover {
  background: var(--color-hover);
}

/* 图标是**填充**图形，见 PROBLEM_ICON 的说明。
   颜色用「错误 / 警告 / 信息」三档 —— 值是从 VS Code 自己的代码里挖的
   （problemsErrorIcon.foreground 就是 editorError.foreground），
   主题文件里没有这几个键，所以是写死的两套 */
.problem-icon {
  flex: 0 0 auto;
  fill: currentColor;
}

.problem-icon.error {
  color: var(--color-problem-error);
}

.problem-icon.warning {
  color: var(--color-problem-warning);
}

.problem-icon.info,
.problem-icon.hint {
  color: var(--color-problem-info);
}

/* 消息占满剩下的宽度并允许收缩 —— 不写 min-width: 0 的话，
   长消息会把右边的「来源 / 行列」顶出面板 */
.problem-message {
  flex: 1 1 auto;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.problem-source,
.problem-location {
  flex: 0 0 auto;
  color: var(--color-text-dim);
  font-size: 11px;
}

.statusbar {
  flex: 0 0 var(--size-statusbar);
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0 10px;
  background: var(--color-statusbar-bg);
  /* ★ 单独一个前景色变量，而不是复用 on-accent。
     它现在**不跟主题走**（底色固定成经典的 #007acc 蓝，见 THEME_VARS_CLEAR_ONLY），
     但仍然保留成变量而不是写死白色 —— 将来想让它跟随主题的话，
     只要把那两行的映射加回去就行，这里一行都不用改。
     （历史上正是「底色交给主题了、前景色却没交」导致浅色下出现白字白底） */
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
  /* ★ 悬停底色现在跟主题走（tab.hoverBackground）——
     它深色下比已打开的标签暗、浅色下比已打开的标签亮，**两边方向相反**。
     这种事就该让主题决定；下面这两行只是「主题没给值」时的兜底。
     ⚠ 早先这里写死过一个「统一压暗」的规则，理由写在下面这段注释里 ——
       那个推理的**前提**是错的（当时以为 tab.hoverBackground 不存在），
       而真正的毛病是读主题时没合并 include 链。
     已知的局限依然成立：深色下标签栏底 → 已打开的标签只差 5 级，
     留给悬停的亮度区间很窄，所以底色变化不明显，
     反馈主要靠**文字提亮**承担（见 --color-tab-hover-fg） */
  --color-tab-bg-hover: #282828;
  --color-tab-hover-fg: #cccccc;
  /* 活动栏图标的三种颜色 + 背后那两个圆角方块。
     ★ 这几个值都是主题里那几个键的**深色档** ——
       这里只是主题没加载时（比如直接在浏览器里看）的兜底 */
  --color-icon-hover: #d7d7d7;      /* activityBar.foreground */
  --color-icon-active: #0078d4;     /* activityBar.activeBorder，强调蓝 */
  --color-icon-hover-bg: #ffffff11; /* modernActivityBarItem.hoverBackground */
  /* ★ 选中项的方框：**比活动栏底（#181818）更深一层**，表达「陷下去 / 被按住」。
     不能用 modernActivityBarItem.activeBackground（它是 #FFFFFF22，更亮），
     主题里也没有更深的键 —— 和 --color-shell-bg 同一个处境，只能写死 */
  --color-icon-active-bg: #0d0d0d;
  /* ★ 「浮起」风格的底：活动栏 / 侧栏 / 编辑器 / 面板都是浮在它上面的卡片，
     卡片之间的缝隙里露出来的就是它。必须比**所有**卡片都深，否则缝隙看不出来。
     注意它不是任何主题键 —— 主题里没有比 activityBar.background(#181818) 更深的值了，
     所以这里用写死的兜底，深浅各一套 */
  --color-shell-bg: #101010;
  /* 卡片之间的缝隙宽度（也是 .sash 的宽度） */
  --size-gap: 6px;
  /* 卡片圆角 */
  --radius-card: 8px;

  /* 分隔条中间那三个小点（提示「这里能拖」）。用**半透明**，
     所以深浅两套主题共用同一组值，下面浅色主题不用再写一遍 */
  --color-sash-dot: rgba(128, 128, 128, 0.55);
  --color-statusbar-bg: #007acc;
  /* 兜底也是**权威值** —— 状态栏不映射主题色，见 THEME_VARS_CLEAR_ONLY */
  --color-statusbar-fg: #ffffff;

  --color-text: #cccccc;
  --color-text-dim: #969696;
  --color-text-emphasis: #ffffff;   /* 激活标签、强调文字 */
  --color-text-on-accent: #ffffff;  /* **我们自己的深色底**（选中项、菜单悬停）上的文字 */

  /* Topilot 里「用户说的话」那个气泡的底色。
     ★ **不用** --color-selection —— 那是「选中文字」的蓝（Dark Modern 下 #0078d4），
       铺一整块太刺眼（这是实测反馈）。这里要的是「压暗了、但还看得出是蓝的」那一档 */
  --color-user-bubble: #1e3f5c;

  /* 活动栏图标的**静止色**。它必须和「活动栏背景」配套 ——
     背景深，图标就得浅；背景浅，图标就得深。
     另外两档（hover / active）和那两个方块在下面 */
  --color-icon: #858585;

  --color-hover: #2a2d2e;
  --color-selection: #094771;
  --color-dirty: #e8ab53;
  --color-error-bg: #5a1d1d;
  --color-error-text: #f48771;

  /* 问题面板里三档严重程度的颜色。
     ★ 主题文件里**没有** `problems*Icon.foreground` 这些键 —— 它们属于
       VS Code 代码里的默认值（和 `gitDecoration.*` / `terminal.ansi*` 同一类）。
       这三个值是从 VS Code 自己的代码里挖出来的：
       problemsErrorIcon.foreground = editorError.foreground，
       problemsWarningIcon.foreground = editorWarning.foreground，
       problemsInfoIcon.foreground = editorInfo.foreground */
  --color-problem-error: #f14c4c;
  --color-problem-warning: #cca700;
  --color-problem-info: #59a4f9;

  /* 滚动条拇指。故意用半透明而不是写死的实色 ——
     轨道是透明的，拇指直接压在侧栏/标签栏的背景上，
     半透明能让它自己跟背景融合，不用为每个容器各配一个值 */
  --color-scrollbar-thumb: rgba(121, 121, 121, 0.4);
  --color-scrollbar-thumb-hover: rgba(121, 121, 121, 0.7);

  /* 欢迎页里的链接色。VS Code 的 TextLink 就是这个色 */
  --color-link: #3794ff;
  /* 搜索结果里那一段命中的高亮底色。
     ★ 用半透明蓝而不是实心色：结果行本身还有 hover 底色、
       半透明能叠在上面而不把它盖掉 */
  --color-search-hit: rgba(120, 170, 255, 0.28);

  /* 源代码管理里那个状态字母的颜色（M / U / D）。
     ★ 主题文件里**没有** `gitDecoration.*` 这些键 —— 它们属于 VS Code
       代码里的默认值，和 `terminal.ansi*` 是同一类情况。
       所以这里是写死的两套，值照搬 VS Code 本身的默认值 */
  --color-git-modified: #e2c08d;
  --color-git-new: #73c991;
  --color-git-deleted: #c74e39;

  /* 快捷键帽（kbd）。故意用**半透明灰**而不是实色 ——
     深浅两套主题可以共用同一组值（下面浅色主题不用再写一遍） */
  --color-kbd-bg: rgba(128, 128, 128, 0.17);
  --color-kbd-border: rgba(128, 128, 128, 0.35);

  --color-menubar-bg: #3c3c3c;
  --color-menubar-hover: #505050;
  --color-menu-bg: #252526;
  --color-menu-border: #454545;

  /* 标题栏按钮的悬停底色。半透明灰 —— 深浅两套主题共用同一组值 */
  --color-titlebar-button-hover: rgba(128, 128, 128, 0.25);

  --size-titlebar: 35px;
  --size-activitybar: 48px;
  --size-tabbar: 35px;
  --size-statusbar: 24px;
  /* ★ 侧栏宽度和面板高度**不在这里** —— 它们可以拖拽，由 JS 的
     DEFAULT_SIDEBAR_WIDTH / DEFAULT_PANEL_HEIGHT 管（见 LAYOUT_KEY）。
     同一个设置只能有一个权威入口，两处写一定会不一致 */

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
  /* 浅色下悬停本来就比「已打开」的标签暗，方向已经是对的，保持 */
  --color-tab-bg-hover: #e0e0e0;
  --color-tab-hover-fg: #1f1f1f;
  /* 浅色下所有卡片都在 #ececec 以上，底要更深才看得出缝隙 */
  --color-shell-bg: #cccccc;
  --color-statusbar-bg: #007acc;
  --color-statusbar-fg: #ffffff;

  --color-text: #333333;
  --color-text-dim: #6f6f6f;
  --color-text-emphasis: #000000;
  --color-text-on-accent: #ffffff;

  /* 浅色下同一个气泡。不用蓝得发紫，淡蓝就够 —— 深了反而扑眼 */
  --color-user-bubble: #d8e8f8;

  /* 活动栏背景变浅了，图标就必须翻成深的，
     否则灰图标糊在浅灰底上根本看不清 */
  --color-icon: #616161;
  --color-icon-hover: #1f1f1f;
  --color-icon-active: #005fb8;
  /* ★ 浅色下这两个方块**都不能用深色那套半透明白** —— 白铺在白底上等于没画。
     hover 用主题给的实色 #F2F2F2；
     active 要「比活动栏底更深」，所以自己定一个。
     ⚠ 它必须比**活动栏的兜底色 #dddddd 还深** —— 否则主题没加载时
       方框会反过来比活动栏亮（而「主题没加载」正好是我们调样式时会遇到的场合）*/
  --color-icon-hover-bg: #f2f2f2;
  --color-icon-active-bg: #c8c8c8;

  --color-hover: #e8e8e8;
  --color-selection: #0060c0;
  --color-dirty: #b8860b;
  --color-error-bg: #f8d7da;
  --color-error-text: #a94442;

  /* 问题面板那三档。浅色必须换 —— #cca700 那种亮黄铺在白底上直接发糊
     （和终端 ANSI 黄色是同一个坑）。值同样来自 VS Code 代码里的默认值 */
  --color-problem-error: #e51400;
  --color-problem-warning: #bf8803;
  --color-problem-info: #0063d3;

  --color-scrollbar-thumb: rgba(100, 100, 100, 0.35);
  --color-scrollbar-thumb-hover: rgba(100, 100, 100, 0.6);

  --color-link: #0066bf;

  --color-search-hit: rgba(0, 100, 200, 0.22);

  /* 浅色下这三个色必须换 —— 深色那套是给深底挑的：
     #e2c08d（淡黄）和 #73c991（亮绿）铺在白底上直接发糊 */
  --color-git-modified: #895503;
  --color-git-new: #007100;
  --color-git-deleted: #ad0707;
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

/* ---------- 把 Monaco 的滚动条拉回来和上面这套对齐 ----------

   ★★ 为什么非要单独写：Monaco 的滚动条**不归 `::-webkit-scrollbar` 管** ——
     它是拿 div 自己画的（`.scrollbar > .slider`），所以上面那套全局规则碰不到它。
     默认形态是 **14px 满宽、方角**，和侧栏那套「10px 轨道 + 2px 内缩 + 圆角」
     并排放着就是两个应用的样子。

   ★ 尺寸在 JS 里给（见 `MONACO_SCROLLBAR`），这里只管**圆角** ——
     Monaco 没有「圆角」这个选项，只能 CSS 补。
     3px 配 6px 宽的滑块 = 两端全圆，和 `::-webkit-scrollbar-thumb` 的 5px 配 6px 一致。

   ⚠ `.scrollbar` 同时包含横向和纵向两个，不用各写一遍；
     而且它不止覆盖主编辑器 —— diff 浮层、内部可滚动的小控件都走同一个类 */
.monaco-editor .scrollbar .slider {
  border-radius: 3px;
}
</style>