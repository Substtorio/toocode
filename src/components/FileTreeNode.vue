<script setup lang="ts">
import { computed, inject } from "vue";
import type { FileNode } from "../types";
import { fileTreeExpansionKey, fileTreeSelectionKey } from "../injectionKeys";

// 这个组件只负责渲染「一个节点」。
// 类型是 FileNode —— 注意它带 children?: FileNode[]，是自引用结构，
// 所以这个组件也会在自己的模板里用到自己。
const props = defineProps<{
  node: FileNode;
}>();

// 从祖先组件注入选中状态。不管这个节点在第几层，都能拿到
const selection = inject(fileTreeSelectionKey);

// ★ 展开状态**不放在这里**，而是从 App 注入的一份「按路径记的集合」里读。
//   原因：放组件里的话，重建整棵树（比如另存为之后刷新）会把展开状态全清掉。
//   提升出去之后，重载时只要路径没变，这里读到的还是 true
const expansion = inject(fileTreeExpansionKey);

const isFolder = computed(() => props.node.type === "folder");

// 统一成数组，避免模板里反复写 `props.node.children ?? []`
const children = computed<FileNode[]>(() => props.node.children ?? []);

const canExpand = computed(() => isFolder.value && children.value.length > 0);

// 高亮跟随「当前激活的文件」—— 注意点标签栏上的标签也会让这里跟着变，
// 因为大家读的是同一个状态
const isActive = computed(() => selection?.activePath.value === props.node.path);

const expanded = computed(() => expansion?.expandedPaths.value.has(props.node.path) ?? false);

function handleClick() {
  if (canExpand.value) {
    expansion?.toggle(props.node.path);
  } else if (props.node.type === "file") {
    selection?.openFile(props.node);
  }
}
</script>

<template>
  <div class="node">
    <div class="label" :class="{ selected: isActive }" @click="handleClick">
      <!-- 箭头位置始终占位：没有箭头的文件才能和文件夹左对齐 -->
      <span class="arrow">
        <template v-if="canExpand">{{ expanded ? "▾" : "▸" }}</template>
      </span>
      <span class="name">{{ props.node.name }}</span>
    </div>

    <!-- 折叠时整棵子树不渲染（v-if 会销毁子组件，见下面的说明） -->
    <div v-if="canExpand && expanded" class="children">
      <!-- 递归：每个子节点再交给本组件自己渲染 -->
      <!-- key 用 path 而不是 name：跨层级可能有同名节点，name 不保证唯一 -->
      <FileTreeNode v-for="child in children" :key="child.path" :node="child" />
    </div>
  </div>
</template>

<style scoped>
.node {
  user-select: none;
}

.label {
  display: flex;
  align-items: center;
  gap: 4px;
  padding: 2px 8px;
  /* 悬停 / 选中时的底色是圆角的 —— 和 VS Code 的侧栏列表一致。
     根级项贴着左边缘，缩进过的项自然就缩进去了，圆角都能看出来 */
  border-radius: 4px;
  white-space: nowrap;
  cursor: pointer;
}

.label:hover {
  background: var(--color-hover);
}

/* 必须写在 :hover 之后。
   两者优先级相同（都是 2 个类级选择器），同优先级时「后写的赢」——
   这样悬停在已选中的行上，也不会把选中色盖掉。 */
.label.selected {
  background: var(--color-selection);
  color: var(--color-text-on-accent);
}

/* 固定宽度：保证「有箭头」和「没箭头」的行内容对齐 */
.arrow {
  flex: 0 0 12px;
  text-align: center;
  font-size: 10px;
  opacity: 0.7;
}

.name {
  overflow: hidden;
  text-overflow: ellipsis;
}

/* 每嵌套一层就多缩进 12px，层级效果自动累积，不需要传 depth */
.children {
  padding-left: 12px;
}
</style>
