<script setup lang="ts">
import { computed, inject } from "vue";
import { CHEVRON_DOWN } from "../icons";
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

/**
 * 展开 / 收起的箭头 —— **VS Code 原生的 codicon `chevron-down`**，
 * 从 `raw.githubusercontent.com/microsoft/vscode-codicons/main/src/icons/chevron-down.svg`
 * 一字不改拄来的（不凭记忆描）。
 *
 * ★★ 它是「细长无柄」的：只有一撇一捺，**没有竖杆** ——
 *   这就是 VS Code 文件树那个箭头的形状。
 *   旧版用的 `▸` / `▾` 是实心三角，又粗又短。「无柄」是第一眼的不像之处
 * ★ 只维护**一条**路径：收起态是把这张图转 -90°，
 *   和 VS Code 一样（`.monaco-tl-twistie.collapsed:before { transform: rotate(-90deg) }`）。
 *   两张图各写一份的话，迟早会有一次只改了其中一个
 * ⚠ codicon 是**填充**图形（图上只有 `fill="currentColor"`，没有 stroke），
 *   所以这里也不给 stroke —— 按描边画会得到「粗轮廓图」，和原生不是一个东西
 * ★ 路径本体在 `../icons.ts` —— 配置界面那个下拉箭头用的是同一份，
 *   免得两条一模一样的长字符串各写一份、以后只改了其中一处
 */

// 高亮跟随「当前激活的文件」—— 注意点标签栏上的标签也会让这里跟着变，
// 因为大家读的是同一个状态
const isActive = computed(() => selection?.activePath.value === props.node.path);

const expanded = computed(() => expansion?.expandedPaths.value.has(props.node.path) ?? false);

function handleClick() {
  if (canExpand.value) {
    expansion?.toggle(props.node.path);
  } else if (props.node.type === "file") {
    selection?.openFile(props.node.path);
  }
}
</script>

<template>
  <div class="node">
    <div class="label" :class="{ selected: isActive }" @click="handleClick">
      <!-- 箭头位置始终占位：没有箭头的文件才能和文件夹左对齐 -->
      <span class="arrow" :class="{ collapsed: !expanded }">
        <svg v-if="canExpand" viewBox="0 0 16 16" aria-hidden="true">
          <path :d="CHEVRON_DOWN" />
        </svg>
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

/* 固定宽度：保证「有箭头」和「没箭头」的行内容对齐。
   宽度**没改**（还是 12px）—— 只是把里面的文字字符换成了图标，
   这样树的对齐、缩进一点都没动，改的只有图标本身 */
.arrow {
  flex: 0 0 12px;
  display: flex;
  align-items: center;
  justify-content: center;
}

/* 尺寸从 VS Code 的 twistie 换算过来：
   它那里是 `width: 16px` 的方框 + `font-size: 10px` 的图标，
   而 codicon 的设计网格是 16 单位 ⇒ 画出来那撇捺只有 ~7px 长。
   所以这里给 10px —— 小了才「细」，大了立刻变成一顶帽子 */
.arrow svg {
  width: 10px;
  height: 10px;
  /* codicon 是填充图形，没有 stroke（见 CHEVRON_DOWN 的说明） */
  fill: currentColor;
  /* 悬浮一层，在深色下不至于发糊 */
  opacity: 0.85;
}

/* 收起态 = 同一张图转 -90°（VS Code 就是这么做的） */
.arrow.collapsed svg {
  transform: rotate(-90deg);
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
