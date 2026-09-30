// 把 VS Code 的代码片段接成 Monaco 的补全项
//
// 数据流：Rust 扫出「语言 → 片段文件路径」→ 前端读文件 → snippetParser 解析
//          → 这里注册成 CompletionItemProvider
//
// ★ 这是「插件机制」的第四块（前三块是 grammars / themes / injectTo）。
//   它和前三块有个共同的取巧：**不去自己发明一套格式** ——
//   VS Code 的片段文件本来就是纯 JSON，Monaco 的补全项要的东西
//   （label / insertText / documentation）恰好都能一一对应

import * as monaco from "monaco-editor";
import { matchedPrefixLength, type ParsedSnippet } from "./snippetParser";

/** 已经注册过补全的语言 —— Monaco 允许重复注册，但重复就是白跑 */
const registeredLanguages = new Set<string>();

/**
 * 给一个语言注册片段补全。
 *
 * ⚠ 这段代码**不认识文件、不认识磁盘**，只收一份解析好的片段数组 ——
 *   和 textmate.ts 一样的分层：底层只懂自己那件事，
 *   「片段是从哪读来的」不影响它
 */
export function registerSnippetCompletions(
  languageId: string,
  snippets: readonly ParsedSnippet[],
): void {
  if (snippets.length === 0 || registeredLanguages.has(languageId)) return;
  registeredLanguages.add(languageId);

  monaco.languages.registerCompletionItemProvider(languageId, {
    provideCompletionItems(model, position) {
      // 光标**之前**那一整行的内容。column 是 1 起的，
      // 所以 `slice(0, column - 1)` 正好是光标左边的部分。
      //
      // ★ 不去用 `model.getWordUntilPosition()` —— 理由见上面
      //   matchedPrefixLength 的说明（真实前缀里有 `.` 和 `()`）
      const before = model.getLineContent(position.lineNumber).slice(0, position.column - 1);

      const suggestions: monaco.languages.CompletionItem[] = [];

      for (const snippet of snippets) {
        const matched = matchedPrefixLength(before, snippet.prefix);
        if (matched < 0) continue; // 对不上，跳过

        // ★★ range 告诉 Monaco「插入时**替掉**从哪到哪」。
        //   不设的话插入是**追加** —— 敲 `log` 再补全会变成 `loglog(...)`。
        //   而替掉多少，就是**这个 prefix 自己**对上的长度（每条各算各的）
        const range: monaco.IRange = {
          startLineNumber: position.lineNumber,
          endLineNumber: position.lineNumber,
          startColumn: position.column - matched,
          endColumn: position.column,
        };

        suggestions.push({
          label: snippet.prefix,
          kind: monaco.languages.CompletionItemKind.Snippet,
          // ⚠ 这里**必须**用 InsertAsSnippet：
          //   不设的话 `$1` / `${2:name}` 会被当**字面量**插进代码里，
          //   变成 `console.log($1);` 这种一看就坏的文本
          insertText: snippet.body,
          insertTextRules: monaco.languages.CompletionItemInsertTextRule.InsertAsSnippet,
          detail: snippet.description ?? "代码片段",
          documentation: snippet.body,
          // 片段排在普通单词补全前面（"0" 前缀让它在字典序里靠前）
          sortText: `0${snippet.prefix}`,
          range,
        });
      }

      // 一条都没有时返回 undefined 而不是空数组 ——
      // 空数组也会把 Monaco 的列表「接管」掉，让它不再去问别的 provider
      return suggestions.length > 0 ? { suggestions } : undefined;
    },
  });
}
