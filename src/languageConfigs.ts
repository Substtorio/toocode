// 把 VS Code 的「语言配置」接进 Monaco
//
// 数据流：Rust 扫出「语言 id → language-configuration.json 路径」→ 前端读文件
//          → monaco.languages.setLanguageConfiguration()
//
// ★ 这是「插件机制」的第五块（前四块是 grammars / themes / injectTo / snippets）。
//   但它管的和那几块**不是一回事**：那几块决定「代码长什么样」，
//   这一块决定「**打字时会发生什么**」——
//   Ctrl+/ 插什么注释符、敲 `{` 会不会自动补 `}`、回车时缩进几格、
//   双击选中的「一个词」到哪为止。这些全是 `language-configuration.json` 里的数据，
//   而 VS Code 的扩展本来就得声明它 —— 我们只是把这份数据拿来用
//
// ⚠ 和语法 / 主题同一个思路：**不自己造一份配置表**。自己写的话，
//   先不说要维护多少种语言，光是「PHP 的注释符是 // 和 # 两种」这类事实就没完没了

import * as monaco from "monaco-editor";
import { invoke } from "@tauri-apps/api/core";

/**
 * JSON 里的正则字段是**字符串**，而 Monaco 要 `RegExp` 对象。
 *
 * ⚠ 从宽处理：转不出来就返回 `undefined`（调用方把这个键丢掉），
 *   而不是整份配置放弃 —— 一个写坏的正则不该让「这个语言的注释符」也一起没
 */
