/**
 * Monaco 内部模块的类型声明。
 *
 * ★ 为什么会有这个文件：Monaco **没有**给「注册打开位置」的公开口子，
 *   而这件事是「跳转到定义」绕不开的 —— 见 `lspFeatures.ts` 里的说明。
 *   所以只能深引两个内部模块，把声明集中在这一个文件里，
 *   好处是「我们碰了内部」这件事一眼可见：升级 monaco 时只要这两个路径还在，就没事。
 *
 * ⚠ 路径不能写 `esm/vs/` 前缀 —— monaco 的 package.json 里有
 *   `"./*": "./esm/vs/*.js"` 的映射，写了会被拼成 `esm/vs/esm/vs/...`
 *   （项目文档里那条「monaco 0.56+ 的 worker 导入不能带 esm/vs 前缀」是同一回事）
 */
declare module "monaco-editor/editor/standalone/browser/standaloneServices" {
  export const StandaloneServices: {
    /** 取一个已经注册的服务实例（不存在就现场创建） */
    get<T>(serviceId: unknown): T;
  };
}

declare module "monaco-editor/editor/browser/services/codeEditorService" {
  /** 服务标识。`createDecorator("codeEditorService")` 的产物 */
  export const ICodeEditorService: unknown;
}

// ---------- 内置语言服务的「把 provider 注册上」那一步 ----------
//
// ★★ 为什么连这个也要引：内置的 json / css / html 服务是在某个语言
//   **第一次被用到**时（`languages.onLanguage`）调 `setupMode(defaults)`
//   把 provider 一次性注册完的，而且**只有那一次** ——
//   `setModeConfiguration` 事后改配置是空操作（那份 defaults 上没有订阅）。
//   ⇒ 我们提前把内置服务关掉之后，万一真服务器起不来，就得自己补一次
//     `setupMode`，否则那个语言两头落空。
//   见 App.vue 的 restoreBuiltinLanguageService。
//
// ⚠ 这里只声明我们**真的用到**的那一个函数，不抄整份模块的类型：
//   抄了就等于把 Monaco 的内部类型搬进我们的代码里
declare module "monaco-editor/languages/features/json/jsonMode" {
  export function setupMode(defaults: unknown): void;
}

declare module "monaco-editor/languages/features/css/cssMode" {
  export function setupMode(defaults: unknown): void;
}

declare module "monaco-editor/languages/features/html/htmlMode" {
  export function setupMode(defaults: unknown): void;
}
