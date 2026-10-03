<script lang="ts">
/**
 * 单独开一个普通 `<script>` 块只是为了 **export 一个类型** ——
 * `<script setup>` 里不能 export 任何东西（包括类型），
 * 而 App.vue 需要这个类型来描述它传进来的数据。
 *
 * 两个 script 块的绑定是共享的，所以下面 `<script setup>` 里能直接用 QuickOpenEntry。
 */
export interface QuickOpenEntry {
  /** 唯一标识。选中时把这个 emit 出去，调用方再映射回真正的动作 */
  key: string;
  /** 主文本。既用来显示，也用来匹配 */
  text: string;
  /** 右侧那段灰色小字（快捷键 / 相对路径）。可选 */
  hint?: string;
}
</script>

<script setup lang="ts">
/**
 * 命令面板 / 快速打开共用的浮层。
 *
 * ★ 两者本质上是同一个东西：一个输入框 + 一个过滤后的列表 + 键盘导航。
 *   差别只在**数据源**和**选中后的动作**，所以合成一个组件，由调用方转换数据。
 *
 * ★ 组件刻意不认识「命令」或「文件」，只认 `{ key, text, hint }`：
 *   不需要泛型，也不用 props 传函数进来生成文本 —— App 侧先把数据转好。
 */
import { computed, nextTick, ref, watch } from "vue";
import { fuzzyFilter } from "../fuzzy";

const props = defineProps<{
  visible: boolean;
  placeholder: string;
  items: readonly QuickOpenEntry[];
  emptyText?: string;
}>();

const emit = defineEmits<{
  (e: "select", key: string): void;
  (e: "close"): void;
  /**
   * 输入框的当前内容。
   *
   * ★ 为什么要把它抛出去：候选集有可能来自后端（「转到文件」的大仓库场景），
   *   后端只能拿到关键词才能缩小范围 —— 候选集大得全量拉回来就不划算了。
   *   组件自己不管这事，它只是把「用户敲了什么」告诉调用方
   */
  (e: "query", value: string): void;
}>();

/** 列表再长也不会超过这个数 —— 不封顶的话一次渲染几千个节点会卡 */
const MAX_RESULTS = 50;

const query = ref("");
const activeIndex = ref(0);
const inputRef = ref<HTMLInputElement | null>(null);
const listRef = ref<HTMLElement | null>(null);

const results = computed(() =>
  fuzzyFilter(query.value, props.items, (entry) => entry.text).slice(0, MAX_RESULTS),
);

// 输入变了就把选中项拉回第一条 —— 否则会出现「选中第 5 项，但新结果只剩 2 项」
watch(query, () => {
  activeIndex.value = 0;
  emit("query", query.value);
});

// 结果变短时也要夹一下（比如删字符导致结果变少）
watch(results, (list) => {
  if (activeIndex.value >= list.length) activeIndex.value = 0;
});

// 打开时清空输入并聚焦。★ 必须 nextTick —— 元素是 v-if 出来的，此刻还不存在
watch(
  () => props.visible,
  async (isVisible) => {
    if (!isVisible) return;
    query.value = "";
    activeIndex.value = 0;
    await nextTick();
    inputRef.value?.focus();
  },
);

function move(delta: number) {
  const count = results.value.length;
  if (count === 0) return;
  // 循环：在最后一项按下 ↓ 会回到第一项（VS Code 也是这个行为）
  activeIndex.value = (activeIndex.value + delta + count) % count;
  void scrollActiveIntoView();
}

async function scrollActiveIntoView() {
  await nextTick();
  listRef.value?.querySelector<HTMLElement>(".quick-item.active")?.scrollIntoView({
    block: "nearest",
  });
}

function confirm() {
  const hit = results.value[activeIndex.value];
  if (hit) emit("select", hit.item.key);
}

function onKeydown(event: KeyboardEvent) {
  switch (event.key) {
    case "ArrowDown":
      event.preventDefault();
      move(1);
      break;
    case "ArrowUp":
      event.preventDefault();
      move(-1);
      break;
    case "Enter":
      event.preventDefault();
      confirm();
      break;
    case "Escape":
      event.preventDefault();
      emit("close");
      break;
  }
}

