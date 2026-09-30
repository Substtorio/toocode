<script setup lang="ts">
import { computed, inject, ref } from "vue";
import type { FileNode } from "../types";
import { fileTreeSelectionKey } from "../injectionKeys";

// 这个组件只负责渲染「一个节点」。
// 类型是 FileNode —— 注意它带 children?: FileNode[]，是自引用结构，
// 所以这个组件也会在自己的模板里用到自己。
const props = defineProps<{
  node: FileNode;
}>();

// 是否展开。放在组件内部 —— 每个节点自己管自己的状态，节点之间零沟通成本。
// 默认收起，和 VS Code 一致（想默认展开就改成 true）
const expanded = ref(false);

// 从祖先组件注入选中状态。不管这个节点在第几层，都能拿到
const selection = inject(fileTreeSelectionKey);

const isFolder = computed(() => props.node.type === "folder");

// 统一成数组，避免模板里反复写 `props.node.children ?? []`
const children = computed<FileNode[]>(() => props.node.children ?? []);

const canExpand = computed(() => isFolder.value && children.value.length > 0);

// 高亮跟随「当前激活的文件」—— 注意点标签栏上的标签也会让这里跟着变，
// 因为大家读的是同一个状态
const isActive = computed(() => selection?.activePath.value === props.node.path);

function handleClick() {
  if (canExpand.value) {
    expanded.value = !expanded.value;
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
