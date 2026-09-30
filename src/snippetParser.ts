// VS Code 代码片段文件的解析
//
// ★ 为什么单独一个文件、而且**不 import 任何东西**：
//   和 `fuzzy.ts` / `pathUtils.ts` 一个思路 —— 解析规则很容易写错，
//   而错了只表现为「补全列表里的内容不对」，靠眼睛看不出来。纯函数才能单独跑用例。

/** 一条解析好的代码片段 */
export interface ParsedSnippet {
  /** 触发词 */
  prefix: string;
  /** 要插入的文本（body 数组已经拼成多行） */
  body: string;
  description?: string;
  /**
   * 这条片段限定在哪些语言里生效（来自 `scope` 字段）。
   * `null` = 文件里没写 scope，由调用方按「文件声明的语言」处理
   */
  languages: string[] | null;
}

/** 把一个字段摊平成字符串数组 —— 它可能是单值、数组、或者根本没有 */
function toStringList(value: unknown): string[] {
  if (typeof value === "string") return [value];
  if (Array.isArray(value)) {
    return value.filter((item): item is string => typeof item === "string");
  }
  return [];
}

/**
 * 解析一个片段文件。
 *
 * 格式是「名字 → 片段定义」的对象：
 * ```json
 * {
 *   "Print to console": {
 *     "prefix": "log",
 *     "body": ["console.log($1);", "$0"],
 *     "description": "打印到控制台",
 *     "scope": "javascript,typescript"
 *   }
 * }
 * ```
 *
 * ★★ 四个字段各有各的脾气，这是最容易漏的一处：
 *   - `prefix` 可以是**数组**（多个触发词指向同一条片段）
 *   - `body` 可以是**单个字符串**，也可以是**多行数组**
 *   - `scope` 是**逗号分隔的字符串**（不是数组！）
 *   - `description` 可能压根没有
 *   漏掉任何一个，症状都是「某条片段莫名不出现」或者「出现一条空的」，
 *   而且不报错
 *
 * ★ body 里的 `$1` / `${2:name}` / `$0` 是**占位符**，这里原样保留 ——
 *   交给 Monaco 处理（注册时要设 `InsertAsSnippet`，否则会被当字面量插进去）
 */
export function parseSnippetFile(text: string): ParsedSnippet[] {
  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch {
    // 片段文件坏了只影响这一个文件，不该拖垮整条链路
    return [];
  }

  // 顶层必须是对象。有的老片段文件是数组格式，不做兼容
  if (raw === null || typeof raw !== "object" || Array.isArray(raw)) return [];

  const snippets: ParsedSnippet[] = [];

  for (const [name, value] of Object.entries(raw as Record<string, unknown>)) {
    if (value === null || typeof value !== "object" || Array.isArray(value)) continue;

    const definition = value as Record<string, unknown>;

    // body 是必需的 —— 没有它这条片段没有任何意义
    const bodyParts = toStringList(definition.body);
    if (bodyParts.length === 0) continue;

    // 没写 prefix 时拿片段的名字当触发词（VS Code 也是这么做的）。
    // 过滤空串：`"prefix": ""` 会变成一条永远触发不了的片段
    const prefixes = toStringList(definition.prefix).filter((item) => item.trim() !== "");
    if (prefixes.length === 0) prefixes.push(name);

    // ⚠ scope 是**逗号分隔的字符串**，不是数组 —— 别按数组处理
    const scope = toStringList(definition.scope)
      .flatMap((item) => item.split(","))
      .map((item) => item.trim())
      .filter(Boolean);

    const description =
      typeof definition.description === "string" ? definition.description : undefined;

    // ★ prefix 是数组时，每个触发词都出一条 —— 它们指向同一份 body
    for (const prefix of prefixes) {
      snippets.push({
        prefix,
        body: bodyParts.join("\n"),
        description,
        languages: scope.length > 0 ? scope : null,
      });
    }
  }

  return snippets;
}

/** 光标前面那个字符算不算「词的一部分」 */
function isWordChar(char: string | undefined): boolean {
  return char !== undefined && /[A-Za-z0-9_$]/.test(char);
}

/**
 * 光标前的文本末尾，和这条片段的前缀对上了几个字符？
 *
 * 返回值：
 *   - 正数 = 用户已经敲了这么多个字符（插入时要**替掉**它，长度就是它）
 *   - 0    = 还没开始敲（光标正好在词边界上），整条片段都该列出来
 *   - -1   = 对不上，这条不该出现
 *
 * ★★ 判据是「**prefix 以用户敲完的那个词开头**」，不是「末尾有几个字符碰巧一样」。
 *   这个差别很实在，我第一版就写错了：
 *   按「末尾最长匹配」来算，敲 `sout` 时 `sysout` 会匹配上第 1 个字符 `s`，
 *   于是那条**不该出现**的片段冒了出来，而且插入时只替掉 `s`，
 *   结果是 `souSystem.out...` 这种一看就坏的东西。
 *
 * ★★ 为什么不用 `model.getWordUntilPosition()`（这是踩到之后才改的）：
 *   它按 `[a-zA-Z0-9_]` 圈词，而**真实的片段前缀里有点和括号** ——
 *   Java 扩展里就有 `"System.out.println()"`。用户敲 `Sys` 时，
 *   `getWordUntilPosition` 只圈出 3 个字符，插入时就只替掉 3 个，
 *   结果是 `System.out.println()System.out.println($0);`。
 *   所以得**拿前缀自己去比**，不能借 Monaco 对「词」的定义。
 *   （敲到 `System` 时仍然工作：`.` 不是词字符，所以「已输入的词」正好是 `System`）
 *
 * ★ 放在这个文件里（而不是 snippets.ts）：snippets.ts 要 import monaco，
 *   在 Node 里跑不起来。而这段逻辑恰恰是「错了也看不出来」的那种 ——
 *   必须能单独跑用例
 */
export function matchedPrefixLength(before: string, prefix: string): number {
  // 先找出「光标前那个词」：从光标往回退，退到词边界为止
  let wordStart = before.length;
  while (wordStart > 0 && isWordChar(before[wordStart - 1])) wordStart -= 1;

  const typed = before.slice(wordStart);

  // 光标正好贴在词边界上 ⇒ 用户还没开始输入，整条都列出来
  if (typed === "") return 0;

  return prefix.toLowerCase().startsWith(typed.toLowerCase()) ? typed.length : -1;
}