/**
 * 把一段文本按命中位置切成片段，用于渲染高亮。
 * 相邻的同类片段会合并成一个 —— 否则 "Save" 全命中时会生成 4 个 span。
 */
function segments(text: string, positions: number[]): Array<{ text: string; hit: boolean }> {
  const hitSet = new Set(positions);
  const out: Array<{ text: string; hit: boolean }> = [];

  for (let index = 0; index < text.length; index += 1) {
    const hit = hitSet.has(index);
    const last = out[out.length - 1];
    if (last && last.hit === hit) last.text += text[index];
    else out.push({ text: text[index], hit });
  }

  return out;
}
</script>

<template>
  <!-- 点浮层外面关掉。mousedown.self 保证点浮层内部不会误关
       （用 mousedown 而不是 click：click 在拖选文本后也会触发，容易误关） -->
  <div v-if="visible" class="quick-backdrop" @mousedown.self="emit('close')">
    <div class="quick-panel">
      <input
        ref="inputRef"
        v-model="query"
        class="quick-input"
        type="text"
        spellcheck="false"
        autocomplete="off"
        :placeholder="placeholder"
        @keydown="onKeydown"
      />

      <div ref="listRef" class="quick-list">
        <p v-if="results.length === 0" class="quick-empty">
          {{ emptyText ?? "没有匹配项" }}
        </p>

        <div
          v-for="(hit, index) in results"
          :key="hit.item.key"
          class="quick-item"
          :class="{ active: index === activeIndex }"
          @mouseenter="activeIndex = index"
          @mousedown.prevent="emit('select', hit.item.key)"
        >
          <span class="quick-text">
            <span
              v-for="(part, partIndex) in segments(hit.item.text, hit.positions)"
              :key="partIndex"
              :class="{ 'quick-hit': part.hit }"
              >{{ part.text }}</span
            >
          </span>
          <span v-if="hit.item.hint" class="quick-hint">{{ hit.item.hint }}</span>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
/* 铺满整个窗口的一层，用来接住「点外面关闭」 */
.quick-backdrop {
  position: fixed;
  inset: 0;
  z-index: 100; /* 比菜单浮层（10）还高 —— 它应该盖住一切 */
  display: flex;
  justify-content: center;
  /* 靠上而不是居中：列表会往下长，居中会让它跳动 */
  align-items: flex-start;
  padding-top: 10vh;
  background: rgba(0, 0, 0, 0.25);
}

.quick-panel {
  display: flex;
  flex-direction: column;
  width: min(600px, 80vw);
  max-height: 60vh;
  border: 1px solid var(--color-menu-border);
  border-radius: 6px;
  background: var(--color-menu-bg);
  box-shadow: 0 8px 24px #00000066;
  overflow: hidden;
}

.quick-input {
  flex: 0 0 auto;
  padding: 10px 14px;
  border: 0;
  /* 只留下面一条分隔线，不画完整边框 —— 输入框和列表是一体的 */
  border-bottom: 1px solid var(--color-menu-border);
  background: transparent;
  color: var(--color-text);
  font: inherit;
  font-size: 13px;
  outline: none;
}

.quick-input::placeholder {
  color: var(--color-text-dim);
}

.quick-list {
  flex: 1 1 auto;
  min-height: 0;
  overflow-y: auto;
  padding: 4px;
}

.quick-empty {
  margin: 10px 12px;
  color: var(--color-text-dim);
  font-size: 12px;
}

.quick-item {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
  padding: 5px 10px;
  border-radius: 4px;
  color: var(--color-text);
  font-size: 13px;
  cursor: pointer;
}

/* 鼠标划过和键盘选中用同一个样式 —— 它们本来就指的是「当前这一项」 */
.quick-item.active {
  background: var(--color-selection);
  color: var(--color-text-on-accent);
}

.quick-text {
  min-width: 0;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}

/* 命中的字符。用下划线而不是加粗或变色：
   选中项的背景是深蓝，变色会看不清；加粗会让文字左右轻微抖动 */
.quick-hit {
  text-decoration: underline;
  text-underline-offset: 2px;
}

.quick-hint {
  flex: 0 0 auto;
  color: var(--color-text-dim);
  font-size: 11px;
}

.quick-item.active .quick-hint {
  color: var(--color-text-on-accent);
}
</style>
