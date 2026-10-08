<!--
  配置界面里用的下拉组合框（自绘）。

  ★★ 为什么不用现成的原生控件：
    · `<select>` 的箭头**是系统画的**，样式改不了（只能整个隐藏）
    · `<input list>` 那个 datalist 箭头又是 **Chromium 另画的一套**
      ⇒ 两个并排放着就是两个样子，而「统一」这件事没有任何 API 能做
    · 更要紧的是：datalist 那个箭头**只能展开，点第二次收不起来**
  ⇒ 所以自己画一个：箭头和文件树共用同一条 codicon `chevron-down`
    （见 `../icons.ts`），浮层和「点开 / 点关」都由我们自己说了算。

  ★ 两种形态，同一个外形：
    · `editable` —— 是个输入框，可以手打任意值（第三方接口的模型名就靠它）
    · 否则 —— 是个按钮，只能从候选里挑
-->
<script setup lang="ts">
import { computed, onUnmounted, ref, watch } from "vue";
import { CHEVRON_DOWN } from "../icons";

const props = defineProps<{
  modelValue: string;
  options: ReadonlyArray<{ id: string; label: string }>;
  /** true = 带输入框（可手打）；false = 只读按钮（只能从候选里挑） */
  editable?: boolean;
  placeholder?: string;
  /** 给读屏用的名字。外面那一行是 div 不是 label，所以这里得自己带上 */
  label?: string;
}>();

const emit = defineEmits<{ (e: "update:modelValue", value: string): void }>();

const open = ref(false);
/** 浮层往上弹还是往下弹，见 toggle() 里的说明 */
const openUp = ref(false);
const rootEl = ref<HTMLElement | null>(null);

/** 按钮形态下显示什么：认识的显示版本名，不认识的照原样 */
const currentLabel = computed(
  () => props.options.find((option) => option.id === props.modelValue)?.label ?? props.modelValue,
);

/**
 * 点箭头 = 开关。
 *
 * ★★ 这就是原来那个 datalist 做不到的事 —— 它的箭头只能展开。
 * ★ 顺带一个「下方放不下就往上弹」：这个浮层是 DOM 里的普通元素，
 *   会被面板的圆角裁掉；而原生 `<select>` 的弹层是系统画的，不受影响。
 *   所以这里得自己看一眼剩余空间 —— 菜单被裁掉就点不到了
 */
function toggle() {
  if (open.value) {
    open.value = false;
    return;
  }
  const rect = rootEl.value?.getBoundingClientRect();
  openUp.value = rect !== undefined && window.innerHeight - rect.bottom < 200;
  open.value = true;
}

function close() {
  open.value = false;
}

function pick(id: string) {
  emit("update:modelValue", id);
  close();
}

/**
 * 点别处关掉。
 *
 * ⚠ 用 **capture 阶段的 `mousedown`**，不是 `click`：
 *   点另一行的箭头时，`mousedown` 会先把这一层关掉、随后那个 `click` 再把
 *   那一层打开 —— 顺序正好，不会出现「关一个又顺手开一个」。
 *   （这和「点别处收起会话列表」那次是同一个坑）
 * ⚠ 判断用 `contains` 而不是「点了谁」—— 浮层里的选项也在 rootEl 里，
 *   不判的话点选项会先被关掉，那一项就永远选不中
 */
function onDocMouseDown(event: MouseEvent) {
  if (rootEl.value !== null && !rootEl.value.contains(event.target as Node)) close();
}

// 只在自己开着的时候才挂全局监听 —— 常驻的话每张页面点多一下都要跑一遍它
watch(open, (value) => {
  if (value) document.addEventListener("mousedown", onDocMouseDown, true);
  else document.removeEventListener("mousedown", onDocMouseDown, true);
});

onUnmounted(() => document.removeEventListener("mousedown", onDocMouseDown, true));
</script>

