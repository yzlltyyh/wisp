/**
 * Overlay 用到的命令（全部命令的子集）。
 *
 * 单独成文件是为了体积（Overlay 入口 ≤ 15 KB）：Rolldown 按「模块被哪些入口引用」切共享 chunk，
 * `commands.ts`（全部 87 条命令）是一个模块，Overlay 只要引用到它，就会预加载整个命令 chunk。
 * 所以 Overlay 只从 `shared/ipc/overlay.ts` 导入（它只引用本文件、runtime / events / boot / url），
 * `commands.ts` 再原样 re-export 这里的函数，其他入口照常用 `api.overlayInit()` 等。
 *
 * `CommandMap`（commands.ts）继承本文件的 `OverlayCommandMap`，Rust 侧的 `ipc::contract_tests` 同时解析两份文件比对参数键。
 */
import { getTransport } from './runtime'
import type * as T from './types'

/** Overlay 命令 → 参数与返回值（`CommandMap` 的一部分） */
export interface OverlayCommandMap {
  overlay_init: { args: Record<string, never>; ret: T.OverlayInit | null }
  overlay_ready: { args: { session_id: number; monitor_id: string }; ret: void }
  overlay_commit: { args: { commit: T.OverlayCommit }; ret: T.OverlayCommitAck }
  overlay_done: { args: { session_id: number }; ret: void }
  overlay_cancel: { args: { session_id: number }; ret: void }
  overlay_focus: { args: { session_id: number }; ret: void }
}

type OverlayCommandName = keyof OverlayCommandMap

function call<K extends OverlayCommandName>(
  cmd: K,
  args: OverlayCommandMap[K]['args'],
): Promise<OverlayCommandMap[K]['ret']> {
  return getTransport().invoke<OverlayCommandMap[K]['ret']>(cmd, args)
}

/**
 * 拉取本 Overlay 窗口当前会话的初始化数据（无参数：Rust 按窗口 label 反查显示器）。
 * 返回 null 表示窗口空闲（预建）：先订阅 `overlay:session` 再调用，收到事件后再调一次（按 session_id 去重）
 */
export function overlayInit() {
  return call('overlay_init', {})
}
/** 首帧渲染完成 → Rust 显示 Overlay */
export function overlayReady(session_id: number, monitor_id: string) {
  return call('overlay_ready', { session_id, monitor_id })
}
/** 立即裁剪并发出请求，返回飞入目标 */
export function overlayCommit(commit: T.OverlayCommit) {
  return call('overlay_commit', { commit })
}
/** 飞入 / 淡出结束 */
export function overlayDone(session_id: number) {
  return call('overlay_done', { session_id })
}
/** Esc 取消 */
export function overlayCancel(session_id: number) {
  return call('overlay_cancel', { session_id })
}
/**
 * 多屏截图时让本窗口成为 key（指针进入本窗口、而它还不是 key 时调用；不激活应用）。
 * 会话已 commit / 已结束时为空操作。macOS 上非 key 的 WKWebView 收不到指针移动（覆盖式滚动条下跟踪区域只在
 * key 窗口生效），那里由原生层跟随光标，本命令只是补充；Windows / X11 上非前台窗口照常收到指针事件
 */
export function overlayFocus(session_id: number) {
  return call('overlay_focus', { session_id })
}
