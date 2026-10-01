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
