<script setup lang="ts">
import { ref, onMounted } from "vue";
import * as monaco from "monaco-editor";
import { invoke } from "@tauri-apps/api/core";
import { fakeFileTree } from "./mockData";
import type { FileNode } from "./types";

const editorContainer = ref<HTMLElement | null>(null);

onMounted(() => {
  if (!editorContainer.value) return;//安全检查，如果容器还没准备好就退出

  const editor = monaco.editor.create(editorContainer.value, {
    value: "// 在这里写代码",
    language: "typescript",
    theme: "vs-dark",
    automaticLayout: true,
  });
});
</script>

<template>
  <div class="app"> <!-- 最外层，占满全屏-->
    <div class="main"> <!-- 中间区域：横向排列-->
      <aside class="sidebar"> <!--左侧栏-->
        <!--这里放文件树-->
      </aside>
      <div class="editor" ref="editorContainer"></div> <!--Monaco 挂载点-->
    </div>
    <footer class="statusbar"> <!--底部状态栏-->
      Ready
    </footer>
  </div>
</template>

<style scoped>
.app {
  display: flex;
  flex-direction: column;
  height: 100%;
}

.main {
  display: flex;
  flex: 1;
  min-height: 0;
}

.sidebar {
  flex: 0 0 240px;
  overflow-y: auto;
}

.editor {
  flex: 1;
  min-width: 0;
}

.statusbar {
  flex: 0 0 24px;
}
</style>

<style>
:root {
  font-family: Inter, "Segoe UI", "Microsoft YaHei", Avenir, Helvetica, Arial, sans-serif;
  font-weight: 400;

  font-synthesis: none;
  text-rendering: optimizeLegibility;
  -webkit-font-smoothing: antialiased;
  -moz-osx-font-smoothing: grayscale;
  -webkit-text-size-adjust: 100%;
}

html,
body,
#app {
  height: 100%;
}

body {
  margin: 0;
  overflow: hidden;
  font-size: 13px;
  line-height: 1.5;
}
</style>