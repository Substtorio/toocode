// TextMate 语法引擎 —— 插件机制真正干活的那一层
//
// 这里干的事：把 VS Code 扩展里的 .tmLanguage.json，变成 Monaco 能用的分词器。
//
// 为什么需要两个包：
//   vscode-oniguruma —— TextMate 语法用的正则是 **Oniguruma** 方言，
//                       和 JS 的 RegExp 不兼容（它支持变长后顾、`\G` 断言等）。
//                       这是个 C 写的引擎，编译成 WASM 才能在浏览器里跑
//   vscode-textmate   —— VS Code 自己的语法解析器：把语法文件编译成状态机，
//                       逐行吐出带 scope 的 token。用的就是 VS Code 里那一份

import * as monaco from "monaco-editor";
import { Registry, parseRawGrammar, type IGrammar, type StateStack } from "vscode-textmate";
import { OnigScanner, OnigString, loadWASM } from "vscode-oniguruma";
// Vite 的 ?url 后缀：不把 .wasm 当模块打包，而是给一个可以 fetch 的地址
import onigWasmUrl from "vscode-oniguruma/release/onig.wasm?url";

/** 一个语法文件：内容 + 它的真实文件名 */
export interface GrammarSource {
  text: string;
  /**
   * ★ 必须是**真实文件名**（带上真实后缀）。
   *   `parseRawGrammar` 靠后缀决定用哪套解析器：
   *   `.json` 直接走 JSON.parse，其它后缀才会去试 JSON / plist 两种。
   *   编一个假名字（比如 `` `${scopeName}.json` ``），
   *   会让 plist 格式的 `.tmLanguage` 走进 JSON.parse 而炸掉
   */
  fileName: string;
}

// scopeName → 语法文件。
// 语法之间会互相 include（Vue 的语法就 include 了 HTML 和 TypeScript 的规则），
// 注册表按 scopeName 回来要的时候得有东西给它。
// 读过的就留着 —— 一个语法被 include 好几次时不用重复读盘
const grammarSources = new Map<string, GrammarSource>();

/**
 * 「给我这个 scopeName 的语法」—— 由调用方注入。
 *
 * ★ 为什么要注入而不是在这里直接读文件：
 *   这一层只该懂 TextMate，不该知道文件是从 Tauri 的 IPC 来的还是别的地方来的。
 *   读盘方式变成本地文件 / 内置资源 / 网络，这里一行都不用改
 */
export type GrammarSourceLoader = (scopeName: string) => Promise<GrammarSource | null>;

let registryPromise: Promise<Registry> | null = null;

/**
 * 初始化 TextMate 引擎。只会真正执行一次，后面都是复用同一个 promise。
 *
 * ★ 注意它是「等到真的要分词时才被调用」的 —— 连 oniguruma 的 WASM 都是那一刻才下载。
 *   用户一个需要 TextMate 的语言都没打开，就一个字节都不花
 */
function getRegistry(loadSource: GrammarSourceLoader): Promise<Registry> {
  if (!registryPromise) {
    registryPromise = (async () => {
      // WASM 得先异步加载完，所以整个 Registry 的创建也是异步的
      const response = await fetch(onigWasmUrl);
      await loadWASM(await response.arrayBuffer());

      return new Registry({
        onigLib: Promise.resolve({
          createOnigScanner: (patterns: string[]) => new OnigScanner(patterns),
          createOnigString: (text: string) => new OnigString(text),
        }),
        // ★ 遇到没见过的 scopeName 时 TextMate 会回调这里来要语法 ——
        //   包括**被别的语法 include 进去的**那些（Vue 会要 html / typescript）。
        //   所以按需加载是「顺着依赖链」发生的，不用我们自己去算要预读哪些
        loadGrammar: async (scopeName: string) => {
          let source = grammarSources.get(scopeName);

          if (source === undefined) {
            const fetched = await loadSource(scopeName);
            // 读不到就告诉 TextMate「没有这个语法」，它会当纯文本处理
            if (fetched === null) return null;
            grammarSources.set(scopeName, fetched);
            source = fetched;
          }

          return parseRawGrammar(source.text, source.fileName);
        },
      });
    })();
  }
  return registryPromise;
}

/**
 * Monaco 要求分词器的状态是个带 `clone()` / `equals()` 的对象，
 * 而 TextMate 的状态是它内部的不透明结构，而且用 `null` 表示「从头开始」。
 *
 * 所以两边都得让一步，这里包一层做翻译：
 *   · clone() 直接返回 this —— TextMate 的状态栈是**不可变**的：
 *     每次 tokenizeLine 都返回一个新的 ruleStack，老的不会被改，
 *     所以「克隆」共享同一个对象是安全的
 *   · equals() 交给 TextMate 自己比。Monaco 靠它判断
 *     「改了上面某行之后，下面哪些行还能复用之前的分词结果」——
 *     永远返回 false 只是变慢，返回错的 true 就会**染错颜色**，
 *     所以拿不准时必须返回 false
 */
class TmState implements monaco.languages.IState {
  constructor(readonly inner: StateStack | null) {}

  clone(): TmState {
    return this;
  }

  equals(other: monaco.languages.IState): boolean {
    if (!(other instanceof TmState)) return false;
    if (this.inner === other.inner) return true;

    const mine = this.inner as unknown as { equals?: (target: unknown) => boolean } | null;
    return typeof mine?.equals === "function" ? mine.equals(other.inner) : false;
  }
}

