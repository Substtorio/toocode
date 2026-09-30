/**
 * 模糊匹配（fuzzy match）—— 命令面板和快速打开的核心算法。
 *
 * 单独放一个文件的原因和 `pathUtils.ts` 一样：
 * `<script setup>` 不能 export 任意函数，塞在组件里就没法单独测。
 * 而"打分"这类逻辑**没法靠眼睛验证对错** —— 只能靠用例。
 *
 * 规则参考 VS Code 的偏好，但简化了很多：
 *   - query 的字符按顺序出现在 target 里就算命中（**不要求连续**）
 *   - 连续命中加分（"ab" 好过 "a…b"）
 *   - 命中词首加分（大写字母开头、或跟着分隔符的字符）
 *   - 越早命中越好
 */

/** 位置是不是"词的开始"：前面是分隔符，或者自己是大写而后一个不是 */
function isWordStart(text: string, index: number): boolean {
  if (index === 0) return true;

  const prev = text[index - 1];
  const current = text[index];

  // 空格、横线、下划线、斜杠、反斜杠、点 —— 这些都是常见的分隔符
  if (/[\s\-_/\\.,:]/.test(prev)) return true;

  // camelCase：小写后面跟大写，那个大写算词首
  return prev === prev.toLowerCase() && current !== current.toLowerCase();
}

/** 连续命中一次加多少分 */
const SCORE_CONSECUTIVE = 8;
/** 命中词首一次加多少分 */
const SCORE_WORD_START = 6;
/** 命中一次的基础分 */
const SCORE_BASE = 1;
/** 命中位置越靠后扣得越多（保证"靠前命中"排前面） */
const PENALTY_PER_INDEX = 0.1;

export interface FuzzyMatch {
  /** 分数越高越靠前 */
  score: number;
  /** 命中的字符下标，用来做高亮 */
  positions: number[];
}

/**
 * 匹配一次。不匹配返回 `null`。
 *
 * query 为空时返回"匹配一切"（空 positions）—— 这样调用方不用特判，
 * 空输入就是把全部条目按原顺序列出来。
 */
export function fuzzyMatch(query: string, target: string): FuzzyMatch | null {
  if (!query) return { score: 0, positions: [] };

  const lowerQuery = query.toLowerCase();
  const lowerTarget = target.toLowerCase();

  const positions: number[] = [];
  let score = 0;
  // 上一次命中的位置，用来判断"连着命中"
  let lastHit = -2;
  // 在 target 里往后找的游标
  let cursor = 0;

  for (const char of lowerQuery) {
    const found = lowerTarget.indexOf(char, cursor);
    if (found === -1) return null;

    positions.push(found);
    score += SCORE_BASE;

    if (isWordStart(target, found)) score += SCORE_WORD_START;
    // 紧挨着上一个命中 → 这是一段连续匹配，额外加分
    if (found === lastHit + 1) score += SCORE_CONSECUTIVE;

    score -= found * PENALTY_PER_INDEX;

    lastHit = found;
    cursor = found + 1;
  }

  // 短目标优先（输入 "sa" 时，名字叫 "Save" 的应该排在 "Save All Files" 前面）
  score -= target.length * 0.05;

  return { score, positions };
}

export interface FuzzyItem<T> {
  item: T;
  /** 命中的字符下标（针对用于匹配的那段文本） */
  positions: number[];
}

/**
 * 过滤 + 排序一整批条目。
 *
 * `textOf` 决定拿哪个字段来匹配（命令面板用 label，快速打开用文件名）。
 * 排序是稳定的：分数相同时保持传入顺序 —— 这样"平手"时条目不会乱跳。
 */
export function fuzzyFilter<T>(
  query: string,
  items: readonly T[],
  textOf: (item: T) => string,
): Array<FuzzyItem<T>> {
  const matched: Array<FuzzyItem<T> & { score: number; order: number }> = [];

  items.forEach((item, order) => {
    const result = fuzzyMatch(query, textOf(item));
    if (!result) return;
    matched.push({ item, positions: result.positions, score: result.score, order });
  });

  matched.sort((a, b) => (b.score === a.score ? a.order - b.order : b.score - a.score));

  return matched.map(({ item, positions }) => ({ item, positions }));
}
