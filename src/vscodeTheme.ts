// 把 VS Code 的主题文件翻译成 Monaco 能用的主题。
//
// ★ 为什么非做不可（这是实测出来的）：
//
//   Monaco 内置的 `vs-dark` / `vs` 只有 **45 条规则** —— 而且它们是从 Monaco
//   自己那套 **Monarch** 分词器的 token 命名里抽出来的。
//   可我们用的是 TextMate 语法，它的 scope 命名空间大得多：
//   标签名是 `entity.name.tag`、属性名是 `entity.other.attribute-name`、
//   CSS 选择器是 `entity.other.attribute-name.class.css`……
//   这些名字**在 Monaco 的主题里压根没有对应规则** ——
//   匹配不到就渲染成默认色，看起来就是「这个文件没什么颜色」。
//
//   VS Code 真正在用的 Dark+ 主题里，这些规则**全都有**：
//     entity.name.tag                  → #569cd6（蓝）
//     entity.other.attribute-name      → #9cdcfe（浅蓝）
//     entity.other.attribute-name.class.css → #d7ba7d
//     punctuation.definition.tag       → #808080
//   规则数量：dark_vs(85 个 scope) + dark_plus(84 个) ≈ 169，是内置那 45 条的三倍多。
//
//   好在两边**格式几乎一样**：VS Code 主题的 `colors` 键名和 Monaco 完全一致，
//   `tokenColors[].settings.foreground` 就是 Monaco 的 `rules[].foreground`。
//   所以这不是「写一个主题转换器」，只是把几处写法差异抹平。

import type * as monaco from "monaco-editor";

/** VS Code 主题文件里我们用得上的部分 */
interface RawTheme {
    /** 引用另一个主题文件（相对当前文件）。Dark+ 就是 `dark_plus.json` include `dark_vs.json` */
    include?: string;
    tokenColors?: Array<{
        scope?: string | string[];
        settings?: { foreground?: string; fontStyle?: string };
    }>;
    /**
     * 外观配色：背景、前景、行号、选区、光标、缩进参考线、查找高亮、滚动条……
     * ★ 键名和 Monaco 是**同一套**（Monaco 的颜色注册表本来就是从 VS Code 里抠出来的），
     *   所以不需要做映射表，直接透传就行
     */
    colors?: Record<string, unknown>;
}

