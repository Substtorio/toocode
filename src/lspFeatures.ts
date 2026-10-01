/**
 * 把语言服务器接进 Monaco 的语言特性 provider：**补全 / 悬停 / 跳转到定义**。
 *
 * ★ 为什么单独一个文件，而不是塞进 `lsp.ts`：
 *   `lsp.ts` 只管协议（分帧、会话、请求），拿到的都是路径 + 纯数据；
 *   这里管的是「Monaco 要什么形状」。两边的失败方式完全不同 ——
 *   协议那边错了是「什么都收不到」，这里错了是「收到了但显示得不对」。
 *   混在一起时，排查一个 symptom 得同时怀疑两层。
 *
 * ★ 「跳转到定义」比另外两个多一件事要办：定义在**别的文件**里的时候，
 *   得有人去把那个文件打开。Monaco 的 standalone 版自己不做这件事 ——
 *   它只会在「目标就是当前这个 model」时把光标挪过去，否则静默放弃
 *   （见下面 registerLspOpenHandler 的说明）。
 */
import * as monaco from "monaco-editor";
import { StandaloneServices } from "monaco-editor/editor/standalone/browser/standaloneServices";
import { ICodeEditorService } from "monaco-editor/editor/browser/services/codeEditorService";
import {
  requestCompletion,
  requestDefinition,
  requestHover,
  requestRename,
  uriToPath,
  type LspCompletionItem,
  type LspHover,
  type LspLocation,
  type LspLocationLink,
  type LspMarkedString,
  type LspPosition,
  type LspRange,
  type LspWorkspaceEdit,
} from "./lsp";
import { samePath } from "./pathUtils";

/** App 提供的两个口子。和 lsp.ts 里那个 host 一样，两边都不知道对方内部长什么样 */
export interface LspFeatureHost {
  /**
   * 从一个 model 反查它的磁盘路径。
   *
   * ⚠ ★ 必须反查，不能直接读 `model.uri` —— 我们的 model 是**匿名**的
   *   （`createModel(content, language)` 没传 uri ⇒ `inmemory://model/3`），
   *   而 provider 拿到的只有 model。所以 App 那边得遍历 `models` 找同一个对象
   *   （和 editorBridge 那边「遍历而不是建索引」同一个理由：
   *     索引表要跟着 openFile / 另存为 / 关标签一起维护，总有漏的地方，
   *     而漏掉的表现是「文件明明开着却读不到」，不报错）
   */
  pathOfModel: (model: monaco.editor.ITextModel) => string | null;

  /**
   * 打开另一个文件，并**等到它真的成了编辑器上的 model**，然后返回编辑器。
   *
   * ⚠ 为什么必须「等到」：Monaco 拿到返回的编辑器之后会立刻
   *   `setSelection` + `revealRangeInCenter`。要是那时编辑器上还挂着**上一个**
   *   model，目标行号在新 model 里完全可能越界 —— 而 Monaco 的
   *   `setSelection` 遇到越界是**抛异常**的
   *
   * ⚠ ★★ `selection` 得**自己应用**：Monaco 那套默认实现（`doOpenEditor`）
   *   之所以会挪光标，就是因为它在里面调了 `setSelection`。而
   *   `openCodeEditor` 是「谁先返回非 null 就用谁，后面的不再问」——
   *   我们一旦返回了编辑器，默认实现就再也没机会跑了。
   *   一开始漏了这一步，症状是「文件确实开了，但光标停在 1:1」。
   */
  openLocation: (
    path: string,
    selection: monaco.IRange | monaco.IPosition | null,
  ) => Promise<monaco.editor.ICodeEditor | null>;

  /**
   * 确保某个文件**有一份 model**（不必打开标签页）。
   *
   * ★★ 什么时候会用到：重命名可能改到**没被打开过**的文件
   *   （跨文件的 css 变量、以后 TS 的自动导入）。
   *   而 Monaco 那套「批量编辑」是按 uri 找 model 的，
   *   找不到就**直接抛**（实测源码：`if (!model) throw new Error('bad edit - model not found')`）
   *   ⇒ 所以得先把 model 建出来，而不是等它抛
   */
  ensureModel: (path: string) => Promise<void>;

  /** 日志出口。「输出」面板在 App 那边，这里只负责写 */
  log: (line: string) => void;
}

// ============================ 翻译：LSP → Monaco ============================

