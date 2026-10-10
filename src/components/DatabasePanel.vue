<script setup lang="ts">
/**
 * 数据库视图的浮层。
 *
 * ★ 为什么是**浮层**而不是标签页：
 *   标签页那套数据结构的底层假设是「一个路径 = 一个文件」（`openTabs: string[]`、
 *   `models` 按路径存、脏状态、另存为要搬家……）。一个「库」不是文件、
 *   一个「表」更不是路径，硬塞进去就得给每一处加特例。
 *   VS Code 自己碰上这类东西也是开一个独立面板（比如设置 / 扩展详情），
 *   不动标签体系。
 *   先例：图片预览、欢迎页、diff 审阅，全都是浮层，`openTabs` 一行都没改过
 *
 * ★ 三个标签页（表数据 / 关系图 / SQL）都在这里，不再往外拆：
 *   它们共享同一个「当前库 + 当前表」的状态，拆开就得靠 props 来回传
 */
import { computed, onMounted, ref, watch } from "vue";
import { invoke } from "@tauri-apps/api/core";
import DbGrid from "./DbGrid.vue";
import { formatBytes } from "../format";

interface DbTable {
  name: string;
  kind: string;
  sql: string;
}

interface DbInfo {
  path: string;
  name: string;
  size: number;
  version: string;
  tables: DbTable[];
}

interface DbColumn {
  name: string;
  type: string;
  notNull: number | null;
  primaryKey: number;
  defaultValue: string | null;
}

interface DbIndex {
  name: string;
  unique: boolean;
  columns: string[];
}

interface DbSchema {
  table: string;
  sql: string;
  columns: DbColumn[];
  indexes: DbIndex[];
}

interface DbRows {
  columns: string[];
  rows: Array<Array<string | null>>;
  offset: number;
  total: number;
  totalCapped: boolean;
}

interface DbQueryResult {
  columns: string[];
  rows: Array<Array<string | null>>;
  changes: number;
  truncated: boolean;
  elapsedMs: number;
}

const props = defineProps<{
  /** 库文件路径 */
  path: string;
  /** 显示用的名字 */
  name: string;
  /** 初始打开哪张表（不传就取第一张） */
  table?: string | null;
}>();
const emit = defineEmits<{ (event: "close"): void }>();

const TABS = [
  { id: "rows" as const, label: "表数据" },
  { id: "graph" as const, label: "关系图" },
  { id: "sql" as const, label: "SQL" },
];
const PAGE_SIZE = 100;
const SQL_LIMIT = 500;

const tab = ref<"rows" | "graph" | "sql">("rows");
const info = ref<DbInfo | null>(null);
const error = ref("");
const activeTable = ref("");

const schema = ref<DbSchema | null>(null);
const page = ref(0);
const rows = ref<DbRows | null>(null);
const rowsBusy = ref(false);

const sql = ref("");
const sqlResult = ref<DbQueryResult | null>(null);
const sqlError = ref("");
const sqlBusy = ref(false);

/** 关系图要用到**每一张表**的列，所以单独存一份「表名 → 结构」 */
const schemas = ref<Record<string, DbSchema>>({});

const metaText = computed(() => {
  if (!info.value) return "正在打开…";
  return `${info.value.tables.length} 张表 · ${formatBytes(info.value.size)} · SQLite ${info.value.version}`;
});

const totalPages = computed(() => {
  const total = rows.value?.total ?? 0;
  return Math.max(1, Math.ceil(total / PAGE_SIZE));
});

const rowsLabel = computed(() => {
  if (!rows.value) return "";
  const shown = rows.value.rows.length;
  const total = rows.value.totalCapped ? `${rows.value.total}+` : String(rows.value.total);
  return `第 ${rows.value.offset + (shown ? 1 : 0)}–${rows.value.offset + shown} 行 / 共 ${total} 行`;
});

/** `CREATE TABLE` 里的第一段注释当提示？不必 —— 摘要给列数和主键就够了 */
const tableSummary = computed(() => {
  const current = activeTable.value;
  const found = info.value?.tables.find((item) => item.name === current);
  if (!found) return "";
  const columns = schemas.value[current]?.columns.length ?? 0;
  return `${found.kind} · ${columns} 列`;
});

