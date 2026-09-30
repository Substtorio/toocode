// 待审的文件改动（diff 预览的"数据层"）
//
// ★★ 为什么 Agent 改文件必须走这一层，而不是直接调 `write_file`：
//   一个跑偏的模型加上写权限，能一口气把整个项目改乱 ——
//   而用户连它改了什么都不知道。所以写操作**先进这里**，
//   等用户看过 diff 说「保留」才真的落盘。
//   这是**安全底线**，不是锦上添花
//
// ★ 为什么用模块级的 ref、而不是放在某个组件里：
//   「登记改动」发生在 ChatPanel（工具执行时），
//   「展示 diff」发生在编辑器区域（那块地方才够宽）—— 跨组件了，
//   放谁那儿都得靠 provide/inject 绕一圈。放模块里最直接
//
// ★ 为什么不自己写 diff 算法：
//   Monaco 自带 `createDiffEditor`，而且是**并排**的那种（和 VS Code 一样）。
//   我们本来就在用 Monaco，直接用它比引一个 diff 库划算得多

import { ref } from "vue";
import { invoke } from "@tauri-apps/api/core";

export interface PendingChange {
  id: string;
  /** 文件的绝对路径 */
  path: string;
  /** 磁盘上的原内容 */
  original: string;
  /** 模型想改成的内容 */
  updated: string;
}

const changes = ref<PendingChange[]>([]);
let seq = 0;

/**
 * 正在审阅哪一条（`null` = 没在审）。
 *
 * ★ 做成模块级的，是为了让「点开 diff」和「展示 diff」不用经过 emit ——
 *   按钮在 ChatPanel，浮层在编辑器区域，中间隔着好几层
 */
const reviewing = ref<string | null>(null);

/** 所有待审改动。模板里直接用它 */
export function pendingChanges() {
  return changes;
}

/** 当前在审阅哪一条 */
export function reviewingChange() {
  return reviewing;
}

/** 打开 / 关闭 diff 浮层 */
export function reviewChange(id: string | null): void {
  reviewing.value = id;
}

/**
 * 写完盘之后的回调。
 *
 * ★ 为什么需要它：`write_file` 动的是**磁盘**，而编辑器里可能正开着这个文件 ——
 *   那边那份内容就过期了。如果不同步，用户下一句话就是「咦我保存一下，
 *   Topilot 的改动怎么没了」—— 他刚刚把新内容盖回旧的了。
 *
 * ★ 为什么用回调而不是让这里直接 import Monaco：
 *   这一层只该管「待审改动」，不该知道编辑器长什么样
 */
let onWritten: ((path: string) => void) | null = null;

export function setWriteListener(listener: (path: string) => void): void {
  onWritten = listener;
}

/**
 * 登记一个待审改动（**不写盘**）。
 *
 * ⚠ 同一个文件重复提议时**替换**而不是追加 ——
 *   模型完全可能在一轮里对同一个文件改两次，
 *   那样列表里会出现两条指向同一文件的改动，用户审完第一条第二条就失效了
 */
export function proposeChange(path: string, original: string, updated: string): PendingChange {
  const change: PendingChange = { id: `change-${++seq}`, path, original, updated };

  const existing = changes.value.findIndex((item) => item.path === path);
  if (existing >= 0) {
    // 保留原来的 id，这样「正在审阅的是哪一条」不会因为替换而错位
    change.id = changes.value[existing].id;
    changes.value.splice(existing, 1, change);
  } else {
    changes.value.push(change);
  }

  return change;
}

/** 保留这一条 —— **这才是真正落盘的那一步** */
export async function acceptChange(id: string): Promise<void> {
  const index = changes.value.findIndex((item) => item.id === id);
  if (index < 0) return;

  const change = changes.value[index];
  // 先写盘、成功了再从列表里拿掉 ——
  // 反过来的话，写失败时用户以为已经改了，实际没有
  await invoke("write_file", { path: change.path, content: change.updated });
  changes.value.splice(index, 1);

  // 通知外面「这个文件变了」—— 编辑器里开着的话得跟着更新（见 setWriteListener）
  onWritten?.(change.path);

  // 这条没了，浮层也就没东西可显示了
  if (reviewing.value === id) reviewing.value = null;
}

/** 撤销这一条。什么都没发生过 —— 磁盘上还是原文 */
export function rejectChange(id: string): void {
  const index = changes.value.findIndex((item) => item.id === id);
  if (index >= 0) changes.value.splice(index, 1);
  if (reviewing.value === id) reviewing.value = null;
}

/** 全部保留 */
export async function acceptAll(): Promise<void> {
  // 按原顺序逐个写。★ 不并行：都是写磁盘，而且中途失败时要能说清是哪个
  for (const change of [...changes.value]) {
    await acceptChange(change.id);
  }
}

/** 全部撤销 */
export function rejectAll(): void {
  changes.value = [];
  reviewing.value = null;
}
