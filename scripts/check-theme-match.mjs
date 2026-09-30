// 判断一个 scope 在真实的 VS Code 主题里到底有没有颜色。
//
// ★ 为什么需要它：`pickThemeScope` 挑出来的 scope 好不好，唯一的判据是
//   「它在主题里能不能匹配到规则」。光看 scope 名字像不像「词法类别」是猜，
//   猜错的代价是「本来有色的变成默认色」—— 而且不报错。
//
//   用法：node scripts/check-theme-match.mjs <scope> [<scope> ...]

import fs from "node:fs";
import path from "node:path";

const THEME_FILES = [
  "D:/vscode/Microsoft VS Code/04c0d99f4f/resources/app/extensions/theme-defaults/themes/dark_modern.json",
  "D:/vscode/Microsoft VS Code/04c0d99f4f/resources/app/extensions/theme-defaults/themes/dark_plus.json",
  "D:/vscode/Microsoft VS Code/04c0d99f4f/resources/app/extensions/theme-defaults/themes/dark_vs.json",
];

/** 递归把 include 链拉平：父在前，自己被自己覆盖（和 vscodeTheme.ts 一样） */
function loadTheme(file, seen = new Set()) {
  if (seen.has(file) || !fs.existsSync(file)) return [];
  seen.add(file);

  const raw = JSON.parse(fs.readFileSync(file, "utf8"));
  const rules = [];

  for (const include of raw.include ? [raw.include] : []) {
    const next = path.resolve(path.dirname(file), include);
    rules.push(...loadTheme(next, seen));
  }

  for (const entry of raw.tokenColors ?? []) {
    // scope 有三种写法：单字符串 / 逗号分隔 / 数组 —— 都要摊平
    const scopes = Array.isArray(entry.scope)
      ? entry.scope
      : typeof entry.scope === "string"
        ? entry.scope.split(",").map((s) => s.trim())
        : [];
    for (const scope of scopes) {
      // 带空格的 scope 表达的是「层级」，Monaco 的字典树表达不了，跳过
      if (!scope || scope.includes(" ")) continue;
      rules.push({ scope, foreground: entry.settings?.foreground });
    }
  }

  return rules;
}

const rules = THEME_FILES.flatMap((file) => loadTheme(file));

/** 模拟 Monaco 的 tokenTheme 匹配：按 `.` 逐段前缀，从最具体往最泛找 */
function matchColor(scope) {
  const parts = scope.split(".");
  for (let take = parts.length; take >= 1; take -= 1) {
    const prefix = parts.slice(0, take).join(".");
    const hit = [...rules].reverse().find((r) => r.scope === prefix);
    if (hit) return { prefix, foreground: hit.foreground ?? "(只有 fontStyle)" };
  }
  return null;
}

for (const scope of process.argv.slice(2)) {
  const hit = matchColor(scope);
  console.log(`${scope}`);
  console.log(
    hit
      ? `  → 匹配到 "${hit.prefix}"，前景色 ${hit.foreground}`
      : `  → 没有任何规则命中 ⇒ 渲染成**默认色**`,
  );
}