// ---------- 读库 ----------

async function loadInfo(): Promise<void> {
  error.value = "";
  try {
    info.value = await invoke<DbInfo>("db_open", { path: props.path });
  } catch (err) {
    // ★ 打不开就把原因原样显示：最常见的是「这不是 SQLite 文件」，
    //   而那句话是后端拼好的，比我们在这儿再猜一句强
    error.value = err instanceof Error ? err.message : String(err);
    info.value = null;
    return;
  }

  // ★ 选哪张表：优先调用方指定的，否则第一张。
  //   ⚠ 指定的那张可能已经不存在了（SQL 标签里刚 DROP 掉）—— 那就回落到第一张，
  //     不能直接用它去查，那样会报「no such table」而用户什么也没做错
  const names = info.value.tables.map((item) => item.name);
  const wanted = props.table && names.includes(props.table) ? props.table : (names[0] ?? "");
  if (wanted) await selectTable(wanted);
  else {
    activeTable.value = "";
    rows.value = null;
    schema.value = null;
  }
}

async function selectTable(name: string): Promise<void> {
  activeTable.value = name;
  page.value = 0;
  await Promise.all([loadSchema(name), loadRows()]);
}

async function loadSchema(name: string): Promise<void> {
  try {
    const loaded = await invoke<DbSchema>("db_schema", { path: props.path, table: name });
    schema.value = loaded;
    schemas.value = { ...schemas.value, [name]: loaded };
  } catch {
    // 拿不到结构不该拦住看数据（虚表就不一定有常规索引）
    schema.value = null;
  }
}

async function loadRows(): Promise<void> {
  if (!activeTable.value) return;
  rowsBusy.value = true;
  error.value = "";
  try {
    rows.value = await invoke<DbRows>("db_rows", {
      path: props.path,
      table: activeTable.value,
      offset: page.value * PAGE_SIZE,
      limit: PAGE_SIZE,
    });
  } catch (err) {
    error.value = err instanceof Error ? err.message : String(err);
    rows.value = null;
  } finally {
    rowsBusy.value = false;
  }
}

async function goPage(next: number): Promise<void> {
  if (next < 0 || next >= totalPages.value) return;
  page.value = next;
  await loadRows();
}

// ---------- 关系图 ----------

/**
 * 关系图上的节点。
 *
 * ★ 这里画的**不是** ER 图（我们没有外键），而是**结构图**：
 *   主表在左，它自己的索引和影子表在右，中间用线连起来。
 *   为什么值得画：SQLite 库里「一个表名背后其实有好几张表」这件事，
 *   光看表清单是看不出来的 —— FTS5 就会额外建 `fts_data` / `fts_idx` /
 *   `fts_docsize` / `fts_config` 四张影子表
 */
interface GraphNode {
  id: string;
  label: string;
  detail: string;
  kind: "table" | "view" | "virtual" | "index" | "shadow";
  x: number;
  y: number;
  w: number;
  h: number;
}

interface GraphEdge {
  key: string;
  x1: number;
  y1: number;
  x2: number;
  y2: number;
}

/** FTS5 影子表的名字 —— 它们跟在虚表名后面（`<虚表>_data` 之类） */
const SHADOW_SUFFIXES = ["_data", "_idx", "_docsize", "_config", "_content"];

