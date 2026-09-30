// 验证注入语法：对比「挂了注入表」和「没挂」时，Vue 里的指令分词结果差多少。
//
// ★ 为什么在 Node 里跑、而不是在应用里看颜色：
//   TextMate 在两个环境里是同一份代码（vscode-textmate + vscode-oniguruma），
//   但应用里要一遍遍改文件、重启、开窗口、拿眼睛判断颜色；
//   这里几秒就能把 token 类型打出来，一眼看出口径变没变。
//
// 用法：node scripts/verify-injections.mjs

import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { Registry, parseRawGrammar } = require("vscode-textmate");
const { OnigScanner, OnigString, loadWASM } = require("vscode-oniguruma");

/** 和 Rust 侧 extension_roots() 找的是同一批目录 */
const EXT_ROOTS = [
  path.join(process.env.USERPROFILE ?? "", ".vscode", "extensions"),
  "D:/vscode/Microsoft VS Code/04c0d99f4f/resources/app/extensions",
];

/** 扫一遍扩展清单：scopeName → 文件路径，外加「谁要注入到谁」的反转表 */
function scan() {
  const byScope = new Map();
  const injections = new Map();

  for (const root of EXT_ROOTS) {
    if (!fs.existsSync(root)) continue;

    for (const dir of fs.readdirSync(root)) {
      const manifestPath = path.join(root, dir, "package.json");
      if (!fs.existsSync(manifestPath)) continue;

      let manifest;
      try {
        manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
      } catch {
        continue;
      }

      for (const grammar of manifest.contributes?.grammars ?? []) {
        const file = path.join(root, dir, grammar.path);
        if (!fs.existsSync(file)) continue;

        if (!byScope.has(grammar.scopeName)) byScope.set(grammar.scopeName, file);

        for (const target of grammar.injectTo ?? []) {
          const scope = target.replace(/^[A-Za-z]+:/, "");
          const list = injections.get(scope) ?? [];
          if (!list.includes(grammar.scopeName)) list.push(grammar.scopeName);
          injections.set(scope, list);
        }
      }
    }
  }

  return { byScope, injections };
}

const { byScope, injections } = scan();

async function makeRegistry({ withInjections }) {
  return new Registry({
    onigLib: Promise.resolve({
      createOnigScanner: (patterns) => new OnigScanner(patterns),
      createOnigString: (text) => new OnigString(text),
    }),
    loadGrammar: async (scopeName) => {
      const file = byScope.get(scopeName);
      if (!file) return null;
      return parseRawGrammar(fs.readFileSync(file, "utf8"), file);
    },
    // 唯一的变量：挂 or 不挂这张表
    ...(withInjections
      ? { getInjections: (scopeName) => injections.get(scopeName) }
      : {}),
  });
}

const SAMPLE = `<template>
  <div v-if="ok" @click="go" :class="cls">{{ msg }}</div>
</template>
`;

// ★ 浏览器里是把 fetch 回来的 ArrayBuffer 交给 loadWASM；
//   但 Node 里它**不接受路径字符串**（会把字符串丢给 WebAssembly.instantiate 而炸掉，
//   报错还顺便把整个 bundle 打出来）。得显式包成 { data: <Uint8Array> }
const onigWasmPath = path.join(
  path.dirname(require.resolve("vscode-oniguruma/package.json")),
  "release",
  "onig.wasm",
);
await loadWASM({ data: fs.readFileSync(onigWasmPath) });

// ── 模拟 textmate.ts 里那段「挑一个交给 Monaco 主题的 scope」──
// 目的：改动 SKIPPED_SCOPE_HEADS 之后，确认「该变色的变了」而且「不该变差的没变差」
const SKIP_OLD = new Set(["punctuation", "meta", "source", "text"]);
const SKIP_NEW = new Set(["punctuation", "source", "text"]);

function pick(scopes, skipped) {
  for (let i = scopes.length - 1; i >= 0; i -= 1) {
    if (!skipped.has(scopes[i].split(".")[0])) return scopes[i];
  }
  return scopes[scopes.length - 1] ?? "source";
}

async function run(withInjections) {
  const registry = await makeRegistry({ withInjections });
  const grammar = await registry.loadGrammar("text.html.vue");
  if (!grammar) return { error: "没能加载 text.html.vue" };

  let state = null;
  const lines = SAMPLE.split("\n");
  const result = {};

  lines.forEach((line) => {
    const { tokens, ruleStack } = grammar.tokenizeLine(line, state);
    state = ruleStack;
    for (const word of ["v-if", "@click", ":class", '"ok"', "\"go\"", "cls", "msg"]) {
      if (!line.includes(word)) continue;
      const at = line.indexOf(word);
      const token = tokens.find((t) => t.startIndex <= at && at < t.endIndex);
      if (!token) continue;
      result[word] = {
        scopes: token.scopes,
        old: pick(token.scopes, SKIP_OLD),
        now: pick(token.scopes, SKIP_NEW),
      };
    }
  });

  return result;
}

const without = await run(false);
const with_ = await run(true);

console.log("注入登记的目标 scope:", [...injections.keys()].join(", "));
console.log("text.html.vue 上要注入的:", (injections.get("text.html.vue") ?? []).join(", "));
console.log();

for (const key of ["v-if", "@click", ":class", '"ok"', '"go"', "cls", "msg"]) {
  const a = without[key];
  const b = with_[key];
  if (!a || !b) continue;

  console.log(`${key}`);
  console.log(`  注入前最后一段: ${a.scopes[a.scopes.length - 1]}`);
  console.log(`  注入后最后一段: ${b.scopes[b.scopes.length - 1]}`);
  console.log(`  旧挑法(跳 meta): ${a.old}  →  ${b.old}`);
  console.log(`  新挑法(留 meta): ${a.now}  →  ${b.now}`);
  console.log(`  ${b.old === b.now ? "→ 挑法改了没影响" : "→ ★ 挑法改了有影响"}`);
  console.log();
}
