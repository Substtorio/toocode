# Toocode

[![CI](https://github.com/Substtorio/toocode/actions/workflows/ci.yml/badge.svg)](https://github.com/Substtorio/toocode/actions/workflows/ci.yml)

一个用 **Tauri 2 + Vue 3 + TypeScript + Monaco** 写的桌面代码编辑器。

它既是练手项目，也是一次「把 VS Code 的那些机制拆开看看到底怎么实现」的实验 ——
所以下面很多功能刻意**不去调现成的库**，而是照着 VS Code 的思路自己搭一遍
（比如 git 状态是直接调 `git`，语法高亮是去读你本机 VS Code 扩展里的语法文件）。

**MIT 许可证** ｜ **只在 Windows 上验证过** ｜ 安装包**未签名**，Windows 会弹 SmartScreen，
点「更多信息 → 仍要运行」即可。

## 它能做什么

**编辑器**

- 三栏布局 + 可拖拽的分隔条；深浅两套主题（配色直接从 VS Code 主题文件映射过来）
- 多标签、未保存标记、关闭前确认、**热退出** —— 崩溃或强杀进程也能恢复未保存的内容
- 图片预览：滚轮**以光标为锚点**缩放、拖拽平移
- 字号缩放（`Ctrl` + 滚轮 / `Ctrl+=` / `Ctrl+-` / `Ctrl+0`）
- 命令面板（`Ctrl+Shift+P`）、转到文件（`Ctrl+P`）、转到符号（`Ctrl+T`），都带模糊匹配
- 内置终端（xterm.js + 真 PTY）
- 源代码管理：git 状态列表 + diff 查看

**全项目搜索（SQLite 索引）**

- 内容检索用 **FTS5 + trigram 分词器**，所以是**子串**语义：`foo` 能搜到 `foobar`
- 另有文件名索引和符号索引 —— 前者能覆盖文件树深度上限之外的文件，后者就是 `Ctrl+T`
- 索引只是**加速手段**：没索引的文件会走现场扫描，所以「索引没建完」永远不会导致搜不到
- 索引是派生数据（放在应用数据目录），随时可删可重建

**语言特性（LSP）**

- 诊断 → 「问题」面板 + 状态栏上的错误 / 警告计数
- 补全（含**自动导入**）、悬停提示、跳转到定义（跨文件）、重命名符号（`F2`）
- 语言服务器**不自己打包**：直接用你本机装的 VS Code 自带的那几个（json / html / css），
  或 PATH 里的 `rust-analyzer` 之类

**调试（DAP）**

- 断点（`F9`）、启动 / 继续（`F5`）、单步进出（`F10` / `F11` / `Shift+F11`）、停止（`Shift+F5`）
- 调用栈、变量、调试控制台（可以在里面求值）
- stdio 和 **TCP** 两种适配器都能接（实测 Python / debugpy）

**插件机制 —— 直接复用本机已装的 VS Code 扩展**

不是自己写死一套语法，而是去读本机 VS Code 扩展里声明的贡献点：

| 贡献点 | 用途 |
| --- | --- |
| `contributes.grammars` | TextMate 语法高亮（连**程序目录**里的内置扩展一起扫，Vue 这类寄生语法才完整） |
| `contributes.themes` | 把 VS Code 主题翻译成 Monaco 主题 + 一整套 CSS 变量 |
| `contributes.grammars[].injectTo` | 注入语法（`v-if`、`@click`、`:class` 这些） |
| `contributes.snippets` | 代码片段补全 |
| `contributes.languages[].configuration` | 语言配置：`Ctrl+/` 插什么注释符、敲 `{` 会不会自动补 `}`、回车缩进几格 |

另外还读**用户自己写的代码片段**（`%APPDATA%\Code\User\snippets\*.json`）——
那个不在任何扩展里，但走的是同一条链路。

**不做的两个**：`contributes.commands`（扩展注册的命令）和 `semanticTokenScopes`。
前者要跑扩展代码（我们没有 JS 扩展宿主，列出来点了没用），后者要先把 LSP 的
**语义 token** 接上（现在只有 TextMate 语法高亮）—— 都是「有前置条件」的，
不是漏了。

**Topilot —— 内置 AI 助手**（`Ctrl+Alt+I`）

- 流式对话、思考过程可见、Markdown 渲染（过 DOMPurify 消毒）
- 多会话：新建 / 切换 / 删除，记录存在应用数据目录里
- 能调工具：读文件 / 列目录 / 全项目搜索 / **改文件**
- **改文件必须过 diff 审阅**：模型只负责「提议」，你点保留才真的落盘
- 看得见编辑器状态（当前文件、光标位置、选中的代码），支持「添加上下文」
- API 密钥用 **Windows DPAPI** 加密存储（只有同一台机器的同一个用户能解开）
- 需要自备密钥，接口兼容 OpenAI 格式（可填任意兼容服务）

### 常用快捷键

| 键 | 作用 |
| --- | --- |
| `Ctrl+Shift+P` / `Ctrl+P` / `Ctrl+T` | 命令面板 / 转到文件 / 转到符号 |
| `Ctrl+Shift+F` | 在文件夹中搜索 |
| `Ctrl+Shift+G` / `Ctrl+Shift+D` / `Ctrl+Shift+M` | 源代码管理 / 运行和调试 / 问题 |
| `Ctrl+J` 或 `` Ctrl+` `` | 显示 / 隐藏底部面板 |
| `Ctrl+Alt+I` | 显示 / 隐藏 Topilot |
| `F5` / `F9` / `F10` / `F11` | 启动调试 / 断点 / 单步跳过 / 单步进入 |
| `F2` | 重命名符号 |

（欢迎页里有一张完整的表）

## 一个前提：它大量复用你本机的 VS Code

这个项目**不自己实现语言**。语法高亮、主题、代码片段，都是去读你本机已装的
VS Code 扩展里声明的贡献点；JSON / HTML / CSS 的语言服务器，也是直接用 VS Code
自带的那几个。所以**装了 VS Code 体验最完整**；没装也能用，只是这几处会降级：

| 没有 VS Code | 会怎样 |
| --- | --- |
| 语法高亮 | 退回 **Monaco 内置分词器** —— 81 种语言照旧有高亮，只是不如 TextMate 精细 |
| 主题 | 退回项目自带的那套配色 —— **深浅两套照旧能用**，只是不是 VS Code 的 Modern 配色 |
| 代码片段 / 注入语法 | 没有片段补全；`v-if`、`@click` 这类没有专门上色 |
| JSON / HTML / CSS 的语言特性 | 退回 **Monaco 内置语言服务** —— 它和 VS Code 用的是**同一份代码**，诊断和补全照旧有 |
| `rust-analyzer` / `debugpy` | 和 VS Code 无关，只要在 PATH 里就行 |
| 其它全部（编辑器 / 搜索索引 / 终端 / git / Topilot） | **不受影响** |

**为什么这么设计**：语言服务器和语法文件都是「机器上已经有现成的、而且质量比我写得
更好」的东西，再实现一遍既不现实也没意义。这个前提换来的，是一个 **5.8 MB 的安装包**
里装下了 5 种语言的智能提示和调试能力。

**如果要给别人用**，正确的做法是把「资源来源」做成三级兜底：
**项目内置一份 → 本机 VS Code 覆盖 → 用户自己的扩展再覆盖**。现在只有中间那一级。
这是已知的、有明确解法的局限，不是设计缺陷。

## 技术栈

| 层次 | 选型 |
| --- | --- |
| 桌面壳 | Tauri 2 |
| 前端 | Vue 3（`<script setup>`）+ TypeScript + Vite |
| 编辑器内核 | `monaco-editor` |
| 语法高亮 | `vscode-textmate` + `vscode-oniguruma` |
| 终端 | `xterm.js` + `portable-pty` |
| 搜索索引 | `rusqlite`（bundled SQLite）+ FTS5 trigram |
| 后端 | Rust（Tauri command，前端通过 `invoke` 调用） |

## 跑起来

需要 Node.js 22+ 和 Rust 工具链（Windows 上还要 MSVC 生成工具）。

```bash
npm install
npm run tauri dev
```

## 测试

```bash
npm run build              # 里含 `vue-tsc --noEmit` 类型检查 + 前端构建
cd src-tauri && cargo test # 43 个用例
```

这些用例刻意挑的是**错了不会报错**的地方 —— 它们失败时只会「悄悄变差」：

- 搜索的列号口径（UTF-16 单位而不是字符数，emoji 是补充平面才暴露）
- 路径包含判断（`D:\foobar` 会被当成在 `D:\foo` 里）
- JSON-RPC 的 `Content-Length`（是**字节数**，含中文时算错会让后面所有消息错位）
- 索引的正确性（`EXPLAIN QUERY PLAN` 断言真的走了 FTS5 索引；建不完时不会漏结果）
- 符号扫描规则（注释不算声明、markdown 跳过围栏代码块、JSON 靠括号深度判顶层）
- 片段去重（VS Code 更新后新旧两个版本目录会让同一份片段被扫两遍）

前端的纯逻辑（模糊匹配 / 路径工具 / 片段解析 / SSE 解析）都拆成了零依赖模块，在 Node 里可直接跑用例。
**组件层没有自动化测试** —— 那部分靠 CDP 直连真窗口验证（做法记在开发笔记里）。

有几个用例需要本机装了 VS Code / node（它们要拿**真实的**语法文件和语言服务器来验），
条件不具备时**自动跳过**而不是失败 —— 所以在 CI 上也是绿的。

## 打包

```bash
npm run tauri build
```

产物在 `src-tauri/target/release/` 下：

- `bundle/nsis/Toocode_<版本>_x64-setup.exe` —— 安装程序，双击就能装（**推荐发给别人用这个**）
- `bundle/msi/Toocode_<版本>_x64_en-US.msi` —— MSI 安装包
- `toocode.exe` —— 裸的可执行文件，单独拿走也能跑（需要系统有 WebView2 运行时）

（改应用图标：画一张 1024×1024 的 PNG，然后 `npm run tauri icon <那个png>`）

发布前要改的版本号有**四处**，别只改一处：`package.json`、`src-tauri/tauri.conf.json`、
`src-tauri/Cargo.toml`，以及用 `npm install --package-lock-only` 同步的 `package-lock.json`。

## 仓库里的文档

- `.github/copilot-instructions.md` —— 这个项目的「上下文笔记」：
  记了每个功能**为什么**这么设计、踩过哪些坑、哪些坑看起来像别的问题。
  写代码时它会被 AI 助手读进去，也可以当开发笔记翻。

## 稳定性承诺

**不会轻易改的**（改了会伤害已经装了的用户）：

- 用户数据的位置与格式：`%APPDATA%\com.toocode.app\` 下的 `toocode.secret`（DPAPI 加密，
  兼容早期版本的明文格式）和 `toocode.chats.json`
- 那几个 localStorage key（`new_vscode:*`）—— 改名字会让老用户丢掉主题、布局和
  **未保存的内容**（hot exit）
- 安装包形态：per-user 安装、不写 `Program Files`、卸载**不删**用户数据

**随时可能变的**（内部实现，别依赖）：

- IPC 命令名与参数形状（只有前端在调）
- SQLite 索引库：派生数据，删了会重建；表结构不保证跨版本兼容
- 模块划分、`.github/copilot-instructions.md` 的写法与结构

## 已知的局限

- 语法 / 主题 / 片段 / JSON·HTML·CSS 的语言特性**依赖本机装了 VS Code**
  —— 见上面「一个前提」那一节（没装也能用，只是这几处降级）
- 补全不会在敲 `.` / `<` 时**自动**弹出（html / css 服务器没声明 `triggerCharacters`）
- 「转到符号」在没打开过的文件上用**行级启发式**扫描，所以嵌套层级显示不出来
- 调试只支持**单个会话**；变量只能展开一层；没有 `launch.json`
- 内容索引有 **128 MB 源码**的上限（trigram 索引体积约为源码的 4 倍），
  超出的文件仍然能搜到，但每次搜索都要现场扫
- 没有扩展市场：插件机制只读本机已装的 VS Code 扩展
- 不支持用户自己的代码片段（`%APPDATA%\Code\User\snippets`）
- 单窗口，不支持同时开两个窗口
- 只在 **Windows** 上验证过。DPAPI 加密是 Windows 专有的，其它平台会退回明文存储

## 许可

本项目按 **MIT** 发布（见 `LICENSE`）。

依赖各自的许可跟着依赖走，不因本项目而改变 —— 发安装包之前扫过一遍：

- **Rust 侧 334 个第三方 crate**：以 MIT / Apache-2.0 双许可为主，另有 18 个 `Unicode-3.0`、
  5 个 `MPL-2.0`、少量 BSD / ISC / Zlib / Unlicense / 0BSD。**没有 GPL / AGPL。**
  （`MPL-2.0` 是弱传染，但它明确允许把**未修改的**库静态链接进更大的作品）
- **前端 32 个生产依赖**：24 个 MIT，其余是 Apache-2.0 / ISC / BSD。
  唯一需要留意的 `dompurify` 是 `MPL-2.0 OR Apache-2.0` —— 有一个更宽松的选项可挑
