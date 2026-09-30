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

/** 一次搜索的结果（对应 Rust 侧的 SearchResponse） */
export interface SearchResponse {
    matches: SearchMatch[];
    /** 是不是撞到结果上限提前停了 */
    truncated: boolean;
    /** 实际扫了多少个文件 */
    filesScanned: number;
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