/**
 * LSP 的补全类型编号 → Monaco 的。
 *
 * ⚠ ★ 两边的编号**完全不一样**，而且错位是从第 3 个开始的：
 *   LSP  `Method=2, Function=3, Field=5, Variable=6, Class=7 …`
 *   Monaco `Function=1, Field=3, Variable=4, Class=5 …`
 *   所以不能「差不多减个 1」，必须查表。
 * ★ 用 Monaco 的**枚举名字**而不是数字 —— 数字再去对一遍太容易看漏，
 *   而名字写错的话类型检查会当场报出来
 */
const COMPLETION_KIND: Record<number, monaco.languages.CompletionItemKind> = {
  1: monaco.languages.CompletionItemKind.Text,
  2: monaco.languages.CompletionItemKind.Method,
  3: monaco.languages.CompletionItemKind.Function,
  4: monaco.languages.CompletionItemKind.Constructor,
  5: monaco.languages.CompletionItemKind.Field,
  6: monaco.languages.CompletionItemKind.Variable,
  7: monaco.languages.CompletionItemKind.Class,
  8: monaco.languages.CompletionItemKind.Interface,
  9: monaco.languages.CompletionItemKind.Module,
  10: monaco.languages.CompletionItemKind.Property,
  11: monaco.languages.CompletionItemKind.Unit,
  12: monaco.languages.CompletionItemKind.Value,
  13: monaco.languages.CompletionItemKind.Enum,
  14: monaco.languages.CompletionItemKind.Keyword,
  15: monaco.languages.CompletionItemKind.Snippet,
  16: monaco.languages.CompletionItemKind.Color,
  17: monaco.languages.CompletionItemKind.File,
  18: monaco.languages.CompletionItemKind.Reference,
  19: monaco.languages.CompletionItemKind.Folder,
  20: monaco.languages.CompletionItemKind.EnumMember,
  21: monaco.languages.CompletionItemKind.Constant,
  22: monaco.languages.CompletionItemKind.Struct,
  23: monaco.languages.CompletionItemKind.Event,
  24: monaco.languages.CompletionItemKind.Operator,
  25: monaco.languages.CompletionItemKind.TypeParameter,
};

/** LSP 的行列（0 开始）→ Monaco 的（1 开始）。**两边都得 +1** */
function toRange(range: LspRange): monaco.Range {
  return new monaco.Range(
    range.start.line + 1,
    range.start.character + 1,
    range.end.line + 1,
    range.end.character + 1,
  );
}

/**
 * 补全项里那个「说明」。
 *
 * ★ LSP 的 `documentation` 有两种写法：一句纯字符串，或者
 *   `{ kind: "markdown" | "plaintext", value }`。第二种要按 markdown 交给 Monaco ——
 *   服务器里的 markdown（表格、代码块）才显示得对
 */
function toDocumentation(
  documentation: LspCompletionItem["documentation"],
): string | monaco.IMarkdownString | undefined {
  if (documentation === undefined) return undefined;
  if (typeof documentation === "string") return documentation;
  return { value: documentation.value };
}

/**
 * 一个补全项。
 *
 * @param fallbackRange 服务器没给 `textEdit` 时用哪个范围替换。
 *   ★ **必须有** —— `monaco.languages.CompletionItem.range` 是必填项。
 *     不给的话插入会变成「追加」（敲 `col` 补出 `color` → `colcolor`）
 */
export function toCompletionItem(
  item: LspCompletionItem,
  fallbackRange: monaco.IRange,
): monaco.languages.CompletionItem {
  const textEdit = item.textEdit;
  // 服务器可能只给一个 range，也可能给 insert / replace 两份（后者更准）
  const range =
    textEdit?.insert !== undefined && textEdit.replace !== undefined
      ? { insert: toRange(textEdit.insert), replace: toRange(textEdit.replace) }
      : textEdit?.range !== undefined
        ? toRange(textEdit.range)
        : fallbackRange;

  return {
    label: item.label,
    kind: COMPLETION_KIND[item.kind ?? 1] ?? monaco.languages.CompletionItemKind.Text,
    detail: item.detail,
    documentation: toDocumentation(item.documentation),
    sortText: item.sortText,
    filterText: item.filterText,
    insertText: textEdit?.newText ?? item.insertText ?? item.label,
    // ★★ 片段必须声明出来。不声明的话 `$1` / `${2:name}` 会被当**字面量**
    //   原样插进代码里 —— 和 snippets.ts 那边是同一个坑
    insertTextRules:
      item.insertTextFormat === 2
        ? monaco.languages.CompletionItemInsertTextRule.InsertAsSnippet
        : undefined,
    range,
  };
}

