<script setup lang="ts">
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
</script>

<template>
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
            :title="column"
            :class="{ 'db-grid-sortable': sortable, 'db-grid-sorted': sortKey === column }"
            @click="onHeaderClick(column)"
          >
            {{ column }}<span v-if="sortKey === column" class="db-grid-sort-mark">{{ sortDescending ? "▾" : "▴" }}</span>
          </th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="(row, index) in rows" :key="index">
          <td class="db-grid-rownum">{{ (offset ?? 0) + index + 1 }}</td>
          <td v-for="(cell, cellIndex) in row" :key="cellIndex">
            <!-- ★ NULL 和「空字符串」必须能区分 —— SQL 里它们是两回事。
                 都渲染成空白的话，用户会以为数据丢了 -->
            <span v-if="cell === null" class="db-null">NULL</span>
            <span v-else :title="cell">{{ cell }}</span>
          </td>
        </tr>
      </tbody>
    </table>
  </div>
</template>

<style scoped>
.db-grid-wrap {
  height: 100%;
  overflow: auto;
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
