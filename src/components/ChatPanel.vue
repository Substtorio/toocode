<!--
  Topilot 对话面板（侧栏第三个视图）。

  ★ 它**自己管自己的消息历史**，不往 App.vue 塞 ——
    对话状态和「哪个文件打开着」没有关系，混在一起只会让 App.vue 更难读

  ★ 密钥**不在前端存**：这里只调 `has_secret` 问「配没配」，
    真正发请求时由 Rust 自己去读那个文件
-->
<script setup lang="ts">
import { computed, nextTick, onUnmounted, ref, watch } from "vue";
import { invoke } from "@tauri-apps/api/core";
// opener 插件：把 URL 交给系统默认浏览器。
// ★ 在 WebView 里让 <a href> 自己导航会把**整个应用界面顶掉**，
//   所以链接必须由我们接管（见 onChatLogClick）
import { openUrl } from "@tauri-apps/plugin-opener";
import { runAgent, type AgentConfig, type ChatMessage } from "../agentLoop";
// 编辑器状态桥：让 Topilot 知道用户正在看哪个文件、光标在哪、选了什么
import { getEditorContext } from "../editorBridge";
import { renderMarkdown } from "../markdown";
import {
  acceptAll,
  acceptChange,
  pendingChanges,
  rejectAll,
  rejectChange,
  reviewChange,
} from "../agentChanges";
// 会话（一段可以独立存、独立切回来的对话）。存储细节全在那边，见文件头
import {
  loadSessions,
  MAX_SESSIONS,
  newSession,
  saveSessions,
  titleFrom,
  type ChatSession,
  type ChatSessionMessage,
} from "../chatSessions";

const props = defineProps<{ visible: boolean; workspaceRoot: string | null }>();

const emit = defineEmits<{
  /** 「我要选个文件当上下文」—— 选择器由 App 开，见 App.vue 里 onQuickOpenSelect 的说明 */
  (e: "request-context"): void;
}>();

// 这两个是**新功能**的配置，不受「项目改名时不动 localStorage」那条约束 ——
// 没有历史数据要兼容，直接用新前缀
const CONFIG_KEY = "toocode:agentConfig";

interface StoredConfig {
  baseUrl: string;
  model: string;
}

function loadConfig(): StoredConfig {
  // ★ 默认值跟**官网当前给的模型字段**走（写成 `deepseek-flash`）。
  //   旧别名（`deepseek-chat` / `deepseek-reasoner`）仍然能用，所以
  //   **不**去改用户已经存好的值 —— 那是「静默换掉他实际发出去的东西」，不该做
  const fallback: StoredConfig = { baseUrl: "https://api.deepseek.com/v1", model: "deepseek-flash" };
  const raw = window.localStorage.getItem(CONFIG_KEY);
  if (raw === null) return fallback;

  try {
    const parsed = JSON.parse(raw) as Partial<StoredConfig>;
    return {
      baseUrl: typeof parsed.baseUrl === "string" ? parsed.baseUrl : fallback.baseUrl,
      model: typeof parsed.model === "string" ? parsed.model : fallback.model,
    };
  } catch {
    return fallback;
  }
}

const config = ref<StoredConfig>(loadConfig());

/**
 * 已知模型：**模型字段（发给接口的 id）→ 人看的版本名**的对照表。
 *
 * ★★ 两者的区别一定要分清：`deepseek-flash` 这种是**给接口看的 slug** ——
 *   它既不说明是哪个版本、也不说明有多大。而配置界面那一栏是给**人**看的，
 *   人想知道的是「我在跟哪个模型说话」。
 *   所以界面显示**版本名**，请求里照旧发**模型字段**（少了任何一个都不行）
 *
 * ★ 这张表**只影响显示**，不再是“唯一的候选来源” ——
 *   真正发给接口的值由下面那一行「模型字段」决定，
 *   候选项则由 `GET /models` 现问现拿（见 fetchModels）。
 *   官网改了模型字段，不用改代码就能跟上
 *
 * ★ 表里没有的照旧原样显示：兼容接口太多
 *   （通义 / Ollama / LM Studio 的模型名五花八门）
 */
const MODEL_PRESETS: ReadonlyArray<{ id: string; label: string }> = [
  { id: "deepseek-flash", label: "DeepSeek V4.1 Flash" },
  { id: "deepseek-v4-pro", label: "DeepSeek V4 Pro" },
  // 下面两个是 DeepSeek 长期存在的稳定别名，现在仍然能用。
  // ★ 留着它们是为了**不打翻已经配好的用户** —— 他们存的 `config.model` 就是这个
  { id: "deepseek-chat", label: "DeepSeek Chat（旧别名）" },
  { id: "deepseek-reasoner", label: "DeepSeek Reasoner（旧别名）" },
];

/** 下拉里那个「自定义…」的哨兵值。用一个真实 id 不可能是的字符串 */
const CUSTOM_MODEL = "\u0000custom";

/**
 * 调用名 → 版本名；表里没有就返回 null。
 *
 * ⚠ 刻意**不**在查不到时返回调用名 ——「这是个已知模型的版本名」和
 *   「这是个我们不认识的 id」是两件事，混在一起就没法判断
 *   「该不该显示自定义输入框」了
 */
function modelLabel(id: string): string | null {
  return MODEL_PRESETS.find((preset) => preset.id === id)?.label ?? null;
}

/** 工具栏 / 下拉框上显示的那个名字：认识的显示版本名，不认识的照原样 */
function modelDisplayName(id: string): string {
  return modelLabel(id) ?? id;
}

/**
 * 从接口拉回来的模型字段（`GET {baseUrl}/models`）。
 *
 * ★★ 为什么要有这个：官网的模型字段是会变的 ——
 *   内置一张写死的表，等它变了只能改代码重新发版。
 *   而 `/models` 是 OpenAI 兼容接口的**标准端点**，问一次拿到的就是**当前**答案，
 *   对 Ollama / LM Studio / 通义这些第三方接口一样管用
 *
 * ⚠ 只在**这次会话**里缓存，**不**写 localStorage：
 *   它是「接口现在有什么」，不是用户的配置 —— 存下来只会变成一个过期的东西
 */
const remoteModels = ref<string[]>([]);
const fetchState = ref<"idle" | "loading" | "ok" | "error">("idle");
const fetchError = ref("");

/** 下拉 / datalist 的候选：内置预设（有版本名）+ 接口报上来的（显示 id 本身） */
const modelOptions = computed(() => {
  const known = new Set(MODEL_PRESETS.map((preset) => preset.id));
  const extra = remoteModels.value
    .filter((id) => !known.has(id))
    .map((id) => ({ id, label: id }));
  return [...MODEL_PRESETS, ...extra];
});

/**
 * 上面那个下拉框绑的值。
 *
 * ★★ 完全**派生**，没有独立的开关状态：取值在候选里就显示它自己，
 *   不在就显示「自定义…」。
 *   （以前有一个 `customModel` ref，那是为了「切到自定义时保留草稿」—— 
 *     现在模型字段那一行**始终**可编辑，草稿天然就在 `config.model` 里，那个开关就多余了）
 */
const modelChoice = computed({
  get: () =>
    modelOptions.value.some((option) => option.id === config.value.model)
      ? config.value.model
      : CUSTOM_MODEL,
  set: (value: string) => {
    // 选「自定义」时**不**动 config.model —— 真正改值的地方是下面那一行输入框
    if (value !== CUSTOM_MODEL) config.value.model = value;
  },
});

/**
 * 问接口要一次模型列表。
 *
 * ⚠ 拉取失败**不是**「不能用了」—— 手填照旧。
 *   所以失败只把原因显示出来，不改 `config.model`、也不拦着保存
 */
async function fetchModels() {
  if (fetchState.value === "loading") return;
  fetchState.value = "loading";
  fetchError.value = "";
  try {
    remoteModels.value = await invoke<string[]>("list_models", { baseUrl: config.value.baseUrl });
    fetchState.value = "ok";
  } catch (error) {
    fetchState.value = "error";
    fetchError.value = error instanceof Error ? error.message : String(error);
  }
}

/** 密钥配了没有。null = 还没问过 */
const hasKey = ref<boolean | null>(null);
const keyInput = ref("");
const savingKey = ref(false);

/**
 * 用户是否**主动**打开了配置界面。
 *
 * ★ 这里之前踩过一个坑：那个「重新配置」按钮直接去调了 `clear_secret` ——
 *   点一下就**把密钥删了**，而且没有回头路（只能重新填）。
 *   「重新配置」的语义应该是**打开配置界面**，不是**清除密钥**。
 *   清除是个破坏性操作，得单独一个按钮、而且要说清楚
 */
const editingConfig = ref(false);

/** 什么时候显示配置界面：没配过（强制）、或者用户主动打开了 */
const showSetup = computed(() => hasKey.value !== true || editingConfig.value);