const graph = computed(() => {
  const tables = info.value?.tables ?? [];
  const names = new Set(tables.map((item) => item.name));

  // 先挑出影子表：它的名字 = 某张虚表名 + 一个已知后缀
  const shadowOf = new Map<string, string>();
  for (const table of tables) {
    if (table.kind !== "virtual") continue;
    for (const suffix of SHADOW_SUFFIXES) {
      const shadow = `${table.name}${suffix}`;
      if (names.has(shadow)) shadowOf.set(shadow, table.name);
    }
  }

  const main = tables.filter((item) => !shadowOf.has(item.name));
  const nodes: GraphNode[] = [];
  const edges: GraphEdge[] = [];

  const PAD = 16;
  const LEFT_W = 220;
  const RIGHT_X = PAD + LEFT_W + 64;
  const RIGHT_W = 240;
  const ROW_H = 22;
  const TITLE_H = 30;
  let y = PAD;

  for (const table of main) {
    const columns = schemas.value[table.name]?.columns ?? [];
    const height = TITLE_H + Math.max(columns.length, 1) * ROW_H + 6;
    const nodeId = `t:${table.name}`;
    nodes.push({
      id: nodeId,
      label: table.name,
      detail: table.kind,
      kind: table.kind === "virtual" ? "virtual" : table.kind === "view" ? "view" : "table",
      x: PAD,
      y,
      w: LEFT_W,
      h: height,
    });

    // 左列：这一张表的列（画在方框内部，见模板）
    // 右列：它的索引 + 它的影子表，竖着排
    let rightY = y;
    const attached: Array<{ id: string; label: string; detail: string; kind: GraphNode["kind"] }> = [];

    for (const index of schemas.value[table.name]?.indexes ?? []) {
      attached.push({
        id: `i:${table.name}:${index.name}`,
        label: index.name,
        detail: (index.unique ? "唯一索引 · " : "索引 · ") + (index.columns.join(", ") || "?"),
        kind: "index",
      });
    }
    for (const [shadow, owner] of shadowOf) {
      if (owner !== table.name) continue;
      attached.push({
        id: `s:${shadow}`,
        label: shadow,
        detail: "影子表",
        kind: "shadow",
      });
    }

    for (const item of attached) {
      const node: GraphNode = { ...item, x: RIGHT_X, y: rightY, w: RIGHT_W, h: 34 };
      nodes.push(node);
      edges.push({
        key: `${nodeId}->${item.id}`,
        x1: PAD + LEFT_W,
        y1: y + Math.min(TITLE_H + 10, height - 10),
        x2: RIGHT_X,
        y2: rightY + 17,
      });
      rightY += 34 + 8;
    }

    y = Math.max(y + height, rightY) + 28;
  }

  return {
    nodes,
    edges,
    width: RIGHT_X + RIGHT_W + PAD,
    height: Math.max(y + PAD, 120),
  };
});

/** 切到关系图时才去补结构（要一张一张查） */
async function ensureSchemas(): Promise<void> {
  for (const table of info.value?.tables ?? []) {
    if (schemas.value[table.name]) continue;
    await loadSchema(table.name);
  }
  // ⚠ 最后要把 `schema` 还原成「当前表」的那一份：`loadSchema` 顺手改了它，
  //   不还原的话从关系图切回表数据，右侧显示的列会变成最后一张表的
  if (activeTable.value) {
    schema.value = schemas.value[activeTable.value] ?? null;
  }
}

watch(tab, (next) => {
  if (next === "graph") void ensureSchemas();
});

/**
 * 侧栏里点了**同一个库的另一张表**时，这个组件已经挂着了 ——
 * 不监听的话 props.table 变了也没人理，界面还是旧表。
 *
 * ⚠ 得确认那张表真的存在再去查：它可能在 SQL 标签里刚被 DROP 掉
 */
watch(
  () => props.table,
  (next) => {
    if (!next || next === activeTable.value) return;
    if (!info.value?.tables.some((item) => item.name === next)) return;
    void selectTable(next);
  },
);

// ---------- 执行 SQL ----------

async function runSql(): Promise<void> {
  const text = sql.value.trim();
  if (!text || sqlBusy.value) return;

  sqlBusy.value = true;
  sqlError.value = "";
  sqlResult.value = null;
  try {
    const result = await invoke<DbQueryResult>("db_query", {
      path: props.path,
      sql: text,
      limit: SQL_LIMIT,
    });
    sqlResult.value = result;

    // ★ 语句可能改了结构或数据（DROP / INSERT / UPDATE）⇒ 表的清单和当前页都过期了。
    //   不刷新的话，界面会显示一个「其实已经不存在」的表
    //   ⚠ 只有真的可能改动了才刷：纯 SELECT 没必要多跑一趟
    if (result.columns.length === 0 || result.changes > 0) {
      await refreshAfterWrite();
    }
  } catch (err) {
    sqlError.value = err instanceof Error ? err.message : String(err);
  } finally {
    sqlBusy.value = false;
  }
}