/** 一种 MarkedString → Monaco 的 markdown */
function markedToMarkdown(entry: LspMarkedString): monaco.IMarkdownString {
  if (typeof entry === "string") return { value: entry };
  if ("language" in entry) {
    // `{ language, value }` 是一段带语言的代码 —— 包成代码块才是它的本意
    return { value: `\`\`\`${entry.language}\n${entry.value}\n\`\`\`` };
  }
  return { value: entry.value };
}

/**
 * 悬停内容。
 *
 * ⚠ ★ 不要给 `IMarkdownString.isTrusted` 赋 true —— 服务器回的内容属于
 *   **不可信内容**（它可能把某个文件里的注释原样搬过来，而那里面可以藏着
 *   `<img onerror=...>`）。默认值就是 false：HTML 不渲染、`command:` 链接不生效。
 *   这和 markdown.ts 那边上 DOMPurify 是同一个理由
 */
export function toHover(hover: LspHover): monaco.languages.Hover | null {
  const list = Array.isArray(hover.contents) ? hover.contents : [hover.contents];
  const contents = list.map(markedToMarkdown).filter((item) => item.value.trim().length > 0);
  if (contents.length === 0) return null;
  return {
    contents,
    range: hover.range === undefined ? undefined : toRange(hover.range),
  };
}

/**
 * 定义位置。
 *
 * ★★ 这里有个**必须**做的区分：定义在**当前文件**里的时候，返回的 uri
 *   必须和当前 model 的 uri **一模一样**。
 *   为什么：Monaco 的 standalone 打开逻辑只会拿目标 uri 跟**当前 model** 比
 *   （`findModel`: 不相等就返回 null，也就是「打不开」）。
 *   而我们的 model 是匿名的 `inmemory://model/3` —— 拿真实的
 *   `file:///c%3A/...` 去比**永远不相等**，于是「同一个文件里跳转」也会失效。
 *   ⇒ 同一个文件就交回 `model.uri`，让 Monaco 自己办；
 *     别的文件才给真实 uri，由我们注册的处理函数去开（见下）
 */
export function toLocations(
  result: Array<LspLocation | LspLocationLink>,
  currentModel: monaco.editor.ITextModel,
  pathOfModel: (model: monaco.editor.ITextModel) => string | null,
): monaco.languages.Location[] {
  const currentPath = pathOfModel(currentModel);

  return result.map((entry) => {
    const isLink = "targetUri" in entry;
    const uri = isLink ? entry.targetUri : entry.uri;
    const range = isLink ? entry.targetSelectionRange ?? entry.targetRange : entry.range;

    const targetPath = uriToPath(uri);
    const sameFile = currentPath !== null && samePath(targetPath, currentPath);

    return {
      uri: sameFile ? currentModel.uri : monaco.Uri.parse(uri),
      range: toRange(range),
    };
  });
}

/**
 * LSP 的 `WorkspaceEdit` → Monaco 的 `WorkspaceEdit`。
 *
 * @returns edits 直接交给 Monaco；targets 是涉及到的磁盘路径
 *   （调用方要先把它们都变成 model，见 host.ensureModel）
 *
 * ★ 两种写法都要认：`changes`（按 uri 分组）和 `documentChanges`（数组，
 *   里面还可能是「新建 / 重命名 / 删除文件」这类操作）。
 *   重命名只会用到文本编辑，所以后者的文件操作**先跳过**（碰到了记一笔）
 */