// 打开配置界面时**自动拉一次**模型列表 —— 这就是「跟着官网更新」的落点：
// 用户不需要知道该按哪个按钮，打开就能看到接口当前认哪些模型字段。
// ⚠ 三个前提缺一不可：有密钥（没密钥问了也是 401）、界面真的开着、这次会话还没拉过
//   （`idle` 就是那个「还没拉过」的状态）
watch(
  [showSetup, hasKey],
  ([open, key]) => {
    if (open && key === true && fetchState.value === "idle") void fetchModels();
  },
  { immediate: true },
);

/** 没配过密钥时才必须填；已有密钥时留空表示「只改地址 / 模型，不动密钥」 */
const canSaveConfig = computed(() => {
  if (savingKey.value) return false;
  // ★ 模型名不能空：空字符串发出去接口必然报错，
  //   而且报的是一句「model is required」这种指不到我们这一行的错。
  //   它同时也管住了「选了自定义但还没填完」那一瞬间
  if (config.value.model.trim() === "") return false;
  if (keyInput.value.trim() !== "") return true;
  // 留空：只有「已经配过」才允许保存
  return hasKey.value === true;
});

/**
 * 会话列表（最新的排最前）。
 *
 * ★★ 消息内容和它放在一起，切会话就只是改一个 `activeSessionId` ——
 *   而不是「把 messages 和 history 两份都换掉」。
 *   两份换的话漏一个就会出现「界面上是 A、发给模型的是 B」这种
 *   不报错、只答非所问的怪事
 */
const sessions = ref<ChatSession[]>([]);
const activeSessionId = ref("");
const sessionsOpen = ref(false);
const sessionWrapEl = ref<HTMLElement | null>(null);

const activeSession = computed(
  () => sessions.value.find((session) => session.id === activeSessionId.value) ?? null,
);

/**
 * 当前会话的消息 / 发给模型的历史。
 *
 * ★ 用 `computed` 派生，不自己再存一份（理由见上面 sessions 的说明）。
 * ★ 带 setter 是因为 `retry` 和清空要**整份替换**这两份数据
 */
const messages = computed<ChatSessionMessage[]>({
  get: () => activeSession.value?.messages ?? [],
  set: (value) => {
    if (activeSession.value !== null) activeSession.value.messages = value;
  },
});

const history = computed<ChatMessage[]>({
  get: () => activeSession.value?.history ?? [],
  set: (value) => {
    if (activeSession.value !== null) activeSession.value.history = value;
  },
});

/** 列表里按最后更新排（新建的永远在最上面） */
const sortedSessions = computed(() => [...sessions.value].sort((a, b) => b.updatedAt - a.updatedAt));

const input = ref("");

/**
 * 附加的上下文文件（绝对路径）。
 * 走「+」按钮 ➝ 快速打开选出来，见 addContext
 */
const contexts = ref<string[]>([]);
const busy = ref(false);
const errorText = ref<string | null>(null);
const stopReason = ref<"answered" | "max-turns" | "aborted" | null>(null);

/** 待审的文件改动。它是模块级的，所以这里直接拿引用就行 */
const changes = pendingChanges();

const logEl = ref<HTMLElement | null>(null);
const inputEl = ref<HTMLTextAreaElement | null>(null);

/**
 * 让输入框跟着内容长高。
 *
 * ★ 不用固定的 rows，也不要右下角那个手动拖拽点 ——
 *   写多行时看不全、还得自己去拖。
 *   做法是每次输入先把高度归零再量 scrollHeight：
 *   **归零那一步不能省**，不然它只会越涨越高、缩不回去。
 *   上限交给 CSS 的 max-height，到顶了就自己出滚动条
 */
function autoGrow() {
  const el = inputEl.value;
  if (el === null) return;
  el.style.height = "auto";
  el.style.height = `${el.scrollHeight}px`;
}

// ★ 监听 input 而不是只在按键里调：这样「发完自动清空」也能跟着缩回去
watch(input, () => {
  void nextTick(autoGrow);
});

let uiSeq = 0;
let controller: AbortController | null = null;

/** 刚复制过哪一条（用它做「已复制」的临时反馈） */
const copiedId = ref<number | null>(null);
let copiedTimer: number | null = null;

/** 节流间隔。比人眼快，但足够把重复计算合并掉 */
const RENDER_INTERVAL_MS = 80;
let renderTimer: number | null = null;

/**
 * 安排一次 Markdown 重新渲染（节流）。
 *
 * ★ 为什么不能每次拿到 chunk 就渲染：
 *   每个 chunk 都是十几个字符，而每次渲染都要把**整段**重新解析一遍。
 *   一段 3000 字的回答要解析上百次，每次都是 O(n) —— 长回答会肉眼可见地卡。
 *   节流之后开销直接降一个数量级，而视觉上完全看不出区别
 */
function scheduleRender(message: ChatSessionMessage) {
  if (renderTimer !== null) return; // 已经排上了，等它跑
  renderTimer = window.setTimeout(() => {
    renderTimer = null;
    message.html = renderMarkdown(message.text);
  }, RENDER_INTERVAL_MS);
}

/**
 * 立刻渲染一次。
 *
 * ★ 流结束时必须调 —— 否则最后 80ms 里的内容还在定时器里没来得及渲染，
 *   表现是「回答的结尾几个字突然不见了」
 */
function flushRender(message: ChatSessionMessage) {
  if (renderTimer !== null) {
    clearTimeout(renderTimer);
    renderTimer = null;
  }
  message.html = renderMarkdown(message.text);
}

/** 供模型看的对话历史 —— 和 UI 上是两套结构，各管各的 */

watch(
  config,
  (value) => {
    window.localStorage.setItem(CONFIG_KEY, JSON.stringify(value));
  },
  { deep: true },
);

/** 新内容进来就滚到底。★ 必须等 DOM 更新完，否则量到的还是旧高度 */
async function scrollToBottom() {
  await nextTick();
  const el = logEl.value;
  if (el) el.scrollTop = el.scrollHeight;
}

async function refreshKeyState() {
  try {
    hasKey.value = await invoke<boolean>("has_secret");
  } catch {
    hasKey.value = false;
  }
}

async function saveKey() {
  if (!canSaveConfig.value) return;

  const value = keyInput.value.trim();
  savingKey.value = true;
  try {
    // ★ 只在**真填了**新密钥时才写。留空就只改地址 / 模型 ——
    //   不然「想换个模型」就得重新去把密钥找出来贴一遗，很笨
    if (value !== "") {
      await invoke("save_secret", { value });
      // 存完立刻从输入框里抹掉 —— 明文留在 DOM 里没有任何好处
      keyInput.value = "";
    }

    editingConfig.value = false;
    await refreshKeyState();
  } catch (error) {
    errorText.value = `保存失败：${String(error)}`;
  } finally {
    savingKey.value = false;
  }
}

/**
 * 清除密钥。
 * ⚠ 这是个**破坏性**操作（而且没有撤销），所以单独一个按钮，
 *   而且要确认一下 —— 之前把它藏在「重新配置」后面，点一下密钥就没了
 */
async function forgetKey() {
  if (!window.confirm("确定要删除已保存的 API 密钥吗？删除后需要重新填写。")) return;

  try {
    await invoke("clear_secret");
    editingConfig.value = false;
    await refreshKeyState();
  } catch (error) {
    errorText.value = `清除密钥失败：${String(error)}`;
  }
}

onUnmounted(() => {
  controller?.abort();
  document.removeEventListener("mousedown", onDocumentMouseDown, true);
  // 顺手存一次 —— 万一最后一轮的落盘还在路上就被拆了
  persist();
});

// 面板第一次显示时才去问密钥状态 / 读聊天记录 —— 没打开过这个视图就不该碰它们
watch(
  () => props.visible,
  (visible) => {
    if (!visible) return;
    if (hasKey.value === null) void refreshKeyState();
    if (sessions.value.length === 0) void initSessions();
  },
  { immediate: true },
);

/**
 * 系统提示词。告诉模型「你在哪、能干什么、别干什么」。
 *
 * ★ 每次 send 都**重新生成**（而不是缓存成常量）——
 *   用户的编辑状态每秒都在变，缓存下来就成了一张过期的快照
 */
