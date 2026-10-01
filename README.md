# Toocode

一个用 **Tauri 2 + Vue 3 + TypeScript + Monaco** 写的桌面代码编辑器。

它既是练手项目，也是一次「把 VS Code 的那些机制拆开看看到底怎么实现」的实验 ——
所以下面很多功能刻意**不去调现成的库**，而是照着 VS Code 的思路自己搭一遍。

## 它能做什么

**编辑器基础**

- 三栏布局 + 可拖拽的分隔条，深浅两套主题（配色直接从 VS Code 主题文件映射过来）
- 多标签、未保存标记、关闭前确认、**热退出** —— 崩溃或强杀进程也能恢复未保存的内容
- 图片预览：滚轮**以光标为锚点**缩放、拖拽平移
- 字号缩放（`Ctrl` + 滚轮 / `Ctrl+=` / `Ctrl+-` / `Ctrl+0`）
- 内置终端（xterm.js + 真 PTY）
- 全项目搜索：Rust 侧遍历，结果按文件分组并高亮命中
- 命令面板（`Ctrl+Shift+P`）、快速打开（`Ctrl+P`），带模糊匹配

**插件机制 —— 直接复用本机已装的 VS Code 扩展**

不是自己写死一套语法，而是去读本机 VS Code 扩展里声明的贡献点：

| 贡献点 | 用途 |
| --- | --- |
| `contributes.grammars` | TextMate 语法高亮（连**程序目录**里的内置扩展一起扫，Vue 这类寄生语法才完整） |
| `contributes.themes` | 把 VS Code 主题翻译成 Monaco 主题 + 一整套 CSS 变量 |
| `contributes.grammars[].injectTo` | 注入语法（`v-if`、`@click`、`:class` 这些） |
| `contributes.snippets` | 代码片段补全 |

**Topilot —— 内置 AI 助手**

- 流式对话、思考过程可见、Markdown 渲染（过 DOMPurify 消毒）
- 能调工具：读文件 / 列目录 / 全项目搜索 / **改文件**
- **改文件必须过 diff 审阅**：模型只负责「提议」，你点保留才真的落盘
- 看得见编辑器状态（当前文件、光标位置、选中的代码）
- 支持「添加上下文」：从快速打开里选文件附加给这一轮对话
- API 密钥用 **Windows DPAPI** 加密存储（只有同一台机器的同一个用户能解开）

## 技术栈

| 层次 | 选型 |
| --- | --- |
| 桌面壳 | Tauri 2 |
| 前端 | Vue 3（`<script setup>`）+ TypeScript + Vite |
| 编辑器内核 | `monaco-editor` |
| 语法高亮 | `vscode-textmate` + `vscode-oniguruma` |
| 终端 | `xterm.js` + `portable-pty` |
| 后端 | Rust（Tauri command，前端通过 `invoke` 调用） |

## 跑起来

需要 Node.js 和 Rust 工具链（Windows 上还要 MSVC 生成工具）。

```bash
npm install
npm run tauri dev
```

## 打包

```bash
npm run tauri build
```

产物在 `src-tauri/target/release/` 下：

- `bundle/nsis/Toocode_0.1.0_x64-setup.exe` —— 安装程序，双击就能装（推荐发给别人用这个）
- `bundle/msi/Toocode_0.1.0_x64_en-US.msi` —— MSI 安装包
- `toocode.exe` —— 裸的可执行文件，单独拿走也能跑

（改应用图标：画一张 1024×1024 的 PNG，然后 `npm run tauri icon <那个png>`）

## 仓库里的文档

- `.github/copilot-instructions.md` —— 这个项目的「上下文笔记」：
  记了每个功能**为什么**这么设计、踩过哪些坑、哪些坑看起来像别的问题。
  写代码时它会被 AI 助手读进去，也可以当开发笔记翻。

## 已知的局限

- 还没有 LSP，所以没有智能提示和跳转定义
- 调试控制台和「问题」面板是空壳（前者要 DAP，后者要 LSP 才有诊断来源）