/**
 * 写操作之后重新对齐表清单。
 *
 * ⚠ 这里**不能**直接调 `loadInfo()` —— 那个函数会按 `props.table` 重新选表，
 *   于是你正看着的表会被悄悄换成第一张（用户只是往一张表里插了一行）
 */
async function refreshAfterWrite(): Promise<void> {
  try {
    const fresh = await invoke<DbInfo>("db_open", { path: props.path });
    info.value = fresh;
    const names = fresh.tables.map((item) => item.name);

    if (activeTable.value && names.includes(activeTable.value)) {
      schemas.value = {}; // 结构可能变了，缓存全部作废
      await Promise.all([loadSchema(activeTable.value), loadRows()]);
      return;
    }
    // 当前表真被删了，才回落到第一张
    if (names[0]) await selectTable(names[0]);
    else {
      activeTable.value = "";
      rows.value = null;
      schema.value = null;
    }
  } catch {
    // 刷新失败不影响已经拿到的 SQL 结果
  }
}

onMounted(() => {
  void loadInfo();
});
</script>

<template>
  <div class="db-view">
    <div class="db-bar">
      <span class="db-name" :title="path">{{ name }}</span>

      <div class="db-tabs">
        <button
          v-for="item in TABS"
          :key="item.id"
          class="db-tab"
          type="button"
          :class="{ active: tab === item.id }"
          @click="tab = item.id"
        >
          {{ item.label }}
        </button>
      </div>

      <span class="db-meta">{{ metaText }}</span>
      <button class="db-close" type="button" title="关闭" @click="emit('close')">×</button>
    </div>

    <p v-if="error" class="db-error">{{ error }}</p>

    <!-- 表数据：工具栏 + 表格 -->
    <template v-if="tab === 'rows'">
      <div class="db-toolbar">
        <select v-model="activeTable" class="db-select" @change="selectTable(activeTable)">
          <option v-for="item in info?.tables ?? []" :key="item.name" :value="item.name">
            {{ item.name }}
          </option>
        </select>
        <span class="db-meta">{{ tableSummary }}</span>
        <span class="db-meta">{{ rowsLabel }}</span>
        <div class="db-spacer"></div>
        <button class="db-button" type="button" :disabled="page === 0 || rowsBusy" @click="goPage(page - 1)">
          上一页
        </button>
        <button
          class="db-button"
          type="button"
          :disabled="page + 1 >= totalPages || rowsBusy"
          @click="goPage(page + 1)"
        >
          下一页
        </button>
      </div>

      <div class="db-body">
        <!-- 列定义单独摆一条：表格里只有数据，看结构一眼就够 -->
        <div v-if="schema && schema.columns.length" class="db-columns">
          <span v-for="column in schema.columns" :key="column.name" class="db-column" :title="column.type">
            <span v-if="column.primaryKey" class="db-pk">PK</span>{{ column.name }}
          </span>
        </div>
        <div class="db-grid-host">
          <DbGrid
            :columns="rows?.columns ?? []"
            :rows="rows?.rows ?? []"
            :empty="rowsBusy ? '正在读取…' : '这张表还没有数据'"
          />
        </div>
      </div>
    </template>

    <!-- 关系图：纯 SVG，位置全自己算（不做力导向 —— 那需要的代码比这个视图本身还多） -->
    <div v-else-if="tab === 'graph'" class="db-body db-graph-host">
      <svg :width="graph.width" :height="graph.height" class="db-graph">
        <!-- 先画线，再画框：线压在框下面，看起来才像「连过去」而不是「穿过去」 -->
        <line
          v-for="edge in graph.edges"
          :key="edge.key"
          :x1="edge.x1"
          :y1="edge.y1"
          :x2="edge.x2"
          :y2="edge.y2"
          class="db-edge"
        />

        <g v-for="node in graph.nodes" :key="node.id">
          <rect
            :x="node.x"
            :y="node.y"
            :width="node.w"
            :height="node.h"
            rx="4"
            class="db-node"
            :class="`db-node-${node.kind}`"
          />
          <text :x="node.x + 10" :y="node.y + 19" class="db-node-title">{{ node.label }}</text>
          <text :x="node.x + node.w - 10" :y="node.y + 19" class="db-node-detail" text-anchor="end">
            {{ node.detail }}
          </text>

          <!-- 表节点里面列字段名（只主表有，索引 / 影子表没有列） -->
          <template v-if="node.kind === 'table' || node.kind === 'virtual' || node.kind === 'view'">
            <text
              v-for="(column, index) in schemas[node.label]?.columns ?? []"
              :key="column.name"
              :x="node.x + 14"
              :y="node.y + 30 + (index + 1) * 22 - 5"
              class="db-node-column"
            >
              {{ column.name }}<tspan v-if="column.primaryKey" class="db-node-detail"> · PK</tspan>
            </text>
          </template>
        </g>
      </svg>
    </div>

    <!-- SQL -->
    <div v-else class="db-body db-sql-body">
      <div class="db-sql-box">
        <textarea
          v-model="sql"
          class="db-sql-input"
          spellcheck="false"
          placeholder="写一条 SQL，按 Ctrl+Enter 执行（会改数据的语句也会真的执行）"
          @keydown.ctrl.enter.prevent="runSql"
        ></textarea>
        <div class="db-sql-actions">
          <span class="db-meta">Ctrl+Enter 执行 · 最多返回 {{ SQL_LIMIT }} 行</span>
          <div class="db-spacer"></div>
          <button class="db-button" type="button" :disabled="sqlBusy || !sql.trim()" @click="runSql">
            {{ sqlBusy ? "执行中…" : "执行" }}
          </button>
        </div>
      </div>

      <p v-if="sqlError" class="db-error">{{ sqlError }}</p>
      <!-- ★ 写操作没有结果集，但要明确说「改了几行」——
           不说的话界面一片空白，用户不知道该不该相信它执行了 -->
      <p v-else-if="sqlResult && sqlResult.columns.length === 0" class="db-ok">
        执行成功，影响 {{ sqlResult.changes }} 行（{{ sqlResult.elapsedMs }} ms）
      </p>

      <div class="db-result-host">
        <DbGrid
          v-if="sqlResult && sqlResult.columns.length"
          :columns="sqlResult.columns"
          :rows="sqlResult.rows"
          empty="查询没有返回任何行"
        />
        <DbGrid v-else-if="!sqlResult && !sqlError" :columns="[]" :rows="[]" empty="执行一条 SELECT 试试" />
      </div>
    </div>
  </div>