function buildSystemPrompt(): ChatMessage {
  const root = props.workspaceRoot;
  const lines: string[] = [
    "你是 Topilot —— Toocode 编辑器内置的编程助手。",
    root
      ? `用户当前打开的文件夹是：${root}`
      : "用户当前没有打开任何文件夹，你需要先问清楚要处理哪个目录。",
  ];

  // ★★ 把「用户此刻在看什么」附上。
  //   为什么不做成一个工具让模型自己来问：几乎每轮对话都要用到它，
  //   做成工具等于每次白多烧一轮 API 调用（几秒 + 一次计费）。
  //   而这段文本只有几百字符，无脑带上更划算
  const context = getEditorContext();
  if (context !== null) {
    const currentPath = context.path;
    lines.push("");

    if (currentPath !== null) {
      lines.push(`用户正在编辑的文件是：${currentPath}`);
      lines.push(`光标停在第 ${context.line} 行、第 ${context.column} 列。`);

      if (context.dirty) {
        // ★ 这一句得说 —— 否则模型会因为「自己读到的内容和用户描述的对不上」而犯迷糊
        lines.push(
          "（这个文件有**未保存**的修改 —— 你读它时拿到的是编辑器里的版本，不是磁盘上的）",
        );
      }

      if (context.selection !== "") {
        lines.push(
          `用户选中了第 ${context.selectionStartLine} 到 ${context.selectionEndLine} 行：`,
        );
        lines.push("```");
        lines.push(context.selection);
        lines.push("```");
        if (context.truncated) lines.push("（选中内容过长，只截取了开头一部分）");
      }
    } else {
      lines.push("用户当前没有打开任何文件。");
    }

    const others = context.openPaths.filter((path) => path !== currentPath);
    if (others.length > 0) lines.push(`另外还开着这些标签：${others.join("、")}`);
  }

  lines.push(
    "",
    "你可以调用工具来查看和**修改**代码。",
    "",
    "★ 修改文件时注意这几条：",
    "- 改之前先用 read_file 看**完整**内容。write_file 是整份替换，不是打补丁，",
    "  没看全就写会把没看到的代码覆盖掉",
    "- 改动**不会立刻生效** —— 会先让用户看 diff、确认后才写盘。",
    "  所以改完要把「改了什么、为什么这么改」说清楚",
    "- 一次别改太多文件，也别顺手重构无关的东西：用户要能一一看完",
    "",
    "读文件前先用 list_directory 或 search_in_folder 确认路径，不要凭空猜。",
    "回答用中文，简洁一些，不要复述你已经读到的整段代码。",
  );

  return { role: "system", content: lines.join("\n") };
}

/**
 * 发一条新消息。
 * ★ 这里只管「校验 + 记下用户说了什么」，真正的发送在 runTurn ——
 *   因为「重新回答」要复用后者，但它不该再推一条用户消息
 */
async function send() {
  const text = input.value.trim();
  if (text === "" || busy.value) return;
  if (hasKey.value !== true) {
    errorText.value = "还没有配置 API 密钥";
    return;
  }
  // 面板刚打开、会话还没读回来时的一小段窗口 —— 等一下再发
  if (activeSession.value === null) await initSessions();
  const session = activeSession.value;
  if (session === null) return;

  // ★ 附加的上下文只把**路径清单**交给模型，让它自己用 read_file 去看。
  //   直接把文件内容拼进来会撑爆上下文，而且模型未必需要全部；
  //   走 read_file 还顺带一个好处：它会**优先读编辑器里那份**（可能没保存）
  const content =
    contexts.value.length > 0
      ? `【需要参考的文件】\n${contexts.value.map((path) => `- ${path}`).join("\n")}\n\n${text}`
      : text;

  history.value.push({ role: "user", content });
  messages.value.push({
    id: ++uiSeq,
    role: "user",
    text,
    html: renderMarkdown(text),
    reasoning: "",
    tools: [],
  });

  // ★ 标题取自**第一条**用户消息 —— 列表里总得有个能认出来的名字。
  //   之后不再改：跟着对话内容变来变去的标题只会让人找不到
  if (session.messages.length === 1) session.title = titleFrom(text);
  session.updatedAt = Date.now();

  // 上下文已经跟着这条消息发出去了，清掉（VS Code 也是这个行为）
  contexts.value = [];
  input.value = "";
  await runTurn();
}

/**
 * 重新生成最后一条回复。
 *
 * ★ 关键是**把上一轮的痕迹清干净**再重发：
 *   · 历史要截断到最后一条用户消息为止。不清的话模型会看到
 *     「自己已经答过一次了」，于是可能只换个说法复述，或者干脆说「如上所述」
 *   · UI 上那条回复也要拿掉 —— 不然会变成两份答案叠在一起
 */
async function retry() {
  if (busy.value) return;
  if (hasKey.value !== true) {
    errorText.value = "还没有配置 API 密钥";
    return;
  }

  let lastUser = -1;
  for (let i = history.value.length - 1; i >= 0; i -= 1) {
    if (history.value[i].role === "user") {
      lastUser = i;
      break;
    }
  }
  if (lastUser === -1) return;

  // 整体替换，而不是原地 splice —— 本轮的工具结果、assistant 消息全在这一段里
  history.value = history.value.slice(0, lastUser + 1);

  for (let i = messages.value.length - 1; i >= 0; i -= 1) {
    if (messages.value[i].role === "assistant") {
      messages.value.splice(i, 1);
      break;
    }
  }

  await runTurn();
}

/** 真正发一轮请求。send 和 retry 共用 */
async function runTurn() {
  errorText.value = null;
  stopReason.value = null;

  busy.value = true;
  controller = new AbortController();

  // 本轮的 UI 承载对象 —— 流式增量都往它上面贴
  const draft: ChatSessionMessage = {
    id: ++uiSeq,
    role: "assistant",
    text: "",
    html: "",
    reasoning: "",
    tools: [],
  };
  messages.value.push(draft);

  // ★★ 拿到**响应式代理**再往下写，而不是继续用上面那个原始对象。
  //
  //   push 进 reactive 数组的对象，模板里读到的是它的**代理**；
  //   而直接改原始对象会绕过 setter —— **不会触发任何更新**。
  //   症状很具体：文字确实攒对了，但**不是一点点冒出来的**，
  //   而是一直憋到整轮结束（busy 变 false 那次才触发）——
  //   看起来像「模型很慢」，其实早就该显示出来了
  const reply = messages.value[messages.value.length - 1] ?? draft;
  await scrollToBottom();

  const configValue: AgentConfig = { baseUrl: config.value.baseUrl, model: config.value.model };

  try {
    await runAgent(configValue, [buildSystemPrompt(), ...history.value], {
      onText(chunk) {
        reply.text += chunk;
        scheduleRender(reply);
        void scrollToBottom();
      },
      onReasoning(chunk) {
        reply.reasoning += chunk;
        void scrollToBottom();
      },
      onToolStart(name, args) {
        reply.tools.push({ name, args, result: "", done: false });
        void scrollToBottom();
      },
      onToolEnd(name, result) {
        // 找最后一个同名且没结束的 —— 同名工具可能连着调好几次
        for (let i = reply.tools.length - 1; i >= 0; i -= 1) {
          if (reply.tools[i].name === name && !reply.tools[i].done) {
            reply.tools[i].result = result;
            reply.tools[i].done = true;
            break;
          }
        }
        void scrollToBottom();
      },
      onDone(reason) {
        stopReason.value = reason;
      },
    }, controller.signal, history.value);
  } catch (error) {
    errorText.value = String(error instanceof Error ? error.message : error);
  } finally {
    // 不管成败都补一次渲染：流末尾那一段还在定时器里等着，
    // 不补的话会直接丢掉
    flushRender(reply);
    busy.value = false;
    controller = null;
    const session = activeSession.value;
    if (session !== null) session.updatedAt = Date.now();
    // ★ 一轮说完就落盘（而不是每个 chunk 都存）——
    //   整份 JSON.stringify + 一次写文件，一轮一次完全能接受
    persist();
    await scrollToBottom();
  }
}

/**
 * 复制一条回复的正文。
 *
 * ★ 复制的是**原文**（message.text），不是渲染好的 HTML ——
 *   贴到编辑器里要的就是源码，带一堆 <p> 标签没有任何用
 */
async function copyMessage(message: ChatSessionMessage) {
  try {
    // Tauri 的 WebView 跑在 http://tauri.localhost 上，属于「可信来源」，
    // 所以 navigator.clipboard 是可用的（它只对安全上下文开放）
    await navigator.clipboard.writeText(message.text);

    copiedId.value = message.id;
    if (copiedTimer !== null) window.clearTimeout(copiedTimer);
    copiedTimer = window.setTimeout(() => {
      copiedId.value = null;
      copiedTimer = null;
    }, 1500);
  } catch (error) {
    errorText.value = `复制失败：${String(error)}`;
  }
}

/** 最后一条回复的 id —— 「重新回答」只挂在它下面 */
const lastAssistantId = computed(() => {
  for (let i = messages.value.length - 1; i >= 0; i -= 1) {
    if (messages.value[i].role === "assistant") return messages.value[i].id;
  }
  return null;
});

function stop() {
  controller?.abort();
}

// ---------- 会话的增 / 删 / 切 ----------

