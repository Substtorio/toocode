/**
 * 路径相关的**纯函数**。
 *
 * 单独放一个文件而不是写在 App.vue 里：
 * `<script setup>` 不能 export 任意函数，塞在组件里就没法单独测。
 * 而路径判断恰恰是最容易写错、又最值得测的一类逻辑（分隔符、大小写、前缀误判）。
 */

/**
 * target 是不是在 root 目录下（含 root 自身）。
 *
 * 用来判断「刚才另存为的那个文件要不要让它出现在工作区文件树里」——
 * 存到工作区外面的就不必刷树，刷了也不会出现在树里。
 */
export function isInside(target: string, root: string): boolean {
  if (!target || !root) return false;

  // 统一分隔符（用户从对话框拿到的路径可能是 / 也可能是 \）、
  // 去掉末尾的斜杠、转小写（Windows 路径不区分大小写）
  const normalize = (value: string) =>
    value.replace(/[\\/]+/g, "\\").replace(/\\+$/, "").toLowerCase();

  const inner = normalize(target);
  const outer = normalize(root);

  if (!inner || !outer) return false;

  // ⚠ 比之前先补一个分隔符：直接 startsWith 的话，
  //   `D:\foobar` 会被当成在 `D:\foo` 里面 —— 这是个很容易漏的边界
  return inner === outer || inner.startsWith(`${outer}\\`);
}

/**
 * 把绝对路径变成相对 `root` 的显示路径，分隔符统一成 `/`。
 *
 * 用在快速打开列表右侧那行灰字上 —— 同名文件只能靠它区分。
 * 不在 root 下面时原样返回（那种情况不该出现在列表里，但不值得为此抛错）。
 */
export function relativePath(target: string, root: string): string {
  if (!target || !root) return target;
  // ★ 这里复用 isInside 的同一套判断，而不是自己再 startsWith 一次 ——
  //   两份实现迟早会不一致，而前缀误判恰恰是最容易出错的地方
  if (!isInside(target, root)) return target;

  const normalizedRoot = root.replace(/[\\/]+/g, "\\").replace(/\\+$/, "");
  return target
    .replace(/[\\/]+/g, "\\")
    .slice(normalizedRoot.length)
    .replace(/^\\+/, "")
    .replace(/\\/g, "/");
}