export function toWorkspaceEdit(
  result: LspWorkspaceEdit,
  host: LspFeatureHost,
): { edits: monaco.languages.IWorkspaceTextEdit[]; targets: string[] } {
  const edits: monaco.languages.IWorkspaceTextEdit[] = [];
  const targets = new Set<string>();

  const push = (uri: string, list: Array<{ range: LspRange; newText: string }>) => {
    // ★ uri 一律过一遍 `monaco.Uri.parse` —— 它会做归一化：
    //   实测 `file:///c%3A/a.css` 和 `file:///c:/a.css` 解析出来是**同一个字符串**。
    //   不归一的话「服务器回的 uri」和「model 的 uri」会差一个 %3A，
    //   而 Monaco 是按这个字符串找 model 的 ⇒ 直接抛「model not found」
    const resource = monaco.Uri.parse(uri);
    targets.add(uriToPath(uri));
    for (const edit of list) {
      edits.push({
        resource,
        // 不传 versionId：传了的话 Monaco 会校验「编辑期间文档没被改过」，
        // 而重命名是「用户点确认」的瞬间拿到的结果，没必要卡这一道
        versionId: undefined,
        textEdit: { range: toRange(edit.range), text: edit.newText },
      });
    }
  };

  for (const [uri, list] of Object.entries(result.changes ?? {})) push(uri, list);

  for (const change of result.documentChanges ?? []) {
    if (!("textDocument" in change)) {
      host.log(`[LSP] 重命名里带了文件操作（${change.kind}），暂时不支持，已跳过`);
      continue;
    }
    push(change.textDocument.uri, change.edits);
  }

  return { edits, targets: [...targets] };
}

// ============================ 注册 ============================

/**
 * `ICodeEditorService` 里我们**真正用到**的那一部分。
 *
 * ★ 只声明这一个方法，不全量抄那个接口 —— 抄了就等于把 monaco 的内部
 *   类型搬进我们的代码里，升级时每一处都得跟着改；
 *   而我们依赖的事实只有一句：「能挂一个『该打开哪个文件』的处理函数，
 *   而且**后注册的先跑**」。
 *   （实测源码：`openCodeEditor` 遍历所有处理函数，谁先返回非 null 用谁，
 *     `registerCodeEditorOpenHandler` 用的是 `unshift`）
 */
interface CodeEditorServiceLike {
  registerCodeEditorOpenHandler(
    handler: (
      input: {
        resource: { toString(): string } | null;
        /** 服务器给的目标位置。Monaco 就是靠它把光标挪过去的 */
        options?: { selection?: monaco.IRange | monaco.IPosition | null } | null;
      },
      source: monaco.editor.ICodeEditor | null,
      sideBySide?: boolean,
    ) => Promise<monaco.editor.ICodeEditor | null>,
    id?: string,
  ): monaco.IDisposable;
}

/** 一个语言只注册一次 —— 重复注册会让同一份补全在列表里出现两遍 */
const registeredLanguages = new Set<string>();

/** Monaco 的 1 基坐标 → LSP 的 0 基 */
function toPosition(position: monaco.Position): LspPosition {
  return { line: position.lineNumber - 1, character: position.column - 1 };
}

/**
 * 给一个语言注册三个 provider。
 *
 * ★ 由 App 在「某个语言的服务器握完手」时调用（`onServerReady`）——
 *   因为 `triggerCharacters` 只有服务器自己知道，而它得写在注册时。
 *   按**语言**注册（而不是一次注册 `*`）也更接近 VS Code 的做法：
 *   没有服务器的语言压根不会被问到
 */
