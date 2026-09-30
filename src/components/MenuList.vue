<!--
  菜单项的渲染器（递归组件）。

  ★ 为什么要单独抽一个组件、而不是在 App.vue 里手写两层：
    「最近打开的文件夹」是个**动态列表** —— 长度不固定、还能带分隔线。
    手写两层的话，以后要加第三层（比如「打开最近 › 某个子目录」）又得重来一遍。
    递归组件是「一层代码管所有层」，和 ACTIVITY_VIEWS / PANEL_TABS 是同一个思路。

  ★ Vue 3 的 <script setup> 允许**自引用**：组件名就是文件名，不用 import 自己
    （FileTreeNode.vue 早就用上这个特性了）

  ★ 子菜单的展开用 **CSS 的 :hover**，不用 JS 状态：
    鼠标离开就自动收起，不用写「移出时收起」的逻辑 —— 那种逻辑最麻烦的地方
    是从父项移到子菜单的**中途**，会先离开父项，一不小心就闪断。
    ⚠ 子菜单因此必须**紧贴**父项（`top: -5px`，不加空隙），否则那段空隙会让 hover 断掉
-->
<script setup lang="ts">
import type { MenuItem } from "../types";

defineProps<{ items: MenuItem[] }>();

// 只往上冒「执行某一项」，不区分层级 —— 递归几层都一样
const emit = defineEmits<{ (e: "run", item: MenuItem): void }>();
</script>

<template>
  <template v-for="(item, index) in items" :key="index">
    <div v-if="item.separator" class="menu-separator" />

    <!-- 带子菜单的项：自己不执行动作，悬停往右弹出下一层 -->
    <div v-else-if="item.items" class="menu-submenu-wrap">
      <button
        class="menu-item"
        type="button"
        :class="{ disabled: item.disabled }"
        :disabled="item.disabled"
      >
        <span>{{ item.label }}</span>
        <span class="menu-arrow">›</span>
      </button>

      <div class="menu-dropdown menu-submenu">
        <MenuList :items="item.items" @run="emit('run', $event)" />
      </div>
    </div>

    <!-- 普通项。:disabled 一设浏览器就不再派发 click 了 ——
         比在 runMenuItem 里再判一次更省事，也顺带拿到原生的可访问性 -->
    <button
      v-else
      class="menu-item"
      type="button"
      :class="{ disabled: item.disabled }"
      :disabled="item.disabled"
      @click="emit('run', item)"
    >
      <span>{{ item.label }}</span>
      <span v-if="item.shortcut" class="menu-shortcut">{{ item.shortcut }}</span>
      <span v-else-if="item.hint" class="menu-hint">{{ item.hint }}</span>
    </button>
  </template>
</template>

<!--
  ★ 为什么这些样式在**这里**、而且**故意不加 scoped**：
    「一个菜单长什么样」这件事，天生横跨两个组件 ——
    顶层那块面板由 App.vue 的模板渲染，子菜单由本组件递归渲染。
    两边都要同一套外观，所以这份样式必须在 scoped 之外。

    ⚠ 如果加了 scoped，会掉进一个**不报错**的坑：
      Vue 会把选择器编译成 `.menu-dropdown[data-v-xxx]`，
      而这个 xxx 是本组件的 id —— App.vue 模板里的那块面板带的是**它自己的** id，
      于是规则一条都匹配不上，菜单会变成一坨没样子的文字。
      scoped 是跟着「模板里的元素」走的，不跟着「组件」走。
      反过来把样式写在 App.vue 里也一样救不了子菜单。

    ⚠ 类名全部带 `menu-` 前缀，不会外泄到别处
-->
<style>
/* 下拉面板。顶层的由 App.vue 的模板直接使用，子菜单的由本组件递归使用 ——
   同一个类名，同一套样式 */
.menu-dropdown {
  position: absolute;
  top: 100%;
  left: 0;
  /* 要盖在编辑器上面 */
  z-index: 10;
  min-width: 220px;
  /* 四周都留 4px：菜单项自己带圆角，得靠这点空隙才看得出来 */
  padding: 4px;
  border: 1px solid var(--color-menu-border);
  border-radius: 5px;
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
  border-radius: 4px;
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

/* 禁用项：灰掉、不响应悬停。
   ⚠ 这两条必须写在 :hover 之后 —— 同优先级时后写的赢，
     不然鼠标移上去还是会亮起来，看起来像能点 */
.menu-item.disabled,
.menu-item.disabled:hover {
  background: transparent;
  color: var(--color-text-dim);
  cursor: default;
}

.menu-item.disabled:hover .menu-shortcut,
.menu-item.disabled:hover .menu-arrow,
.menu-item.disabled:hover .menu-hint {
  color: var(--color-text-dim);
}

/* 次要说明（比如最近文件夹的完整路径）。
   和 shortcut 的区别：它不表示「按哪个键」，只是补充信息 */
.menu-hint {
  max-width: 260px;
  overflow: hidden;
  color: var(--color-text-dim);
  font-size: 11px;
  white-space: nowrap;
  text-overflow: ellipsis;
}

.menu-item:hover .menu-hint {
  color: var(--color-text-on-accent);
}

.menu-separator {
  height: 1px;
  margin: 4px 0;
  background: var(--color-menu-border);
}

/* ---------- 子菜单 ---------- */

/* 子菜单的容器。
   ★ 它是 hover 的「锚」：CSS 的 :hover 是**沿 DOM 树**算的，
     所以鼠标停在父项上、或者停到它弹出的子菜单里，这个 wrapper 都算被悬停 ——
     于是「移出时收起」根本不用写一行 JS，浏览器替我们算了 */
.menu-submenu-wrap {
  position: relative;
}

/* 子菜单往右弹。
   ★ 这几条能覆盖 .menu-dropdown 的 top/left，是因为两者在**同一个 scoped 块**里，
     顺序有保证（写在后面就赢）。
   ⚠ top: -5px 是跟着 .menu-item 的上下 padding（5px）走的：
     让子菜单第一项的**文字**和父项文字齐平。
   ⚠ 必须**紧贴**父项（left: 100%，中间不留缝）—— 留了缝的话，
     鼠标从父项移到子菜单的路上会经过一块「谁都不属于」的区域，
     hover 会断开，子菜单闪一下就没 */
.menu-submenu {
  top: -5px;
  left: 100%;
  min-width: 200px;
  /* 平时藏起来，展开靠下面那条 :hover 规则 */
  display: none;
}

.menu-submenu-wrap:hover > .menu-submenu {
  display: block;
}

/* 有子菜单的项右侧那个指示箭头 */
.menu-arrow {
  color: var(--color-text-dim);
  font-size: 14px;
  line-height: 1;
}

.menu-item:hover .menu-arrow {
  color: var(--color-text-on-accent);
}
</style>
