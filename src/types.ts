//定义文件夹和文件类型，用|隔开表示二选一，
export interface FileNode {
    name: string;
    path: string;//从工作区根算起的完整路径，作为唯一标识（给 key 和标签页用）
    type: "file" | "folder";//联合类型
    children?: FileNode[];//文件夹用的字段，?表示可选，文件无子节点，不用写字段
}

/** 全文搜索的一条命中（对应 Rust 侧的 SearchMatch） */
export interface SearchMatch {
    path: string;
    /** 1 起的行号 */
    line: number;
    /** 1 起的列号 */
    column: number;
    /** 整行内容，已去掉首尾空白 */
    text: string;
    /**
     * 匹配在 text 里的起止下标。
     * ★ 口径是 **UTF-16 单位**，和 JS 的 `slice()` 一致 ——
     *   所以可以直接拿它切字符串做高亮，不用再换算
     */
    start: number;
    end: number;
}

/**
 * 一条符号命中（对应 Rust 侧的 SymbolRow）。
 *
 * ★ `kind` 用的是 **LSP 的 SymbolKind 编号** —— 将来打开过的文件改用语言服务器
 *   的 `documentSymbol` 覆盖这些启发式结果时，编号不用换算
 */
export interface SymbolRow {
    path: string;
    name: string;
    kind: number;
    /** 1 起的行号 */
    line: number;
    /** 1 起的列号 */
    column: number;
}

/** 一次搜索的结果（对应 Rust 侧的 SearchResponse） */
export interface SearchResponse {
    matches: SearchMatch[];
    /** 是不是撞到结果上限提前停了 */
    truncated: boolean;
    /** 实际扫了多少个文件 */
    filesScanned: number;
}

/**
 * 搜索索引的状态（对应 Rust 侧的 IndexInfo）。
 *
 * ★ 索引是**加速手段**，不是正确性的前提：拿不到 / 建不起来时搜索照常能用，
 *   只是退回全量扫描
 */
export interface IndexInfo {
    /** 索引里的文件总数 */
    files: number;
    /** 其中内容已经建好的 */
    indexed: number;
    /** 还没建好的（这些每次搜索都要现场扫） */
    pending: number;
    /** 超出上限、决定不索引的（同样现场扫，但**不会**再减少） */
    skipped: number;
    /** 上次同步里新建 / 重建了几个 */
    updated: number;
    /** 上次同步里删掉了几个 */
    removed: number;
    /** 上次同步花了多少毫秒 */
    tookMs: number;
    /** 库文件多大（字节） */
    dbBytes: number;
    /** 已进索引的源码字节数。和 dbBytes 一比就知道索引膨胀了多少倍 */
    indexedBytes: number;
    /** 索引是不是真的能用了（false = 一直在走旧路径） */
    built: boolean;
}

/**
 * 菜单项。子菜单和普通项用的是**同一个形状** —— 区别只在有没有 `items`。
 *
 * ★ 为什么放在这里而不是 App.vue：MenuList.vue 要递归地用它，
 *   而 <script setup> 里不能 export 类型
 */
export interface MenuItem {
    /** 分隔线 */
    separator?: boolean;
    label?: string;
    shortcut?: string;
    /**
     * 右侧的次要说明，小字灰字显示（比如最近文件夹的完整路径）。
     * 和 shortcut 的区别：它不表示「按哪个键」，只是补充信息
     */
    hint?: string;
    /**
     * 灰掉且点不动。
     *
     * ★ 不要写成「在 run 里判断一下就好」—— 那样菜单项看起来还是能点的，
     *   点下去没反应比直接置灰更让人困惑（VS Code 里也是灰的）
     */
    disabled?: boolean;
    run?: () => void;
    /**
     * 子菜单。
     *
     * ★ 「有子菜单」和「有动作」是**互斥**的：
     *   带 items 的项自己不执行任何东西，悬停它只是往右弹出下一层。
     *   所以这类项不写 run
     */
    items?: MenuItem[];
}

export interface Menu {
    label: string;
    items: MenuItem[];
}