import type { InjectionKey, Ref } from "vue";

/**
 * 侧杨文件树需要知道的「当前激活文件」信息。
 *
 * 为什么单独放一个文件而不是定义在 App.vue 里：
 * App.vue 已经 import 了 FileTreeNode，如果 FileTreeNode 反过来 import App.vue 拿这个 key，
 * 就形成循环依赖 —— key 可能在 FileTreeNode 模块求值时还没初始化好，拿到 undefined。
 */
export interface FileTreeSelection {
  /** 当前激活文件的完整路径；没有打开任何文件时为 null */
  activePath: Ref<string | null>;
  /** 打开一个文件（已经在标签栏里的就只切换激活，不重复添加） */
  openFile: (path: string) => void;
}

// 用 Symbol 而不是字符串当 key：Symbol 天然唯一，不会和别处定义的 key 撞名
export const fileTreeSelectionKey: InjectionKey<FileTreeSelection> = Symbol("fileTreeSelection");

/**
 * 文件树的展开状态。
 *
 * ★ 为什么要从组件里搬出来：原来 `expanded` 是 `FileTreeNode` 自己的 `ref`，
 *   每个节点管自己的。那样很干净，但有个代价 —— **重建整棵树 = 清空所有展开状态**。
 *   而「另存为之后刷新树」必然要重建，于是用户辛苦展开的一堆目录会全部折起来。
 *
 *   提升成一份「按路径记的集合」之后，重载树时只要路径没变，
 *   节点读到的还是 true —— 不需要任何额外的「保留逻辑」。
 */
export interface FileTreeExpansion {
  /** 已经展开的目录路径集合 */
  expandedPaths: Ref<Set<string>>;
  /** 切换某个目录的展开状态 */
  toggle: (path: string) => void;
}

export const fileTreeExpansionKey: InjectionKey<FileTreeExpansion> = Symbol("fileTreeExpansion");