</template>

<style scoped>
/* 盖在编辑器上的一层。z-index 和 .diff-review 同档（见 App.vue 那条注释） */
.db-view {
  position: absolute;
  inset: var(--tabbar-offset, 0px) 0 0 0;
  z-index: 2;
  display: flex;
  flex-direction: column;
  background: var(--color-editor-bg);
  color: var(--color-text);
}

.db-bar {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 4px 8px;
  border-bottom: 1px solid var(--color-menu-border);
  background: var(--color-tabbar-bg);
}

.db-name {
  max-width: 220px;
  overflow: hidden;
  font-size: 12px;
  font-weight: 600;
  white-space: nowrap;
  text-overflow: ellipsis;
}

.db-tabs {
  display: flex;
  align-items: center;
  gap: 2px;
}

.db-tab {
  height: 22px;
  padding: 0 10px;
  border: none;
  border-radius: 3px;
  background: transparent;
  color: var(--color-text);
  font-family: inherit;
  font-size: 12px;
  cursor: pointer;
}

.db-tab:hover {
  background: var(--color-hover);
}

.db-tab.active {
  background: var(--color-selection);
  color: var(--color-text-on-accent);
}

.db-meta {
  color: var(--color-text-dim);
  font-size: 11px;
  white-space: nowrap;
}

.db-spacer {
  flex: 1;
}

