/**
 * 显示用的格式化工具。
 *
 * ★ 单独一个文件，理由和 `pathUtils.ts` / `fuzzy.ts` / `snippetParser.ts` 一样：
 *   `<script setup>` 里不能 export 任意函数，塞在组件里就没法在别处用。
 *   而「字节数写成 MB 还是 GB」这种换算，两处各写一份迟早会不一致
 */

/** 把字节数写成给人看的样子 */
export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} 字节`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}
