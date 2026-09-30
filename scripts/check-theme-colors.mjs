// 查 UI 颜色键在深浅两个主题里**是不是都有**，顺便把值打出来。
//
// ★ 为什么需要它：CSS_VAR_BY_COLOR 那条原则是「只映射深浅主题都有的键」——
//   某一边缺了就会退化成兜底值，而那种错**不会报错**，
//   只是某一套主题下颜色不对（比如浅色下白字白底）。
//   靠印象判断「这个键应该两边都有」是不可靠的，必须查。
//
// 用法：node scripts/check-theme-colors.mjs

import fs from "node:fs";
import path from "node:path";

const THEMES_DIR =
  "D:/vscode/Microsoft VS Code/04c0d99f4f/resources/app/extensions/theme-defaults/themes";

/** 我们想映射的 CSS 变量 → 候选的 VS Code 键（按优先级，先试前面的） */
const CANDIDATES = [
  ["--color-hover", ["list.hoverBackground", "toolbar.hoverBackground"]],
  ["--color-selection", ["list.activeSelectionBackground", "menu.selectionBackground"]],
  ["--color-text-on-accent", ["menu.selectionForeground", "list.activeSelectionForeground"]],
  ["--color-menu-bg", ["menu.background", "editorWidget.background"]],
  ["--color-menu-border", ["menu.border", "editorWidget.border", "sideBar.border"]],
  ["--color-scrollbar-thumb", ["scrollbarSlider.background"]],
  ["--color-scrollbar-thumb-hover", ["scrollbarSlider.hoverBackground"]],
  ["--color-tab-hover-bg", ["tab.hoverBackground"]],
  ["--color-border", ["sideBar.border", "editorGroup.border"]],
  ["--color-activitybar-active-border", ["activityBar.activeBorder"]],
  ["--color-statusbar-hover", ["statusBarItem.hoverBackground"]],
];

/** 递归合并 include 链：父在前，自己被自己覆盖 */
function loadColors(file, seen = new Set()) {
  if (seen.has(file) || !fs.existsSync(file)) return {};
  seen.add(file);

  const raw = JSON.parse(fs.readFileSync(file, "utf8"));
  let colors = {};

  for (const include of raw.include ? [raw.include] : []) {
    colors = { ...colors, ...loadColors(path.resolve(path.dirname(file), include), seen) };
  }

  return { ...colors, ...(raw.colors ?? {}) };
}

const dark = loadColors(path.join(THEMES_DIR, "dark_modern.json"));
const light = loadColors(path.join(THEMES_DIR, "light_modern.json"));

console.log(`dark_modern  colors: ${Object.keys(dark).length} 条`);
console.log(`light_modern colors: ${Object.keys(light).length} 条`);
console.log();

let bothCount = 0;
let oneCount = 0;

for (const [cssVar, keys] of CANDIDATES) {
  // 找一个「两边都有」的键
  const usable = keys.find((key) => dark[key] && light[key]);

  if (usable) {
    bothCount += 1;
    console.log(`${cssVar.padEnd(30)} ← ${usable}`);
    console.log(`  dark : ${dark[usable]}`);
    console.log(`  light: ${light[usable]}`);
    continue;
  }

  oneCount += 1;
  console.log(`${cssVar.padEnd(30)} ← ✗ 没有两边都有的键`);
  for (const key of keys) {
    console.log(`  ${key.padEnd(34)} dark=${dark[key] ?? "(无)"}  light=${light[key] ?? "(无)"}`);
  }
}

console.log();
console.log(`可以映射：${bothCount} 个；映射不了、只能继续用兜底值：${oneCount} 个`);
