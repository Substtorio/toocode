/**
 * 容忍注释的 JSON 解析（JSONC）。
 *
 * ★ 为什么需要它：VS Code 的 `language-configuration.json` **允许写注释**
 *   （它们按 JSONC 对待），而实测本机扫到的 77 份里有 **3 份**带注释
 *   （`rnc` / `vue` / `markdown`）—— 直接用 `JSON.parse` 会整个跳过，
 *   于是这几种语言拿不到「注释符 / 括号补全 / 回车缩进」这些配置。
 *   而症状只是「Ctrl+/ 出来的是 //」这种**不报错、只是少一点东西**的样子
 *   （控制台里确实有一行 warn，但没人会去看）
 *
 * ⚠★ **不能简单地正则删 `//`**：字符串里的 `//` 会被误删 ——
 *   实测真实文件里就有 `"wordPattern": "..."` 这种值，虽然当前没有含 `//` 的，
 *   但 `"lineComment": "//"` 这种**遍地都是**（注释符本身就是 // ！）。
 *   一删就变成 `"lineComment": "` ⇒ 整份文件语法错
 *   ⇒ 所以必须扫一遍，记住「现在在不在字符串里」
 *
 * ⚠ 去注释之后**不处理尾随逗号**（`[1, 2, ]`）：实测那 3 份文件只有注释问题，
 *   没见到尾随逗号。真碰到了再加 —— 现在加反而要写更多容易出错的逻辑
 */
export function parseJsonc(text: string): unknown {
  let out = "";
  let inString = false;
  let inLineComment = false;
  let inBlockComment = false;

  for (let index = 0; index < text.length; index += 1) {
    const char = text[index];
    const next = text[index + 1];

    if (inLineComment) {
      // 行注释到换行结束。★ 换行要留住 —— 不然整份文件会挤成一行，
      // 万一将来 JSON 的报错信息里带行号就全对不上了
      if (char === "\n") {
        inLineComment = false;
        out += char;
      }
      continue;
    }

    if (inBlockComment) {
      if (char === "*" && next === "/") {
        inBlockComment = false;
        index += 1;
      }
      continue;
    }

    if (inString) {
      out += char;
      // 转义符要连它后面的字符一起吞掉，否则 `\"` 会被当成字符串结束
      if (char === "\\") {
        out += next ?? "";
        index += 1;
        continue;
      }
      if (char === '"') inString = false;
      continue;
    }

    if (char === '"') {
      inString = true;
      out += char;
      continue;
    }
    if (char === "/" && next === "/") {
      inLineComment = true;
      index += 1;
      continue;
    }
    if (char === "/" && next === "*") {
      inBlockComment = true;
      index += 1;
      continue;
    }

    out += char;
  }

  return JSON.parse(out);
}