/** `'#569cd6'` → `'569CD6'`。Monaco 不要那个 `#` */
function toMonacoColor(color: string | undefined): string | undefined {
    if (!color) return undefined;

    const hex = color.replace(/^#/, "").trim();
    // 只认 #RRGGBB / #RRGGBBAA —— 主题里偶尔有 `"foreground": ""` 这种空值
    return /^[0-9a-fA-F]{6,8}$/.test(hex) ? hex.toUpperCase() : undefined;
}

/**
 * 把 scope 写法摊平成一条条规则名。
 *
 * VS Code 的 scope 有三种写法，都得处理：
 *   · 单个字符串        "entity.name.tag"
 *   · 逗号分隔          "entity.name.tag, meta.tag"   ← 一个字段里塞多条
 *   · 数组              ["entity.name.tag", "meta.tag"]
 *
 * ★ 还有一种带空格的：`source.css entity.other.attribute-name.class`
 *   —— 意思是「在 source.css 里面才匹配后者」。Monaco 的主题规则表达不了这个层级，
 *   只能跳过。跳过不会丢什么：同一份主题里通常还有一条不带空格的
 *   `entity.other.attribute-name.class.css`，那条照样命中
 */
function flattenScopes(scope: string | string[] | undefined): string[] {
    if (!scope) return [];

    const list = Array.isArray(scope) ? scope : [scope];
    return list
        .flatMap((item) => item.split(","))
        .map((item) => item.trim())
        .filter((item) => item.length > 0 && !item.includes(" "));
}

/** 把 `./themes/dark_vs.json` 这样的相对引用，解析成和 base 同风格的绝对路径 */
function resolveRelative(basePath: string, relative: string): string {
    // 我们的路径统一用 /（Rust 侧已经转过了），所以这里直接按 / 拆
    const stack = basePath.split("/").slice(0, -1);

    for (const part of relative.split("/")) {
        if (part === "" || part === ".") continue;
        if (part === "..") stack.pop();
        else stack.push(part);
    }

    return stack.join("/");
}

/** 读一份主题文件，并把 include 的父主题递归合并进来（父在前，自己被自己的覆盖） */
async function readThemeTree(path: string, readFile: (path: string) => Promise<string>): Promise<RawTheme> {
    let parsed: RawTheme;
    try {
        parsed = JSON.parse(await readFile(path)) as RawTheme;
    } catch {
        // 主题文件坏了只意味着「这个主题没有额外规则」，不能让它把启动搞崩
        return {};
    }

    if (!parsed.include) return parsed;

    const parent = await readThemeTree(resolveRelative(path, parsed.include), readFile);
    return {
        // 父在前、自己被自己覆盖
        tokenColors: [...(parent.tokenColors ?? []), ...(parsed.tokenColors ?? [])],
        // ★ colors 也必须合并：Dark+ 的 tokenColors 在 `dark_plus.json` 里，
        //   但**所有颜色**都在被 include 的 `dark_vs.json` 里（dark_plus 的 colors 是空的）
        colors: { ...(parent.colors ?? {}), ...(parsed.colors ?? {}) },
    };
}

/**
 * 把主题里的 colors 挑成 Monaco 要的`Record<string, string>`。
 *
 * ★ 故意**不**过滤「Monaco 不认识」的键（比如 `activityBar.background` 这种工作台颜色）：
 *   一是要维护一份「Monaco 认哪些键」的清单，它会随 Monaco 版本变，不如不维护；
 *   二是 Monaco 内部把颜色存成一个 map，查不到的键根本没人问 —— 放着无害
 */
function toMonacoColors(raw: Record<string, unknown> | undefined): Record<string, string> {
    const colors: Record<string, string> = {};

    for (const [key, value] of Object.entries(raw ?? {})) {
        // 主题里偶尔有 `""` 或 null，那些得丢掉；剩下的值 VS Code 和 Monaco 写法一致
        if (typeof value !== "string" || value.trim().length === 0) continue;
        colors[key] = value.trim();
    }

    return colors;
}

/**
 * 读一个 VS Code 主题文件，转成 Monaco 的 `IStandaloneThemeData`。
 *
 * ★ `inherit: true` 是关键：新规则**叠在** Monaco 内置主题之上，
 *   而不是替换它。这样那些 VS Code 主题没写的规则（比如调试面板、光标）
 *   还能继续用 Monaco 自带的，我们也不用把整份主题抄一遍
 */export async function loadVscodeTheme(options: {
    path: string;
    /** 读文件（由调用方注入，这一层不该知道文件是从哪来的） */
    readFile: (path: string) => Promise<string>;
    /** Monaco 的基础主题：深色用 vs-dark，浅色用 vs */
    base: monaco.editor.IStandaloneThemeData["base"];
}): Promise<{
    theme: monaco.editor.IStandaloneThemeData;
    /**
     * 原始颜色表（还是 VS Code 的键名）。
     *
     * ★ 为什么要把这个也交出去：主题里的颜色是两个世界的混合体 ——
     *   `editor.*` 归 Monaco（上面的 `theme.colors`），
     *   而 `sideBar.*` / `tab.*` / `statusBar.*` 归**我们自己的 UI**。
     *   后者 Monaco 不认识，只能由调用方挑出来铺到自己的 CSS 变量上
     */
    colors: Record<string, string>;
}> {
    const theme = await readThemeTree(options.path, options.readFile);

    const rules: monaco.editor.ITokenThemeRule[] = [];

    for (const item of theme.tokenColors ?? []) {
        const foreground = toMonacoColor(item.settings?.foreground);
        // VS Code 的 fontStyle 和 Monaco 是同一套写法（"italic" / "bold" / "underline"，
        // 多个用空格连），所以直接搬。空字符串表示「清掉」——那也是合法的取值
        const fontStyle = item.settings?.fontStyle;

        // 一个都没有的条目不用生成规则
        if (!foreground && fontStyle === undefined) continue;

        for (const scope of flattenScopes(item.scope)) {
            rules.push({
                token: scope,
                // ★ 不传的字段不能写成 undefined，Monaco 会把 undefined 当成「0 / 无」，
                //   真的想表达的「这个字段不覆盖」得靠「干脆不放这个 key」
                ...(foreground ? { foreground } : {}),
                ...(fontStyle !== undefined ? { fontStyle } : {}),
            });
        }
    }

    return {
        theme: {
            base: options.base,
            inherit: true,
            rules,
            // ★ 外观配色也一起搬过来：背景、前景、行号、选区、光标、缩进参考线、
            //   括号匹配、查找高亮、滚动条，以及智能提示面板（以后接 LSP 要用）。
            //   这让编辑器内部的观感和 VS Code 一致
            colors: toMonacoColors(theme.colors),
        },
        // ⚠ Monaco 只认 `editor.*` 那一半。`sideBar.*` / `tab.*` / `statusBar.*`
        //   这些工作台颜色得由调用方自己铺到 CSS 变量上（见 App.vue）
        colors: toMonacoColors(theme.colors),
    };
}