export function registerLspLanguage(
  languageId: string,
  triggerCharacters: string[],
  host: LspFeatureHost,
  supportsRename: boolean,
): void {
  if (registeredLanguages.has(languageId)) return;
  registeredLanguages.add(languageId);

  monaco.languages.registerCompletionItemProvider(languageId, {
    triggerCharacters,
    provideCompletionItems: async (model, position) => {
      const path = host.pathOfModel(model);
      if (path === null) return undefined;

      const items = await requestCompletion(path, toPosition(position));
      // ★ 一条都没有时返回 undefined，**不是**空数组 ——
      //   空数组会把 Monaco 的列表「接管」掉，它就再不去问别的 provider 了
      //   （snippets.ts 那边踩过同一个坑）
      if (items === null || items.length === 0) return undefined;

      // 服务器没给 textEdit 时的兜底范围：光标所在的这个词
      // ⚠ `getWordUntilPosition` 只按 `[a-zA-Z0-9_]` 圈词，所以
      //   `System.out.println` 这种词会被截断 —— 影响是「插入时少替掉一截」，
      //   而服务器给的 textEdit 本来就覆盖绝大多数情况（JSON / CSS / HTML 都会给）
      const word = model.getWordUntilPosition(position);
      const fallback = new monaco.Range(
        position.lineNumber,
        word.startColumn,
        position.lineNumber,
        position.column,
      );

      return { suggestions: items.map((item) => toCompletionItem(item, fallback)) };
    },
  });

  monaco.languages.registerHoverProvider(languageId, {
    provideHover: async (model, position) => {
      const path = host.pathOfModel(model);
      if (path === null) return undefined;

      const hover = await requestHover(path, toPosition(position));
      if (hover === null) return undefined;
      return toHover(hover) ?? undefined;
    },
  });

  monaco.languages.registerDefinitionProvider(languageId, {
    provideDefinition: async (model, position) => {
      const path = host.pathOfModel(model);
      if (path === null) return undefined;

      const result = await requestDefinition(path, toPosition(position));
      if (result === null || result.length === 0) return undefined;
      return toLocations(result, model, host.pathOfModel);
    },
  });

  // ⚠ 重命名**只在服务器声明支持时才注册**（见 LspServerCapabilities.supportsRename）。
  //   不声明也注册的话，按下 F2 会弹出一个输入框，输完却什么也没发生 ——
  //   而 json 服务器就是这种情况
  if (supportsRename) {
    monaco.languages.registerRenameProvider(languageId, {
      provideRenameEdits: async (model, position, newName) => {
        const path = host.pathOfModel(model);
        if (path === null) return undefined;

        // 一条真有用的日志（不是临时脚手架）：出问题时第一件要确认的事，
        // 就是「这个请求到底发出去没有」。VS Code 的输出面板里也记这些
        host.log(`[LSP] 重命名 ${newName}：${path} 第 ${position.lineNumber} 行`);

        const result = await requestRename(path, toPosition(position), newName);
        if (result === null) return undefined;

        const { edits, targets } = toWorkspaceEdit(result, host);
        if (edits.length === 0) return undefined;

        host.log(`[LSP] 重命名结果：${edits.length} 处编辑，涉及 ${targets.length} 个文件`);

        // ★★ 先把涉及到的每个文件都保证「有 model」，再交给 Monaco 去应用。
        //   顺序不能反 —— bulk edit 找不到 model 是直接抛的（见 host.ensureModel）
        for (const target of targets) await host.ensureModel(target);

        return { edits };
      },
    });
  }
}

/**
 * 注册「打开另一个文件」的处理函数。**全局只调一次**。
 *
 * ★★ 为什么非做不可：Monaco 的 standalone 版里，跳转定义最后会走到
 *   `ICodeEditorService.openCodeEditor(...)`，而它默认那套实现的判据是
 *   「目标和**当前** model 是同一个吗」—— 不是就返回 null，也就是**放弃**。
 *   （实测源码：`findModel(editor, resource)` → `uri 不相等` → `return null`）
 *   所以「跳到一个还没打开的文件」在 standalone 里本来是**根本不会发生**的。
 *
 * ★ 为什么可以挂上去：`openCodeEditor` 是**遍历**所有处理函数、
 *   谁先返回非 null 就用谁，而 `registerCodeEditorOpenHandler` 用的是
 *   `unshift` —— **后注册的排在前面**。所以我们在编辑器建好之后注册，
 *   就一定能抢在那套默认实现之前拿到机会；我们返回 null 时它照旧兜底。
 *
 * ⚠ 代价：`ICodeEditorService` 没有公开的获取途径（`monaco.editor.*` 里
 *   没有这样的函数，公开类型里也没导出这个接口），只能从 `StandaloneServices`
 *   这个内部容器里取，并自己声明用到的那一个方法 ——
 *   全项目就这一处碰内部，声明集中在下面和 `monacoInternals.d.ts`
 */
export function registerLspOpenHandler(host: LspFeatureHost): void {
  const service = StandaloneServices.get<CodeEditorServiceLike>(ICodeEditorService);

  service.registerCodeEditorOpenHandler(async (input) => {
    const uri = input.resource?.toString() ?? "";
    // ⚠ 只接 `file://`。同一个文件里跳转时 uri 是匿名的 `inmemory://model/N`，
    //   那种情况交给默认实现（它认这个 uri，会直接把光标挪过去）。
    //   不判的话 `uriToPath` 会把 "inmemory://model/3" 当成路径，
    //   然后真的去磁盘上找一个叫 inmemory 的文件 —— 症状是弹出一个打不开的标签
    if (!uri.startsWith("file://")) return null;

    return host.openLocation(uriToPath(uri), input.options?.selection ?? null);
  });
}