/** 当前「在聊哪个」。只存一个 id，很小，放 localStorage 不会和 hot exit 抢配额 */
const ACTIVE_CHAT_KEY = "toocode:activeChat";

/** 落盘。★ 不 await —— 调用它的地方都是「顺手存一下」，不该把对话拖慢 */
function persist() {
  void saveSessions(sessions.value);
}

/**
 * 切过去之后把还没渲染过的消息补上。
 *
 * ★ `html` 不存盘（它是从 `text` 现算的派生数据），所以从磁盘读回来的消息
 *   是「没有 HTML」的 —— 不补的话切回旧会话就是一片空白
 */
function renderMessages(session: ChatSession) {
  for (const message of session.messages) {
    if (message.html === undefined) message.html = renderMarkdown(message.text);
  }
}

function activateSession(id: string) {
  const session = sessions.value.find((item) => item.id === id);
  if (session === undefined) return;

  activeSessionId.value = session.id;
  renderMessages(session);
  window.localStorage.setItem(ACTIVE_CHAT_KEY, session.id);

  // 上一个会话的报错 / 停止原因不该带到这一个里
  errorText.value = null;
  stopReason.value = null;
  copiedId.value = null;
  void scrollToBottom();
}

/** 面板第一次显示时才去读聊天记录 */
async function initSessions() {
  const loaded = await loadSessions();
  sessions.value = loaded.length > 0 ? loaded : [newSession()];

  // ★ 消息 id 要接在历史里最大的那个后面 —— 不然新消息会和旧消息撞 id，
  //   而 id 是列表的 key（撞了 Vue 会复用错节点，症状是「内容串了」）
  uiSeq = sessions.value.reduce(
    (max, session) =>
      session.messages.reduce((inner, message) => Math.max(inner, message.id), max),
    0,
  );

  const last = window.localStorage.getItem(ACTIVE_CHAT_KEY);
  const target = sessions.value.find((session) => session.id === last) ?? sessions.value[0];
  activeSessionId.value = target.id;
  renderMessages(target);
}

/** 新建一段对话。
 *
 * ★ 这里原来是「清空对话」（还带一个确认框）。有了会话之后它就没必要了 ——
 *   新建**不会**毁掉旧的，所以不用问；而「清空」和「新建」给用户的结果一样
 *   （都是一段空对话），两个按钮干一件事只会让人犹豫点哪个
 * ⚠ 流式进行中不让切：那时候 `reply` 指着旧会话里的那条消息，
 *   切走之后流还在往一个看不见的地方写。要做成可切的话，
 *   得把 `controller` 和 `busy` 都变成**按会话**的 —— 那是另一件事了
 */
function newChat() {
  if (busy.value) return;

  const session = newSession();
  sessions.value.unshift(session);
  // ★ 别让列表无限长，而且**从列表里就丢掉** ——
  //   只在存盘时截断的话，界面上有、文件里没有，两边就对不上了
  if (sessions.value.length > MAX_SESSIONS) {
    sessions.value = sessions.value.slice(0, MAX_SESSIONS);
  }

  activateSession(session.id);
  persist();
}

function openSession(id: string) {
  if (busy.value) return;
  sessionsOpen.value = false;
  activateSession(id);
}

/** 删除一段对话。★ 这是**破坏性**操作（而且没有撤销），所以问一句 */
function deleteChat(id: string) {
  if (busy.value) return;

  const index = sessions.value.findIndex((session) => session.id === id);
  if (index === -1) return;
  const target = sessions.value[index];
  if (!window.confirm(`确定要删除「${target.title}」这段对话吗？删除后无法恢复。`)) return;

  sessions.value.splice(index, 1);

  // 删的正好是当前这段 ⇒ 挪到相邻一段去；一段都不剩就开个新的
  if (id === activeSessionId.value) {
    if (sessions.value.length === 0) {
      newChat();
      return; // newChat 自己会落盘
    }
    activateSession(sessions.value[Math.min(index, sessions.value.length - 1)].id);
  }
  persist();
}

