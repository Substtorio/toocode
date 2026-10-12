<script setup lang="ts">
import { computed, onUnmounted, ref } from "vue";

/**
 * 数据库结果的表格。
 *
 * ★ 抽成组件是因为它要出现**两次以上**（表数据 / SQL 结果）——
 *   抄两遍的话，「NULL 怎么显示」「长文本怎么截」这几条迟早只改一处，
 *   而那种不一致**不报错**，只是两张表看起来微妙地不一样
 *
 * ★ 排序是**可选**的（`sortable`）：SQL 标签里的结果不给排 ——
 *   那条 SQL 是用户自己写的，要排序就在语句里写 `ORDER BY`，
 *   界面再插一手反而和他写的东西对不上
 */
const props = defineProps<{
  columns: string[];
  rows: Array<Array<string | null>>;
  /** 没有结果时显示什么 */
  empty?: string;
  /** 点了列头能不能排序 */
  sortable?: boolean;
  /** 当前按哪一列排（null = 没排） */
  sortKey?: string | null;
  /** 是不是降序 */
  sortDescending?: boolean;
  /** 这一页从第几行开始（行号要接着往下数，而不是每页都从 1 开始） */
  offset?: number;
  /**
   * 列名 → 附加说明（类型 / PK / NOT NULL / 默认值）。表头里显示在列名下面。
   *
   * ★ 为什么不另开一行显示：一个 `th` 里放两行，`position: sticky` 天然就对；
   *   拆成两行（用 `rowspan`）的话第二行得自己算「贴在多少像素处」
   */
  columnNotes?: Record<string, string>;
}>();

const emit = defineEmits<{(event: "sort", key: string | null, descending: boolean): void}>();

/**
 * 列头是**三态**的：升序 → 降序 → 不排。
 *
 * ★ 为什么要「不排」这一态：没有它的话点一下就只能永远排着某一列，
 *   而「按数据库自己的顺序看」是个真实需求（尤其是翻了页之后想回到原样）
 */
function onHeaderClick(column: string) {
  if (!props.sortable) return;
  if (props.sortKey !== column) emit("sort", column, false);
  else if (!props.sortDescending) emit("sort", column, true);
  else emit("sort", null, false);
}

// ---------- 双击复制 ----------
//
// ★ 为什么不自己做「选区 + Ctrl+C」：那要实现一整套「哪些格子被选中」的状态
//   （点、拖、Shift 点、全选……），而表格本来就不该变成一个迷你 Excel。
//   双击复制一格解决的是真实需求（把某个值拿走）
const copiedKey = ref("");
const copyHint = ref("");
let copyTimer: number | undefined;

async function copyCell(cell: string | null, key: string) {
  if (cell === null) {
    // ★ NULL 不进剪贴板：复制出一个空字符串，贴到别处看不出
    //   「这里本来就没值」还是「复制失败了」
    copyHint.value = "这一格是 NULL，没有内容可复制";
  } else {
    try {
      await navigator.clipboard.writeText(cell);
      copiedKey.value = key;
      copyHint.value = "已复制";
    } catch (error) {
      // 剪贴板在非安全上下文 / 没有焦点时会直接抛
      copyHint.value = `复制失败：${String(error)}`;
    }
  }

  window.clearTimeout(copyTimer);
  copyTimer = window.setTimeout(() => {
    copyHint.value = "";
    copiedKey.value = "";
  }, 1500);
}

const hasNotes = computed(() => props.columns.some((column) => props.columnNotes?.[column]));

onUnmounted(() => window.clearTimeout(copyTimer));
</script>

