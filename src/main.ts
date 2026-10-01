// 这个文件只干一件事：在「第一次创建编辑器之前」把 MonacoEnvironment 设置好。
// Monaco 本体是在 App.vue 里导入使用的，所以这里不需要再 import monaco。

// 注意 monaco-editor 0.56 的 package.json 里有 exports 映射：
//   "./*": "./esm/vs/*.js"
// 包名后面的路径会被自动接到 esm/vs/ 下面，所以这里不能再写 "esm/vs/" 前缀，
// 否则会被拼成 esm/vs/esm/vs/... 而找不到文件。
import editorWorker from "monaco-editor/editor/editor.worker?worker";
import jsonWorker from "monaco-editor/language/json/json.worker?worker";
import cssWorker from "monaco-editor/language/css/css.worker?worker";
import htmlWorker from "monaco-editor/language/html/html.worker?worker";
import tsWorker from "monaco-editor/language/typescript/ts.worker?worker";

self.MonacoEnvironment = {
    getWorker(_, label) {
        if (label === "json") return new jsonWorker();
        if (label === "css" || label === "scss" || label === "less") return new cssWorker();
        if (label === "html" || label === "handlebars" || label === "razor") return new htmlWorker();
        if (label === "typescript" || label === "javascript") return new tsWorker();
        return new editorWorker();
    }
};

import { createApp } from "vue";
import App from "./App.vue";
// 悬停提示。样式在 App.vue 的**全局** style 块里（不是 scoped）——
// 它是挂在 <body> 下面的，拿不到组件的 data-v 属性
import { installTooltips } from "./tooltip";

createApp(App).mount("#app");

// ★ 放在 mount **之后**：它会把页面上已有的 `[title]` 一次性搬走，
//   挂载之后再跑才能扫到首屏那些元素。
// ★ 放在这里而不是 App.vue 的 onMounted：它是纯粹的「文档级」行为，
//   和 App 里那些状态一点关系都没有（挂上了就不用管）
installTooltips();
