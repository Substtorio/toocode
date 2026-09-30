import type { InjectionKey, Ref } from "vue";
import type { FileNode } from "./types";

/**
 * 侧栏文件树需要知道的「当前激活文件」信息。
 *
 * 为什么单独放一个文件而不是定义在 App.vue 里：
 * App.vue 已经 import 了 FileTreeNode，如果 FileTreeNode 反过来 import App.vue 拿这个 key，
 * 就形成循环依赖 —— key 可能在 FileTreeNode 模块求值时还没初始化好，拿到 undefined。
 */
export interface FileTreeSelection {
  /** 当前激活文件的完整路径；没有打开任何文件时为 null */
  activePath: Ref<string | null>;
  /** 打开一个文件（已经在标签栏里的就只切换激活，不重复添加） */
  openFile: (node: FileNode) => void;
}

// 用 Symbol 而不是字符串当 key：Symbol 天然唯一，不会和别处定义的 key 撞名
export const fileTreeSelectionKey: InjectionKey<FileTreeSelection> = Symbol("fileTreeSelection");