<template>
  <div class="db-grid-root">
    <!-- 复制反馈。★ 用绝对定位浮在右下角 ——
         占一行的话它出现 / 消失时表格会跳一下 -->
    <div v-if="copyHint" class="db-grid-hint">{{ copyHint }}</div>

    <div class="db-grid-wrap">
      <p v-if="columns.length === 0" class="db-grid-empty">{{ empty ?? "没有结果" }}</p>

      <table v-else class="db-grid">
        <thead>
          <tr>
            <!-- 行号列。
                 ★ 它用的是**绝对行号**（offset + 行内序号），不是每页从 1 重数 ——
                   否则翻到第二页会看到又是 1、2、3，看不出这是第 101 行 -->
            <th class="db-grid-rownum"></th>
            <th
              v-for="column in columns"
              :key="column"
              :class="{ 'db-grid-sortable': sortable, 'db-grid-sorted': sortKey === column }"
              @click="onHeaderClick(column)"
            >
              <span class="db-grid-head-label">
                {{ column }}<span v-if="sortKey === column" class="db-grid-sort-mark">{{ sortDescending ? "▾" : "▴" }}</span>
              </span>
              <!-- 表头第二行：类型 / PK / NOT NULL。
                   ★ 不悬停就能看到「这张表长什么样」 —— 而这是看库时最先要问的事 -->
              <span v-if="hasNotes" class="db-grid-head-note" :title="columnNotes?.[column]">
                {{ columnNotes?.[column] }}
              </span>
            </th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="(row, index) in rows" :key="index">
            <td class="db-grid-rownum">{{ (offset ?? 0) + index + 1 }}</td>
            <td
              v-for="(cell, cellIndex) in row"
              :key="cellIndex"
              :title="cell === null ? 'NULL' : cell"
              :class="{ 'db-grid-copied': copiedKey === `${index}:${cellIndex}` }"
              @dblclick="copyCell(cell, `${index}:${cellIndex}`)"
            >
              <!-- ★ NULL 和「空字符串」必须能区分 —— SQL 里它们是两回事。
                   都渲染成空白的话，用户会以为数据丢了 -->
              <span v-if="cell === null" class="db-null">NULL</span>
              <span v-else>{{ cell }}</span>
            </td>
          </tr>
        </tbody>
      </table>
    </div>
  </div>
</template>

<style scoped>
.db-grid-root {
  position: relative;
  height: 100%;
}

.db-grid-wrap {
  height: 100%;
  overflow: auto;
}

.db-grid-hint {
  position: absolute;
  right: 14px;
  bottom: 14px;
  z-index: 2;
  padding: 3px 10px;
  border: 1px solid var(--color-menu-border);
  border-radius: 3px;
  background: var(--color-menu-bg);
  color: var(--color-text);
  font-size: 11px;
  /* 提示条只是反馈，不该挡住它下面的格子 */
  pointer-events: none;
}

.db-grid-empty {
  margin: 24px;
  color: var(--color-text-dim);
  font-size: 12px;
}

.db-grid {
  /* ★ border-collapse 而不是 separate：表头和单元格之间不留缝，
     滚动时那条线才不会一半有一半没 */
  border-collapse: collapse;
  font-size: 12px;
  color: var(--color-text);
  /* 让列宽跟着内容走，但整体不超过容器 */
  width: max-content;
  min-width: 100%;
}

.db-grid th,
.db-grid td {
  padding: 3px 10px;
  border-right: 1px solid var(--color-menu-border);
  border-bottom: 1px solid var(--color-menu-border);
  text-align: left;
  /* ★ 一格里的内容不换行、超出截断：允许换行的话，一个长文本格
     能占十几行，整个表就成了一堵墙（和搜索结果列表那条同一个理由） */
  max-width: 320px;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
}

.db-grid th {
  position: sticky;
  top: 0;
  z-index: 1;
  background: var(--color-tabbar-bg);
  font-weight: 600;
  /* ★ 底部对齐：行号列的空 th 会比别的矮一行，
     底部对齐之后它下面的边框和邻居在同一个高度上 */
  vertical-align: bottom;
}

/* 表头里的两行：列名在上，类型 / PK / NOT NULL 在下 */
.db-grid-head-label {
  display: block;
}

.db-grid-head-note {
  display: block;
  margin-top: 1px;
  color: var(--color-text-dim);
  font-size: 10px;
  font-weight: 400;
}

/* 双击复制之后闪一下底色 —— 不然完全不知道复制的是哪一格 */
.db-grid-copied {
  background: var(--color-selection);
  color: var(--color-text-on-accent);
}

.db-grid .db-grid-rownum {
  /* 行号列窄一点、压暗、不参与选中 —— 它是坐标不是数据 */
  padding: 3px 8px;
  color: var(--color-text-dim);
  text-align: right;
  background: var(--color-tabbar-bg);
  position: sticky;
  left: 0;
}

/* 可排序的列头。悬停给底色 —— 不然根本不知道它能点 */
.db-grid-sortable {
  cursor: pointer;
  user-select: none;
}

.db-grid-sortable:hover {
  background: var(--color-hover);
}

.db-grid-sorted {
  color: var(--color-text-emphasis);
}

.db-grid-sort-mark {
  margin-left: 4px;
  color: var(--color-selection);
}

.db-null {
  font-style: italic;
  color: var(--color-text-dim);
}
</style>