function toRegExp(source: unknown): RegExp | undefined {
  if (typeof source !== "string" || source === "") return undefined;

  // ★★ 含 `\p{...}` / `\P{...}` 的**必须**用 unicode 模式：
  //   不加 `u` 时 `\p` 会被当成「转义的字母 p」，**不报错但语义是错的**
  //   （C++ / C# 的 wordPattern 就是这么写的）。
  //   症状是「双击选中的范围不对」，而几乎没人会联想到正则标志
  const needsUnicode = /\\[pP]\{/.test(source);

  try {
    return new RegExp(source, needsUnicode ? "u" : "");
  } catch {
    // 反过来再试一次：有些写法在普通模式下合法、在 `u` 模式下反而非法（如 `\-`）
    try {
      return new RegExp(source, needsUnicode ? "" : "u");
    } catch {
      return undefined;
    }
  }
}

/**
 * `language-configuration.json` 的形状 → Monaco 要的。
 *
 * ★ 两边字段名**几乎一样**（VS Code 这套本来就和 Monaco 同源），只有几处要抹平：
 *   · `wordPattern` / `indentationRules.*` / `onEnterRules[].beforeText|afterText`
 *     在 JSON 里是字符串，Monaco 要 `RegExp`
 *   · `comments` / `brackets` / `autoClosingPairs` / `surroundingPairs`
 *     名字和形状完全一致，直接透传
 * ⚠ 认不出来的键**留着**而不是删掉：Monaco 只读它认识的，多带几个是安全的；
 *   而删掉的话，以后 Monaco 支持新键我们就自动落后了
 */
function toMonacoConfig(raw: Record<string, unknown>): monaco.languages.LanguageConfiguration {
  const config: Record<string, unknown> = { ...raw };

  const wordPattern = toRegExp(raw.wordPattern);
  if (wordPattern === undefined) delete config.wordPattern;
  else config.wordPattern = wordPattern;

  const indentation = raw.indentationRules;
  if (typeof indentation === "object" && indentation !== null) {
    const rules = indentation as Record<string, unknown>;
    config.indentationRules = {
      ...rules,
      increaseIndentPattern: toRegExp(rules.increaseIndentPattern),
      decreaseIndentPattern: toRegExp(rules.decreaseIndentPattern),
      indentNextLinePattern: toRegExp(rules.indentNextLinePattern),
      unIndentedLinePattern: toRegExp(rules.unIndentedLinePattern),
    };
  }

  const onEnterRules = raw.onEnterRules;
  if (Array.isArray(onEnterRules)) {
    config.onEnterRules = onEnterRules.map((rule) => {
      const item: Record<string, unknown> = { ...(rule as Record<string, unknown>) };
      item.beforeText = toRegExp(item.beforeText);
      item.afterText = toRegExp(item.afterText);
      item.previousLineText = toRegExp(item.previousLineText);
      return item;
    });
  }

  return config as monaco.languages.LanguageConfiguration;
}

/**
 * 等 Monaco 的语言注册表**有内容**了再返回它。
 *
 * ★★ 为什么不能直接 `getLanguages()`：`basic-languages` 那个 contribution
 *   是**异步**跑完的 —— 同步拿到的是**一张空表**。
 *   而空表的后果不是报错，是「所有语言配置都被跳过」⇒ 静默全失效。
 *   （App.vue 里 `monacoLanguageMaps` 必须懒加载，是同一个坑）
 */
async function waitForLanguageIds(): Promise<Set<string>> {
  for (let attempt = 0; attempt < 20; attempt += 1) {
    const languages = monaco.languages.getLanguages();
    if (languages.length > 0) {
      return new Set(languages.map((language) => language.id));
    }
    await new Promise<void>((resolve) => {
      setTimeout(resolve, 50);
    });
  }
  return new Set();
}

/** 扫一遍扩展里的语言配置，逐份读出来交给 Monaco */
export async function loadLanguageConfigurations(): Promise<void> {
  let entries: Array<{ id: string; path: string }>;
  try {
    const loaded = await invoke<Array<{ id: string; path: string }> | null>(
      "scan_language_configurations",
    );
    // ★★ 形状要自己验：`invoke` 回 `null` **不会**抛异常，所以上面的 `catch` 接不住它
    //   —— 而 `for...of null` 会抛，**直接把 onMounted 从那里斩断**，
    //   后面那些「锦上添花」的初始化（包括 hot exit 恢复）就全都不跑了。
    //   ★ 教训：**有 try/catch 不等于安全** —— 先确定「失败」是抛出来的还是返回的。
    //   （`dap.availableAdapters` 上踩过一模一样的坑）
    entries = Array.isArray(loaded) ? loaded : [];
  } catch (error) {
    console.warn(`[语言配置] 扫描失败（不影响其它功能）：${String(error)}`);
    return;
  }

  const known = await waitForLanguageIds();

  /** 备好的配置（语言 id → 配置） */
  const configs = new Map<string, monaco.languages.LanguageConfiguration>();
  for (const entry of entries) {
    // ★ 只给 Monaco **已经认识**的语言设置。
    //   `setLanguageConfiguration` 对没注册过的 id 会顺手把它**注册**进去 ——
    //   于是 `getLanguages()` 会多出一批我们没有分词器的语言，
    //   而那张表还被「扩展名 → 语言」用着（见 App.vue 的 monacoLanguageMaps）
    if (!known.has(entry.id)) continue;

    try {
      const raw = await invoke<string>("read_file", { path: entry.path });
      configs.set(entry.id, toMonacoConfig(JSON.parse(raw) as Record<string, unknown>));
    } catch (error) {
      // 单份读失败 / 不是合法 JSON 都跳过 —— 一百多份里有一份坏的，
      // 不该让其它语言的打字行为一起没
      console.warn(`[语言配置] ${entry.id} 加载失败（已跳过）：${String(error)}`);
    }
  }

  // ★★★ 设置时机是这块唯一的坑，而且**极其隐蔽** ——
  //   调用了、返回值正常、日志也说"设好了"，就是不起作用，而且一声不响。
  //
  // 根因在 Monaco 自己的实现里（`vs/languages/definitions/_.contribution.js`）：
  //
  //     languages.onLanguageEncountered(languageId, async () => {
  //         const mod = await lazyLanguageLoader.load();      // ← 一次动态 import
  //         languages.setLanguageConfiguration(languageId, mod.conf);
  //     });
  //
  // —— 内置那份配置藏在**一次动态 import** 后面。所以：
  //   · 在 onMounted 里直接设 ⇒ 用户第一次打开 .js 时被它覆盖掉
  //   · 挂在 onLanguage 上再 `setTimeout(0)` ⇒ 那个 import 要几十毫秒，
  //     一个宏任务根本追不上
  //   ⇒ 正确的信号是「**这个 model 真的被分词过一次**」—— 那说明 import 已完成。
  //     再延一个宏任务，我们才是最后说话的那个
  // 实测：直接设时 Ctrl+/ 出来的是内置的 `//`；用这里的时机才是我们配的那个
  const pending = new Set(configs.keys());

  const applyWhenReady = (model: monaco.editor.ITextModel): void => {
    const id = model.getLanguageId();
    // 一个语言只处理一次（内置那份也只设一次）
    if (!pending.delete(id)) return;
    const config = configs.get(id);
    if (config === undefined) return;

    let subscription: monaco.IDisposable | undefined;
    // ⚠ 这个事件在 `ITextModel` 的**公开类型**里没有（0.56 把它挪到了内部的
    //   `tokenization` 部件上，而 `model.tokenization` 同样不在公开类型里），
    //   但**运行时确实存在** —— 实测这个信号是准的。
    //   ★ 用 `@ts-expect-error` 而不是 `as any`：升级 Monaco 之后如果它真进了
    //     公开类型，这条抑制指令会自己报「未使用」，等于给我们留了个提醒。
    //     项目里碰 Monaco 内部的另一处是 `lspFeatures.ts` 拿 `ICodeEditorService`
    // @ts-expect-error 内部 API，见上面的说明
    subscription = model.onDidChangeTokens(() => {
      subscription?.dispose();
      setTimeout(() => {
        monaco.languages.setLanguageConfiguration(id, config);
      }, 0);
    });
  };

  // 已经开着的 model 和以后新建的都要管
  for (const model of monaco.editor.getModels()) applyWhenReady(model);
  monaco.editor.onDidCreateModel(applyWhenReady);

  console.log(`[语言配置] 备好 ${configs.size} 份，语言被用到时应用（扫到 ${entries.length} 份）`);
}
