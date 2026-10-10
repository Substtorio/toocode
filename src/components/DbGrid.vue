<script setup lang="ts">
/**
 * 数据库结果的表格。
 *
 * ★ 抽成组件是因为它要出现**两次以上**（表数据 / SQL 结果）——
 *   抄两遍的话，「NULL 怎么显示」「长文本怎么截」这几条迟早只改一处，
 *   而那种不一致**不报错**，只是两张表看起来微妙地不一样
 */
defineProps<{
  columns: string[];
  rows: Array<Array<string | null>>;
  /** 没有结果时显示什么 */
  empty?: string;
}>();
</script>

<template>
  <div class="db-grid-wrap">
    <p v-if="columns.length === 0" class="db-grid-empty">{{ empty ?? "没有结果" }}</p>

    <table v-else class="db-grid">
      <thead>
        <tr>
          <!-- 行号列。★ 它同时在回答「这一页是从第几行开始的」——
               没有它的话，翻到第二页就看不出这是第 101 行 -->
          <th class="db-grid-rownum"></th>
          <th v-for="column in columns" :key="column" :title="column">{{ column }}</th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="(row, index) in rows" :key="index">
          <td class="db-grid-rownum">{{ index + 1 }}</td>
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

.db-null {
  font-style: italic;
  color: var(--color-text-dim);
}
</style>