/**
 * 从 TextMate 的 scope 链里挑一个「最适合交给 Monaco 主题」的。
 *
 * ★★ 这里有一个必须搞清楚的细节（踩过了）：
 *
 *   Monaco 会把 **整个 scopes 字符串** 当 token 类型，直接扔进它的主题字典树
 *   （standaloneLanguages.js: `tokenTheme.match(languageId, t.scopes)`），
 *   字典树是按 `.` 和 `-` 分段走的。
 *
 *   所以**不能**给「空格分隔的 scope 列表」——
 *   那样它只会拿第一段（通常是 `source.xxx`）去匹配，
 *   一条规则都命中不了，结果整篇都是默认色。
 *   @see https://github.com/microsoft/monaco-editor/blob/main/src/vs/editor/standalone/browser/standaloneLanguages.ts
 *
 * ★ 从后往前挑：TextMate 的 scopes 是从「泛」到「专」排列的，最后一个最具体。
 *   但跳过两类「有层级意义、没有词法类别意义」的 scope：
 *     punctuation.*  —— TextMate 语法几乎给每个括号引号都标一个
 *     meta.*         —— 描述的是结构，不是词法类别
 *   不跳的话，一个字符串里的引号会挑到 punctuation.definition.string.begin，
 *   反而把本来能着色的 string 档掉了
 */
const SKIPPED_SCOPE_HEADS = new Set(["punctuation", "meta", "source", "text"]);

function pickThemeScope(scopes: string[]): string {
  for (let index = scopes.length - 1; index >= 0; index -= 1) {
    const scope = scopes[index];
    if (!SKIPPED_SCOPE_HEADS.has(scope.split(".")[0])) return scope;
  }
  return scopes[scopes.length - 1] ?? "source";
}

/**
 * 把一个 TextMate 语法注册成 Monaco 的分词器。
 *
 * ★ 核心取巧：**把 TextMate 的 scope 名当成 Monaco 的 token 类型用**。
 *
 *   Monaco 的主题规则是按字典树**逐段前缀**匹配的 —— 规则 `{ token: "keyword" }`
 *   能命中 token 类型 `keyword.control.flow.ts`。
 *   而 TextMate 的 scope 命名（`keyword` / `string` / `comment` /
 *   `constant.numeric` / `entity.name.function` / `variable` …）
 *   恰好和 Monaco 内置主题里那套名字对得上。
 *
 *   于是**不需要把 TextMate 主题也转换过来** —— 内置的 vs-dark / vs 就能上色。
 *   这是让整件事从「要写一个主题转换器」缩到「一个适配函数」的关键。
 *
 * ★ 这里**不读语法文件**，只登记「这个语言由哪个语法负责」。
 *   本机装了 59 个语法、加起来 2 MB —— 启动时全读进来编译一遍是纯粹的浪费：
 *   用户一次顶多打开两三种语言的文件。真正读盘发生在
 *   `create()` 里，也就是**第一次给这个语言分词**的时候
 */
export function registerTextMateLanguage(options: {
  languageId: string;
  scopeName: string;
  loadSource: GrammarSourceLoader;
}): void {
  const { languageId, scopeName, loadSource } = options;

  // 语言 id 得先注册过，否则 createModel(content, id) 会退化成纯文本。
  // Monaco 已经认识这个 id（比如内置的 typescript）就别重复注册 —— 它会警告
  const known = monaco.languages.getLanguages().some((language) => language.id === languageId);
  if (!known) monaco.languages.register({ id: languageId });

  // ★ 用「工厂」而不是直接 setTokensProvider：
  //   create() 可以返回 Promise，Monaco 会在**第一次真要给这个语言分词时**才调它。
  //   这就是按需加载的落点 —— 语法文件是用户打开这种文件的那一刻才读的
  monaco.languages.registerTokensProviderFactory(languageId, {
    create: async () => {
      let grammar: IGrammar | null = null;

      try {
        grammar = await (await getRegistry(loadSource)).loadGrammar(scopeName);
      } catch (error) {
        // 语法文件坏了只影响这一个语言，不能拖垮编辑器
        console.warn(`[textmate] 语法 ${scopeName} 编译失败：`, error);
      }

      // 返回 null = 告诉 Monaco「这个语言我没有分词器」，它会退回纯文本。
      // 这是刻意的降级：坏一个语法，其它功能照旧
      return grammar ? createTokensProvider(grammar) : null;
    },
  });
}

/** 把编译好的 TextMate 语法包成 Monaco 要的分词器 */
function createTokensProvider(grammar: IGrammar): monaco.languages.TokensProvider {
  return {
    getInitialState: () => new TmState(null),

    tokenize: (line: string, state) => {
      const previous = state instanceof TmState ? state.inner : null;
      const result = grammar.tokenizeLine(line, previous);

      return {
        tokens: result.tokens
          // 丢掉零长度的 token —— Monaco 期望的是「一段一段盖满整行」
          .filter((token) => token.endIndex > token.startIndex)
          .map((token) => ({
            startIndex: token.startIndex,
            scopes: pickThemeScope(token.scopes),
          })),
        endState: new TmState(result.ruleStack),
      };
    },
  };
}