<template>
  <div ref="rootEl" class="combo" :class="{ 'combo-up': openUp }">
    <!-- 输入框形态：值直接写回外面（没有本地副本，「唯一真相」在 config.model） -->
    <input
      v-if="editable"
      class="combo-control"
      type="text"
      spellcheck="false"
      :value="modelValue"
      :placeholder="placeholder"
      :aria-label="label"
      @input="emit('update:modelValue', ($event.target as HTMLInputElement).value)"
      @keydown.esc="close"
      @keydown.down.prevent="toggle"
    />
    <button
      v-else
      type="button"
      class="combo-control combo-button"
      :aria-label="label"
      :aria-expanded="open"
      @click="toggle"
      @keydown.esc="close"
    >
      {{ currentLabel }}
    </button>

    <!-- 箭头是个**真按钮**（要能点开也要能点关），只是长得像装饰 -->
    <button
      type="button"
      class="combo-arrow"
      tabindex="-1"
      aria-hidden="true"
      @click="toggle"
    >
      <svg viewBox="0 0 16 16">
        <path :d="CHEVRON_DOWN" />
      </svg>
    </button>

    <ul v-if="open" class="combo-menu">
      <li v-for="option in options" :key="option.id">
        <button
          type="button"
          class="combo-option"
          :class="{ active: option.id === modelValue }"
          @click="pick(option.id)"
        >
          <span class="combo-option-label">{{ option.label }}</span>
          <!-- 版本名和字段名不一样时才多露一行 id —— 排错（404）看的就是它 -->
          <span v-if="option.label !== option.id" class="combo-option-id">{{ option.id }}</span>
        </button>
      </li>
    </ul>
  </div>
</template>

<style scoped>
.combo {
  /* 浮层要相对它定位 */
  position: relative;
  display: flex;
}

.combo-control {
  flex: 1;
  /* ⚠ min-width 不写 0 的话，flex 子项不会缩到内容宽度以下，浮层右侧会对不上 */
  min-width: 0;
  height: 26px;
  box-sizing: border-box;
  /* 右边给箭头让位置，不然长文本会一头钻到箭头底下 */
  padding: 0 24px 0 8px;
  border: 1px solid var(--color-menu-border);
  border-radius: 3px;
  background: var(--color-editor-bg);
  color: var(--color-text);
  font-family: inherit;
  font-size: 12px;
  text-align: left;
  outline: none;
}

.combo-button {
  cursor: pointer;
}

.combo-control:focus {
  border-color: var(--color-link);
}

.combo-arrow {
  position: absolute;
  top: 1px;
  right: 1px;
  bottom: 1px;
  width: 22px;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 0;
  border: none;
  border-radius: 0 2px 2px 0;
  background: none;
  color: var(--color-text-dim);
  cursor: pointer;
}

.combo-arrow:hover {
  color: var(--color-text-emphasis);
}

/* codicon 是**填充**图形：给 fill，不给 stroke */
.combo-arrow svg {
  width: 10px;
  height: 10px;
  fill: currentColor;
}

.combo-menu {
  position: absolute;
  top: calc(100% + 2px);
  left: 0;
  right: 0;
  /* 比同一层里的其它东西都高，但不追 z-index —— 它和浮层的规则一样：
     只要比「同场的东西」大就行 */
  z-index: 30;
  margin: 0;
  padding: 4px;
  list-style: none;
  max-height: 180px;
  overflow-y: auto;
  background: var(--color-menu-bg);
  border: 1px solid var(--color-menu-border);
  border-radius: 5px;
  box-shadow: 0 2px 10px rgb(0 0 0 / 30%);
}

/* 下方放不下的那种情况（见 toggle()） */
.combo-up .combo-menu {
  top: auto;
  bottom: calc(100% + 2px);
}

.combo-option {
  display: flex;
  flex-direction: column;
  gap: 1px;
  width: 100%;
  padding: 4px 6px;
  border: none;
  border-radius: 4px;
  background: none;
  color: var(--color-text);
  font-family: inherit;
  font-size: 12px;
  text-align: left;
  cursor: pointer;
}

.combo-option:hover {
  background: var(--color-selection);
  color: var(--color-text-on-accent);
}

.combo-option.active {
  color: var(--color-link);
}

.combo-option.active:hover {
  color: var(--color-text-on-accent);
}

.combo-option-id {
  font-size: 10px;
  color: var(--color-text-dim);
}

.combo-option:hover .combo-option-id {
  /* 悬停时底色变成选中蓝，那两行文字得一起翻白 ——
     只翻标题的话副标题会在蓝底上发糊 */
  color: inherit;
}
</style>