/** 列表右边那个时间。★ 只说「大概多久以前」就够 —— 精确到秒没人看 */
function timeLabel(timestamp: number): string {
  const diff = Date.now() - timestamp;
  if (diff < 60_000) return "刚刚";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)} 分钟前`;

  const date = new Date(timestamp);
  const now = new Date();
  if (date.toDateString() === now.toDateString()) {
    const hh = String(date.getHours()).padStart(2, "0");
    const mm = String(date.getMinutes()).padStart(2, "0");
    return `${hh}:${mm}`;
  }
  return `${date.getMonth() + 1}月${date.getDate()}日`;
}

/**
 * 点别处就把会话列表收起来。
 *
 * ⚠ 开关按钮自己是包在 `sessionWrapEl` 里的 —— 不收进「外面」那一类，
 *   否则点开关时会「先被 mousedown 关掉、再被 click 打开」，永远关不上
 */
function onDocumentMouseDown(event: MouseEvent) {
  const target = event.target;
  if (target instanceof Node && sessionWrapEl.value?.contains(target) === true) return;
  sessionsOpen.value = false;
}

watch(sessionsOpen, (open) => {
  if (open) document.addEventListener("mousedown", onDocumentMouseDown, true);
  else document.removeEventListener("mousedown", onDocumentMouseDown, true);
});

/** 「+」按钮：请 App.vue 打开快速打开（上下文模式） */
function requestContext() {
  emit("request-context");
}

/**
 * 收一条附加的上下文文件。由 App.vue 在快速打开里选中后调过来。
 *
 * ★ 用 `defineExpose` 把方法交出去，而不是让父组件塞 prop、
 *   子组件 watch 再自己清空 —— 那样还得维护一个「这条用过了」的标记，
 *   绕而且容易漏
 */
function addContext(path: string) {
  if (contexts.value.includes(path)) return; // 同一个文件加两次没有意义
  contexts.value.push(path);
}

function removeContext(path: string) {
  contexts.value = contexts.value.filter((item) => item !== path);
}

defineExpose({ addContext });

/** 输入框里按 Enter 发送，Shift+Enter 换行 */
function onKeydown(event: KeyboardEvent) {
  if (event.key === "Enter" && !event.shiftKey) {
    event.preventDefault();
    void send();
  }
}

/**
 * 对话区的点击。目前只管一件事：链接。
 *
 * ★★ 必须**先 preventDefault，而且要在判断能不能打开之前** ——
 *    WebView 里 `<a href>` 一旦自己导航，整个应用界面会被替换成那个网页，
 *    没有地址栏、没有后退，只能关掉进程重开。
 *    所以这一步是**保命**的，和「打开成功与否」无关
 */
function onChatLogClick(event: MouseEvent) {
  const el = event.target as HTMLElement | null;
  // v-html 插进来的元素绑不了 Vue 事件，只能用事件委托
  const anchor = el?.closest?.("a[href]") as HTMLAnchorElement | null;
  if (anchor === null) return;

  event.preventDefault();

  const url = anchor.getAttribute("href") ?? "";
  // 消毒已经把非 http/https 的都挡了，这里再判一次不算冗余 ——
  // 它是最后一道，而且很便宜
  if (!/^https?:\/\//i.test(url)) return;

  // 交给系统默认浏览器。直接用 openUrl 而不是 window.open：
  // 后者在 WebView 里会尝试开一个新窗口，而 Tauri 默认不允许
  void openUrl(url).catch((error) => {
    errorText.value = `打不开链接：${String(error)}`;
  });
}
</script>

<template>
  <div class="chat-panel">
    <!-- 没配密钥时先只显示配置，不显示对话框 —— 让人先看到该做什么 -->
    <div v-if="showSetup" class="chat-setup">
      <p class="chat-setup-title">配置 Topilot</p>
      <p class="chat-setup-hint">
        密钥会保存在应用的本地数据目录里，<strong>不会写进前端</strong>，也不会回传给界面。
      </p>

      <label class="chat-field">
        <span>接口地址</span>
        <input v-model="config.baseUrl" type="text" spellcheck="false" />
      </label>

      <!-- 模型：下拉只是**快捷选择**，真正决定发给接口什么的是下面那行「模型字段」。
           ★ 选项来自「内置预设（有版本名）+ 接口报上来的」（见 modelOptions） -->
      <label class="chat-field">
        <span>模型</span>
        <select v-model="modelChoice">
          <option v-for="option in modelOptions" :key="option.id" :value="option.id">
            {{ option.label }}
          </option>
          <option :value="CUSTOM_MODEL">自定义…</option>
        </select>
      </label>

      <!-- 模型字段：**真正发给接口的那串 id**。
           ★ 始终可编辑（不再藏在「自定义…」后面）—— 值一旦不在上面的候选里，
             上面会显示「自定义…」，而改值就在这一行
           ★ 带 datalist：既能从候选里挑，也能手打任意 id（第三方接口就靠这个） -->
      <label class="chat-field">
        <span>模型字段（发给接口）</span>
        <input
          v-model="config.model"
          type="text"
          spellcheck="false"
          list="topilot-model-ids"
          placeholder="接口那边认的模型 id，如 deepseek-flash"
        />
      </label>

      <!-- 拉取状态 + 手动刷新。★ 拉取失败**不拦着保存** —— 手填照旧可用 -->
      <p class="chat-model-note">
        <button
          type="button"
          class="chat-mini-button"
          :disabled="fetchState === 'loading'"
          @click="fetchModels"
        >
          {{ fetchState === "loading" ? "获取中…" : "从接口获取列表" }}
        </button>
        <span v-if="fetchState === 'ok'">接口报了 {{ remoteModels.length }} 个模型，可从上面的候选里挑</span>
        <span v-else-if="fetchState === 'error'">拉取失败（手填照旧可用）：{{ fetchError }}</span>
        <span v-else-if="fetchState === 'loading'">正在问接口要模型列表…</span>
        <span v-else>配上密钥后，打开这一页会自动问一次接口，拿到的是官网当前的模型字段</span>
      </p>

      <!-- 候选数据源。放哪儿都行，它自己不渲染 -->
      <datalist id="topilot-model-ids">
        <option v-for="option in modelOptions" :key="option.id" :value="option.id">
          {{ option.label }}
        </option>
      </datalist>

      <label class="chat-field">
        <span>API 密钥</span>
        <input
          v-model="keyInput"
          type="password"
          spellcheck="false"
          :placeholder="hasKey === true ? '留空表示不修改' : ''"
          @keydown.enter="saveKey"
        />
      </label>

      <div class="chat-setup-actions">
        <button class="chat-primary" type="button" :disabled="!canSaveConfig" @click="saveKey">
          {{ savingKey ? "保存中…" : "保存" }}
        </button>
        <!-- 已经配过才给「返回」—— 没配过的话返回也没地方可去 -->
        <button
          v-if="hasKey === true"
          class="chat-secondary"
          type="button"
          @click="editingConfig = false"
        >
          返回
        </button>
        <!-- 破坏性操作单独放，而且要确认（见 forgetKey 的说明） -->
        <button v-if="hasKey === true" class="chat-danger" type="button" @click="forgetKey">
          删除密钥
        </button>
      </div>

      <p class="chat-setup-hint">
        兼容 OpenAI 接口的都可以：DeepSeek、通义、以及本地的 Ollama / LM Studio。
      </p>
    </div>

    <template v-else>
      <!-- 顶部工具栏。
           ★ 「清空」放这里、**远离发送按钮** —— 它俩原来是挨着的，
             手一滑整段对话就没了。
           ★ 「配置」也挪上来：它和「发送」毫无关系，混在一排会让人以为
             那一排都是「发送相关的」 -->
      <div class="chat-toolbar">
        <!-- 入口不再是纯图标按钮，而是「图标 + 当前模型」的框。
             两个理由：
             ① 模型名是「我现在在跟谁说话」的关键信息，藏在配置面板里等于没有
             ② `title` 提示只有鼠标停下来才看得到 —— 而模型名经常需要瞄一眼确认
             ★ 显示的是**版本名**（`DeepSeek V4.1 Flash`），不是调用名 ——
               和配置里那一栏保持同一套叫法，否则两边会对不上
             ★ 框宽**跟着名字走**（不设固定宽度）—— 模型名长短能差三倍 -->
        <button
          class="chat-model"
          type="button"
          :title="`配置 Topilot（当前模型：${modelDisplayName(config.model)}）`"
          @click="editingConfig = true"
        >
          <!-- 调节滑块 —— 比齿轮更贴「调整参数」这个意思 -->
          <svg
            viewBox="0 0 16 16"
            width="13"
            height="13"
            fill="none"
            stroke="currentColor"
            stroke-width="1.4"
            stroke-linecap="round"
            aria-hidden="true"
          >
            <path d="M2 5h5.2M11.2 5H14M2 11h2.8M8.8 11H14" />
            <circle cx="9.2" cy="5" r="1.9" />
            <circle cx="6.8" cy="11" r="1.9" />
          </svg>
          <span class="chat-model-name">{{ modelDisplayName(config.model) }}</span>
        </button>

        <!-- 会话：新建 + 历史列表。
             ★ 这里原来是「清空对话」（带确认框）。有了会话之后它就没必要了 ——
               新建**不会**毁掉旧的，所以不用问；而「清空」和「新建」的结果
               又是一样的（都是一段空对话），两个按钮干一件事只会让人犹豫点哪个 -->
        <div ref="sessionWrapEl" class="chat-sessions-wrap">
          <button
            class="chat-icon-button"
            type="button"
            title="新建聊天"
            :disabled="busy"
            @click="newChat"
          >
            <!-- 加号 -->
            <svg
              viewBox="0 0 16 16"
              width="14"
              height="14"
              fill="none"
              stroke="currentColor"
              stroke-width="1.4"
              stroke-linecap="round"
              aria-hidden="true"
            >
              <path d="M8 3.4v9.2M3.4 8h9.2" />
            </svg>
          </button>

          <button
            class="chat-icon-button"
            type="button"
            :title="`聊天记录（${sessions.length}）`"
            :disabled="busy"
            :class="{ active: sessionsOpen }"
            @click="sessionsOpen = !sessionsOpen"
          >
            <!-- 表盘 + 指针 = 历史。★ 不用「列表」图标：那和文件树 / 搜索结果撞脸 -->
            <svg
              viewBox="0 0 16 16"
              width="14"
              height="14"
              fill="none"
              stroke="currentColor"
              stroke-width="1.4"
              stroke-linecap="round"
              aria-hidden="true"
            >
              <circle cx="8" cy="8" r="5.6" />
              <path d="M8 4.9V8l2.3 1.5" />
            </svg>
          </button>

          <div v-if="sessionsOpen" class="chat-sessions">
            <div class="chat-sessions-head">聊天记录</div>
            <button
              v-for="session in sortedSessions"
              :key="session.id"
              class="chat-session"
              :class="{ active: session.id === activeSessionId }"
              type="button"
              @click="openSession(session.id)"
            >
              <span class="chat-session-title">{{ session.title }}</span>
              <span class="chat-session-time">{{ timeLabel(session.updatedAt) }}</span>
              <!-- 删除。⚠ 它不能是 <button> 套 <button>（HTML 不允许，浏览器会
                   把嵌套那个拆出去），所以是行内的兄弟节点 -->
              <span
                class="chat-session-delete"
                title="删除这段对话"
                @click.stop="deleteChat(session.id)"
              >
                ×
              </span>
            </button>
          </div>
        </div>
      </div>

      <div ref="logEl" class="chat-log" @click="onChatLogClick">
        <p v-if="messages.length === 0" class="chat-empty">
          我是 <strong>Topilot</strong>。问点什么吧 —— 我可以读当前工作区的文件来回答。
        </p>

        <div v-for="message in messages" :key="message.id" class="chat-msg" :class="message.role">
          <template v-if="message.role === 'assistant'">
            <!-- 思考过程。
                 ★ 默认**展开**（open）—— 用户要的就是「能看见它在想什么」。
                   流式期间这个 pre 会实时长出来，能直接看到推理到哪一步了。
                   不限高的话一段长推理会把回答挤出屏幕，所以 max-height 卡在 CSS 里
                 ⚠ 用户手动折叠之后，Vue 不会把它重新拨开：
                   新旧 vnode 的 open 值一样（都是 true），patch 时不会重设这个属性 -->
            <details v-if="message.reasoning" class="chat-reasoning" open>
              <summary>思考过程</summary>
              <pre>{{ message.reasoning }}</pre>
            </details>

            <details v-for="(tool, index) in message.tools" :key="index" class="chat-tool">
              <summary>
                <span class="chat-tool-name">{{ tool.name }}</span>
                <span class="chat-tool-state">{{ tool.done ? "" : "进行中…" }}</span>
              </summary>
              <pre class="chat-tool-args">{{ tool.args }}</pre>
              <pre v-if="tool.done" class="chat-tool-result">{{ tool.result }}</pre>
            </details>

            <div v-if="message.text" class="chat-text" v-html="message.html"></div>

            <!-- 每条回复下面的操作。
                 ★ 流式期间整组藏起来 —— 一边往外冒字、一边有按钮闪进闪出很难看，
                   而且那时候「重新回答」的语义也是错的 -->
            <div v-if="message.text && !busy" class="chat-msg-actions">
              <!-- 复制／已复制：用对勾代替「已复制」三个字 -->
              <button
                class="chat-msg-action"
                type="button"
                :title="copiedId === message.id ? '已复制' : '复制'"
                @click="copyMessage(message)"
              >
                <svg
                  v-if="copiedId === message.id"
                  viewBox="0 0 16 16"
                  width="13"
                  height="13"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="1.5"
                  stroke-linecap="round"
                  stroke-linejoin="round"
                  aria-hidden="true"
                >
                  <path d="M3 8.6l3.4 3.4L13 5" />
                </svg>
                <!-- 两个叠起来的方框 -->
                <svg
                  v-else
                  viewBox="0 0 16 16"
                  width="13"
                  height="13"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="1.4"
                  stroke-linecap="round"
                  stroke-linejoin="round"
                  aria-hidden="true"
                >
                  <rect x="5.4" y="5.4" width="8" height="8" rx="1.2" />
                  <path
                    d="M10.6 5.4V3.6a1.2 1.2 0 0 0-1.2-1.2H3.6a1.2 1.2 0 0 0-1.2 1.2v5.8a1.2 1.2 0 0 0 1.2 1.2h1.8"
                  />
                </svg>
              </button>
              <!-- ★「重新回答」只给最后一条：中间的回复后面还接着对话，
                   重新生成它会让后面那些都变得答非所问 -->
              <button
                v-if="message.id === lastAssistantId"
                class="chat-msg-action"
                type="button"
                title="重新回答"
                @click="retry"
              >
                <!-- 循环箭头 -->
                <svg
                  viewBox="0 0 16 16"
                  width="13"
                  height="13"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="1.4"
                  stroke-linecap="round"
                  stroke-linejoin="round"
                  aria-hidden="true"
                >
                  <path d="M13.2 8a5.2 5.2 0 1 1-1.6-3.7" />
                  <path d="M13.2 2.2v3.2H10" />
                </svg>
              </button>
            </div>
          </template>

          <div v-else class="chat-text" v-html="message.html"></div>
        </div>

        <p v-if="errorText" class="chat-error">{{ errorText }}</p>
        <p v-if="stopReason === 'max-turns'" class="chat-warn">
          达到最大轮数就停下了。可以接着说「继续」。
        </p>
      </div>

      <!-- 待审改动。
           ★ 放在对话流和输入框之间 —— 它是本轮对话的产物，紧挨着最合理。
             点文件名会在编辑器区域打开**并排 diff**（那里才够宽） -->
      <div v-if="changes.length > 0" class="chat-changes">
        <div class="chat-changes-head">
          <span class="chat-changes-count">{{ changes.length }} 处改动待确认</span>
          <button class="chat-link" type="button" @click="acceptAll">全部保留</button>
          <button class="chat-link" type="button" @click="rejectAll">全部撤销</button>
        </div>

        <div v-for="change in changes" :key="change.id" class="chat-change">
          <button
            class="chat-change-name"
            type="button"
            :title="change.path"
            @click="reviewChange(change.id)"
          >
            {{ change.path.split("/").pop() }}
          </button>
          <button class="chat-link" type="button" @click="acceptChange(change.id)">保留</button>
          <button class="chat-link" type="button" @click="rejectChange(change.id)">撤销</button>
        </div>
      </div>

      <div class="chat-compose">
        <!-- 已附加的上下文。放在输入框**上方** ——
             它们是「这条消息要带上的东西」，混进正文里就分不清哪句是用户说的了 -->
        <div v-if="contexts.length > 0" class="chat-contexts">
          <span v-for="path in contexts" :key="path" class="chat-context-chip" :title="path">
            <span class="chat-context-name">{{ path.split(/[\\/]/).pop() }}</span>
            <button
              class="chat-context-remove"
              type="button"
              title="移除"
              @click="removeContext(path)"
            >
              ×
            </button>
          </span>
        </div>

        <!-- 按钮浮在输入框**里面**的右下角，而不是在下面单独占一排 ——
             后者会平白多出一块空荡荡的区域，显得很突兀。
             所以这里要有个 wrapper 给按钮当定位基准 -->
        <div class="chat-input-wrap">
          <textarea
            ref="inputEl"
            v-model="input"
            class="chat-input"
            rows="1"
            title="Enter 发送，Shift+Enter 换行"
            spellcheck="false"
            @keydown="onKeydown"
          />

          <!-- 添加上下文：**圆角方形**（和圆形的发送区分开）。
               这个按钮刻意不放蓝色 —— 免得和「发送」抢注意力 -->
          <button
            class="chat-context-button"
            type="button"
            title="添加上下文"
            @click="requestContext"
          >
            <svg
              viewBox="0 0 16 16"
              width="13"
              height="13"
              fill="none"
              stroke="currentColor"
              stroke-width="1.5"
              stroke-linecap="round"
              aria-hidden="true"
            >
              <path d="M8 3.4v9.2M3.4 8h9.2" />
            </svg>
          </button>

          <!-- 发送和停止占**同一个位置**（互斥），
               这样忙起来的时候按钮不会换地方，手不用重新找 -->
          <button v-if="busy" class="chat-send" type="button" title="停止" @click="stop">
            <svg viewBox="0 0 16 16" width="12" height="12" aria-hidden="true">
              <rect x="4" y="4" width="8" height="8" rx="1" fill="currentColor" />
            </svg>
          </button>
          <button
            v-else
            class="chat-send"
            type="button"
            title="发送"
            :disabled="!input.trim()"
            @click="send"
          >
            <!-- 向上的箭头 -->
            <svg
              viewBox="0 0 16 16"
              width="13"
              height="13"
              fill="none"
              stroke="currentColor"
              stroke-width="1.7"
              stroke-linecap="round"
              stroke-linejoin="round"
              aria-hidden="true"
            >
              <path d="M8 13.2V3.2" />
              <path d="M4 7.2L8 3.2l4 4" />
            </svg>
          </button>
        </div>
      </div>
    </template>
  </div>
</template>

<style scoped>
.chat-panel {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
}

/* ---------- 配置界面 ---------- */
.chat-setup {
  padding: 12px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.chat-setup-title {
  margin: 0;
  color: var(--color-text-emphasis);
  font-size: 13px;
  font-weight: 600;
}

.chat-setup-hint {
  margin: 0;
  color: var(--color-text-dim);
  font-size: 11px;
  line-height: 1.5;
}

.chat-field {
  display: flex;
  flex-direction: column;
  gap: 3px;
  font-size: 11px;
  color: var(--color-text-dim);
}

.chat-field input,
.chat-field select {
  height: 26px;
  /* ⚠ `box-sizing` 必须显式写。Chrome 的 UA 样式给 `<select>` 设了
     `box-sizing: border-box`，而 `<input>` 是默认的 content-box ——
     于是同样写 `height: 26px`，输入框量出来是 **28px**（26 + 上下两条边框）、
     下拉框是 26px，并排放着差 2px。实测量出来才发现的 */
  box-sizing: border-box;
  padding: 0 8px;
  border: 1px solid var(--color-menu-border);
  background: var(--color-editor-bg);
  color: var(--color-text);
  font-family: inherit;
  font-size: 12px;
  outline: none;
  border-radius: 3px;
}

/* ⚠ input 和 select 用**同一条规则**：它俩是同一栏里的控件，
   分开写两份迟早会不一致 —— 而那种不一致只是「看着有点怪」，不报错 */
.chat-field input:focus,
.chat-field select:focus {
  border-color: var(--color-link);
}

/* 「从接口获取列表」那一行 —— 小一号的**次要**按钮 + 状态说明。
   ★ 刻意不给它主色：这是辅助动作，主操作始终是下面那个「保存」 */
.chat-model-note {
  margin: -2px 0 0;
  display: flex;
  align-items: center;
  gap: 6px;
  color: var(--color-text-dim);
  font-size: 11px;
  line-height: 1.5;
}

.chat-mini-button {
  flex: 0 0 auto;
  height: 22px;
  box-sizing: border-box;
  padding: 0 8px;
  border: 1px solid var(--color-menu-border);
  border-radius: 3px;
  background: var(--color-hover);
  color: var(--color-text);
  font-family: inherit;
  font-size: 11px;
  cursor: pointer;
}

.chat-mini-button:hover:not(:disabled) {
  border-color: var(--color-link);
  color: var(--color-text-emphasis);
}

.chat-mini-button:disabled {
  opacity: 0.6;
  cursor: default;
}

.chat-setup-actions {
  display: flex;
  align-items: center;
  gap: 6px;
  margin-top: 2px;
}

/* 破坏性操作：用警示色，和「保存」「返回」拉开距离 */
.chat-danger {
  height: 24px;
  margin-left: auto;
  padding: 0 10px;
  border: 1px solid var(--color-error-text);
  background: transparent;
  color: var(--color-error-text);
  font-family: inherit;
  font-size: 11px;
  cursor: pointer;
  border-radius: 3px;
}

/* ---------- 消息列表 ---------- */
.chat-log {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 8px 10px;
}

.chat-empty {
  margin: 12px 4px;
  color: var(--color-text-dim);
  font-size: 12px;
  line-height: 1.6;
}

.chat-msg {
  margin-bottom: 10px;
  font-size: 12px;
  line-height: 1.6;
}

/* 用户说的话靠右。
   ★ 用 flex 把气泡推过去，而不是 text-align —— 后者只移动文字，
     底色块还是满宽的，看起来像一整条横幅而不是「一个气泡」 */
.chat-msg.user {
  display: flex;
  justify-content: flex-end;
}

.chat-msg.user .chat-text {
  /* 宽度由内容决定（短消息就是个小气泡），但别长到贴着左边栏 */
  max-width: 85%;
  padding: 6px 8px;
  /* ★ 这里**不用** --color-selection —— 那是「选中文字」用的蓝
     （Dark Modern 下是 #0078d4），铺一整块太刺眼。
     单独给一个压暗过的蓝，深浅各一套 */
  background: var(--color-user-bubble);
  color: var(--color-text);
  border-radius: 5px;
}

/* ★ 这里**不能**再用 white-space: pre-wrap 了：
   v-html 插进来的 HTML 自带换行和缩进，再叠上 pre-wrap，
   每个段落之间都会多出一大块空白。
   需要保留换行的只有 pre / code 内部，单独给它们开（见下面） */
.chat-text {
  white-space: normal;
  word-break: break-word;
}

/* ---------- Markdown 正文 ----------
   ★ 必须用 :deep()。v-html 插进来的元素**不带 scoped 的 data-v 属性**，
     普通写法会被编译成 `.chat-text p[data-v-xxx]`，一条都匹配不上 ——
     症状是「Markdown 样式全部没生效」，而且不报错 */
.chat-text :deep(p) {
  margin: 0 0 6px;
}

.chat-text :deep(p:last-child) {
  margin-bottom: 0;
}

.chat-text :deep(ul),
.chat-text :deep(ol) {
  margin: 0 0 6px;
  padding-left: 18px;
}

.chat-text :deep(h1),
.chat-text :deep(h2),
.chat-text :deep(h3),
.chat-text :deep(h4) {
  margin: 8px 0 4px;
  font-size: 12px;
  font-weight: 600;
}

.chat-text :deep(h1:first-child),
.chat-text :deep(h2:first-child),
.chat-text :deep(h3:first-child) {
  margin-top: 0;
}

/* 行内代码 */
.chat-text :deep(code) {
  padding: 1px 4px;
  background: var(--color-hover);
  border-radius: 3px;
  font-family: Consolas, "Courier New", monospace;
  font-size: 11px;
}

/* 代码块 */
.chat-text :deep(pre) {
  margin: 0 0 6px;
  padding: 8px;
  background: var(--color-editor-bg);
  border: 1px solid var(--color-menu-border);
  border-radius: 4px;
  overflow: auto;
  max-height: 320px;
}

/* ★ 换行和缩进只在这里保留 —— 理由见上面 .chat-text 那段 */
.chat-text :deep(pre code) {
  padding: 0;
  background: none;
  white-space: pre;
}

.chat-text :deep(blockquote) {
  margin: 0 0 6px;
  padding-left: 8px;
  border-left: 2px solid var(--color-menu-border);
  color: var(--color-text-dim);
}

.chat-text :deep(a) {
  color: var(--color-link);
  text-decoration: underline;
  cursor: pointer;
}

.chat-text :deep(table) {
  border-collapse: collapse;
  margin-bottom: 6px;
}

.chat-text :deep(th),
.chat-text :deep(td) {
  border: 1px solid var(--color-menu-border);
  padding: 2px 6px;
}

.chat-text :deep(hr) {
  margin: 8px 0;
  border: none;
  border-top: 1px solid var(--color-menu-border);
}

/* 用户气泡里的行内代码：换个底色，不然和气泡底糊在一起。
   用编辑器底色 —— 它天然和气泡底色差一档，而且两套主题都对 */
.chat-msg.user .chat-text :deep(code) {
  background: var(--color-editor-bg);
}

/* ---------- 每条回复下面的操作 ---------- */
.chat-msg-actions {
  display: flex;
  gap: 2px;
  margin-top: 2px;
}

.chat-msg-action {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 22px;
  height: 20px;
  padding: 0;
  border: 0;
  border-radius: 3px;
  background: none;
  color: var(--color-text-dim);
  cursor: pointer;
}

.chat-msg-action:hover {
  background: var(--color-hover);
  color: var(--color-text);
}

/* ---------- 工具调用 ---------- */
.chat-tool,
.chat-reasoning {
  margin-bottom: 4px;
  border: 1px solid var(--color-menu-border);
  background: var(--color-editor-bg);
  border-radius: 4px;
  font-size: 11px;
}

.chat-tool summary,
.chat-reasoning summary {
  padding: 4px 8px;
  color: var(--color-text-dim);
  cursor: pointer;
  user-select: none;
}

.chat-tool-name {
  color: var(--color-text);
  font-weight: 600;
}

.chat-tool-state {
  margin-left: 6px;
  color: var(--color-dirty);
}

.chat-tool-args,
.chat-tool-result,
.chat-reasoning pre {
  margin: 0;
  padding: 6px 8px;
  border-top: 1px solid var(--color-menu-border);
  color: var(--color-text-dim);
  font-family: Consolas, "Courier New", monospace;
  font-size: 11px;
  /* 长内容自己滚，不要把整条消息撑到没边 */
  max-height: 220px;
  overflow: auto;
  white-space: pre-wrap;
  word-break: break-all;
}

/* ---------- 待审改动 ---------- */
.chat-changes {
  border-top: 1px solid var(--color-menu-border);
  padding: 6px 8px;
  background: var(--color-editor-bg);
}

.chat-changes-head {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-bottom: 4px;
}

.chat-changes-count {
  flex: 1;
  color: var(--color-dirty);
  font-size: 11px;
}

.chat-change {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 2px 0;
}

.chat-change-name {
  flex: 1;
  min-width: 0;
  padding: 0;
  border: none;
  background: transparent;
  color: var(--color-text);
  font-family: inherit;
  font-size: 12px;
  text-align: left;
  /* 长路径截断，不要把按钮撑破 */
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
  cursor: pointer;
}

.chat-change-name:hover {
  color: var(--color-link);
  text-decoration: underline;
}

/* ---------- 输入区 ---------- */
.chat-compose {
  border-top: 1px solid var(--color-menu-border);
  padding: 6px 8px;
}

/* 已附加的上下文。放在输入框上方 —— 它们是「这条消息要带上的东西」，
   混进正文里就分不清哪句是用户说的了 */
.chat-contexts {
  display: flex;
  flex-wrap: wrap;
  gap: 4px;
  margin-bottom: 5px;
}

.chat-context-chip {
  display: flex;
  align-items: center;
  gap: 2px;
  max-width: 100%;
  height: 20px;
  padding: 0 2px 0 6px;
  border: 1px solid var(--color-menu-border);
  border-radius: 3px;
  background: var(--color-hover);
  color: var(--color-text);
  font-size: 11px;
}

.chat-context-name {
  min-width: 0;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}

.chat-context-remove {
  display: flex;
  flex: 0 0 auto;
  align-items: center;
  justify-content: center;
  width: 14px;
  height: 14px;
  padding: 0;
  border: 0;
  border-radius: 2px;
  background: none;
  color: var(--color-text-dim);
  font-size: 12px;
  line-height: 1;
  cursor: pointer;
}

.chat-context-remove:hover {
  background: var(--color-selection);
  color: var(--color-text-on-accent);
}

/* ★ 这个 wrapper 存在的唯一理由：给那两个按钮**当定位基准** */
.chat-input-wrap {
  position: relative;
}

.chat-input {
  width: 100%;
  /* ★ 必须是 block —— textarea 默认是 inline-block，
     待在容器里会在底部留出**基线下沉的幽灵间隙**（实测 4px），
     于是 wrapper 比输入框高，那两个绝对定位的按钮就会往下偏。
     症状是「上下留白不对称」，但肉眼只会觉得「有点歪」，说不上来哪不对 */
  display: block;
  /* ★ 右边要给两个按钮留位（从右往左：发送 5+24、加号 5+24，再留 6 的间隙）——
     不留的话长文本会一头钻到按钮底下 */
  padding: 7px 64px 7px 8px;
  border: 1px solid var(--color-menu-border);
  background: var(--color-editor-bg);
  color: var(--color-text);
  font-family: inherit;
  font-size: 12px;
  line-height: 1.5;
  outline: none;
  /* ★ 不要右下角那个手动拖拽点 —— 高度由 autoGrow 自己管（见 script 里的说明） */
  resize: none;
  /* 单行时也得装得下那两个按钮（24px 高 + 上下各 5px） */
  min-height: 34px;
  /* 长到一定程度就自己滚，不能让它把对话区挤没 */
  max-height: 160px;
  overflow-y: auto;
  border-radius: 6px;
  box-sizing: border-box;
}

/* 聚焦时整个框包一层蓝 —— VS Code 的聚焦边框就是这个色
   （--color-link 是文字链接那种偏亮的蓝，不是它） */
.chat-input:focus {
  border-color: var(--color-selection);
}

/* 添加上下文：**圆角方形**，和圆形的发送区分开 —— 一眼能认出是两个不同的东西。
   ★ 这个按钮刻意不放蓝色（悬停、按下都是灰的）：
     一蓝一灰主次分明，两个都蓝就分不出哪个是主操作了 */
.chat-context-button {
  position: absolute;
  right: 34px;
  bottom: 5px;
  display: flex;
  align-items: center;
  justify-content: center;
  width: 24px;
  height: 24px;
  padding: 0;
  border: 0;
  border-radius: 4px;
  background: none;
  color: var(--color-text-dim);
  cursor: pointer;
}

.chat-context-button:hover {
  background: var(--color-hover);
  color: var(--color-text);
}

/* 按下时比悬停再浓一档，按下去有反应 */
.chat-context-button:active {
  background: rgba(128, 128, 128, 0.4);
  color: var(--color-text-emphasis);
}

/* 发送 / 停止：浮在输入框**右下角**的一个圆形按钮。
   ★ 贴底而不是垂直居中 —— 输入框会跟着内容长高，
     按钮跟着底边走才自然（写多行时它一直在手边）
   ★ 圆形比小方框更像「发送」这个动作，
     而且在圆角输入框里不会显得是个「贴上来的方块」 */
.chat-send {
  position: absolute;
  right: 5px;
  bottom: 5px;
  display: flex;
  align-items: center;
  justify-content: center;
  width: 24px;
  height: 24px;
  padding: 0;
  border: 0;
  border-radius: 50%;
  /* ★ 默认**没有底色** —— 输入框空着时只该看到一个淡淡的箭头，
     不该有个抢眼的实心圆摆在那儿 */
  background: none;
  color: var(--color-text-dim);
  cursor: pointer;
}

/* ★ 有内容（按钮可点）时才亮起来。这是「现在可以发了」的唯一提示，
   比在别处写一行灰字有效得多 */
.chat-send:not(:disabled) {
  background: var(--color-selection);
  color: var(--color-text-on-accent);
}

.chat-send:not(:disabled):hover {
  opacity: 0.85;
}

.chat-send:disabled {
  cursor: default;
}

/* ---------- 顶栏 + 图标按钮 ---------- */
.chat-toolbar {
  display: flex;
  flex: 0 0 auto;
  justify-content: flex-end;
  gap: 2px;
  padding: 4px 6px 0;
}

/* 统一的图标按钮：不带任何文字，靠 title 提示。
   ★ 无文字是有代价的 —— 图标得挑得足够常见，
     否则「简洁」就变成了「猜谜」 */
.chat-icon-button {
  display: flex;
  flex: 0 0 auto;
  align-items: center;
  justify-content: center;
  width: 24px;
  height: 24px;
  padding: 0;
  border: 0;
  border-radius: 4px;
  background: none;
  color: var(--color-text-dim);
  cursor: pointer;
}

.chat-icon-button:hover:not(:disabled) {
  background: var(--color-hover);
  color: var(--color-text);
}

.chat-icon-button:disabled {
  opacity: 0.4;
  cursor: default;
}

/* 下拉打开时按钮保持高亮 —— 让人知道「这个面板是从哪儿出来的」 */
.chat-icon-button.active {
  background: var(--color-hover);
  color: var(--color-text);
}

/* ---------- 会话（新建 + 历史列表）---------- */

/* ★ 外面这层要 `position: relative` —— 下拉浮层以它为基准 */
.chat-sessions-wrap {
  position: relative;
  display: flex;
  flex: 0 0 auto;
  gap: 2px;
}

.chat-sessions {
  position: absolute;
  top: calc(100% + 2px);
  right: 0;
  z-index: 5;
  width: 240px;
  max-height: 280px;
  overflow-y: auto;
  padding: 4px;
  border: 1px solid var(--color-menu-border);
  border-radius: 5px;
  /* 和菜单 / 悬停提示用同一个底色（都是 editorWidget.background），
     免得同一个面板里弹出两种颜色的浮层 */
  background: var(--color-menu-bg);
  box-shadow: 0 0 12px rgba(0, 0, 0, 0.14);
}

.chat-sessions-head {
  padding: 2px 6px 4px;
  color: var(--color-text-dim);
  font-size: 11px;
}

/* 一行 = 一段对话 */
.chat-session {
  display: flex;
  align-items: center;
  gap: 6px;
  width: 100%;
  padding: 4px 6px;
  border: 0;
  border-radius: 4px;
  background: none;
  color: var(--color-text);
  font-family: inherit;
  font-size: 12px;
  text-align: left;
  cursor: pointer;
}

.chat-session:hover {
  background: var(--color-hover);
}

/* 当前这段。⚠ 写在 :hover **之后**（同优先级后写的赢），
   否则鼠标划过去会把选中色盖掉 */
.chat-session.active {
  background: var(--color-selection);
  color: var(--color-text-on-accent);
}

.chat-session-title {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}

.chat-session-time {
  flex: 0 0 auto;
  color: var(--color-text-dim);
  font-size: 10px;
}

/* 选中项里那行小字跟着反白 —— 它自己的 dim 色在深蓝底上看不清 */
.chat-session.active .chat-session-time {
  color: inherit;
}

/* 删除：平时**不显示**。
   ★ 用 opacity 而不是 display —— display 一改标题的宽度就跳一下
   ⚠ 必须留 :focus-visible：只靠 :hover 的话键盘用户看不到焦点在哪 */
.chat-session-delete {
  flex: 0 0 auto;
  width: 16px;
  height: 16px;
  border-radius: 3px;
  color: inherit;
  font-size: 13px;
  line-height: 15px;
  text-align: center;
  opacity: 0;
  cursor: pointer;
}

.chat-session:hover .chat-session-delete,
.chat-session-delete:focus-visible {
  opacity: 0.8;
}

.chat-session-delete:hover {
  background: var(--color-hover);
  opacity: 1;
}

/* 模型框。取代原来那个纯图标的「配置」按钮。
   ★★ 「框的长度自动增长」靠的是**不写宽度**：
     `display: flex` + 条目默认 `flex: 0 1 auto` ⇒ 宽度就是内容宽（图标 + 文字）
     ⇒ 模型名换长了，框自己变长，不用改任何代码
   ⚠ 但不能没有上限：面板最窄可以到 240px，一个特别长的模型名会把
     旁边那个「清空」按钮挤出去。所以 `min-width: 0` 让它可收缩 +
     文字那层用省略号 —— 这是个「先能缩，才敢长」的顺序问题 */
.chat-model {
  display: flex;
  flex: 0 1 auto;
  min-width: 0;
  align-items: center;
  gap: 4px;
  height: 24px;
  padding: 0 7px;
  border: 1px solid var(--color-menu-border);
  border-radius: 4px;
  background: none;
  color: var(--color-text-dim);
  font-family: inherit;
  font-size: 11px;
  cursor: pointer;
}

.chat-model:hover {
  background: var(--color-hover);
  color: var(--color-text);
}

/* 图标不会缩，缩的只能是文字 */
.chat-model svg {
  flex: 0 0 auto;
}

.chat-model-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.chat-primary {
  height: 24px;
  padding: 0 12px;
  border: none;
  background: var(--color-selection);
  color: var(--color-text-on-accent);
  font-family: inherit;
  font-size: 12px;
  cursor: pointer;
  border-radius: 3px;
}

.chat-primary:disabled {
  opacity: 0.5;
  cursor: default;
}

.chat-secondary {
  height: 24px;
  padding: 0 10px;
  border: 1px solid var(--color-menu-border);
  background: transparent;
  color: var(--color-text);
  font-family: inherit;
  font-size: 12px;
  cursor: pointer;
  border-radius: 3px;
}

.chat-secondary:disabled {
  opacity: 0.5;
  cursor: default;
}

.chat-link {
  margin-left: auto;
  border: none;
  background: transparent;
  color: var(--color-link);
  font-family: inherit;
  font-size: 11px;
  cursor: pointer;
}

.chat-error {
  margin: 6px 2px;
  padding: 6px 8px;
  background: var(--color-error-bg);
  color: var(--color-error-text);
  font-size: 11px;
  border-radius: 4px;
  white-space: pre-wrap;
}

.chat-warn {
  margin: 6px 2px;
  color: var(--color-dirty);
  font-size: 11px;
}
</style>