.db-close {
  height: 22px;
  padding: 0 8px;
  border: 1px solid var(--color-menu-border);
  border-radius: 3px;
  background: transparent;
  color: var(--color-text);
  font-family: inherit;
  font-size: 14px;
  line-height: 1;
  cursor: pointer;
}

.db-close:hover {
  background: var(--color-hover);
}

.db-toolbar {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 4px 8px;
  border-bottom: 1px solid var(--color-menu-border);
}

.db-select {
  height: 24px;
  max-width: 260px;
  padding: 0 4px;
  border: 1px solid var(--color-menu-border);
  border-radius: 3px;
  background: var(--color-menu-bg);
  color: var(--color-text);
  font-family: inherit;
  font-size: 12px;
}

.db-button {
  height: 22px;
  padding: 0 10px;
  border: 1px solid var(--color-menu-border);
  border-radius: 3px;
  background: transparent;
  color: var(--color-text);
  font-family: inherit;
  font-size: 12px;
  cursor: pointer;
}

.db-button:hover:not(:disabled) {
  background: var(--color-hover);
}

.db-button:disabled {
  opacity: 0.45;
  cursor: default;
}

/* 表格那一段要能自己滚（不撑破浮层）—— min-height: 0 是关键 */
.db-body {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
}

.db-columns {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  padding: 5px 8px;
  border-bottom: 1px solid var(--color-menu-border);
  background: var(--color-tabbar-bg);
}

.db-column {
  font-size: 11px;
  color: var(--color-text-dim);
}

.db-pk {
  margin-right: 3px;
  color: var(--color-problem-warning, #cca700);
  font-weight: 600;
}

.db-grid-host,
.db-result-host {
  flex: 1;
  min-height: 0;
}

.db-error,
.db-ok {
  margin: 0;
  padding: 6px 10px;
  border-bottom: 1px solid var(--color-menu-border);
  font-size: 12px;
}

.db-error {
  color: var(--color-problem-error, #f14c4c);
  /* ★ 出错信息原样显示、允许换行：SQL 报错里那句「near "FROM": syntax error」
     才是唯一有用的线索，截断了就等于没说 */
  white-space: pre-wrap;
}

.db-ok {
  color: var(--color-git-new, #73c991);
}

/* ---------- 关系图 ---------- */

.db-graph-host {
  overflow: auto;
  background: var(--color-editor-bg);
}

.db-graph {
  display: block;
}

.db-node {
  fill: var(--color-menu-bg);
  stroke: var(--color-menu-border);
  stroke-width: 1;
}

.db-node-virtual {
  stroke-dasharray: 4 3;
}

.db-node-index,
.db-node-shadow {
  fill: transparent;
  stroke: var(--color-menu-border);
  stroke-dasharray: 2 2;
}

.db-edge {
  stroke: var(--color-menu-border);
  stroke-width: 1;
  stroke-dasharray: 3 3;
}

.db-node-title {
  fill: var(--color-text);
  font-size: 12px;
  font-weight: 600;
}

.db-node-column {
  fill: var(--color-text);
  font-size: 11px;
}

.db-node-detail {
  fill: var(--color-text-dim);
  font-size: 10px;
}

/* ---------- SQL ---------- */

.db-sql-body {
  padding: 8px;
  gap: 8px;
}

.db-sql-box {
  display: flex;
  flex-direction: column;
  border: 1px solid var(--color-menu-border);
  border-radius: 4px;
  overflow: hidden;
}

.db-sql-input {
  height: 84px;
  padding: 6px 8px;
  border: none;
  background: var(--color-editor-bg);
  color: var(--color-text);
  font-family: Consolas, "Cascadia Mono", monospace;
  font-size: 12px;
  line-height: 1.5;
  resize: vertical;
}

.db-sql-input:focus {
  outline: none;
}

.db-sql-actions {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 4px 6px;
  border-top: 1px solid var(--color-menu-border);
  background: var(--color-tabbar-bg);
}

.db-result-host {
  border: 1px solid var(--color-menu-border);
  border-radius: 4px;
}
</style>
