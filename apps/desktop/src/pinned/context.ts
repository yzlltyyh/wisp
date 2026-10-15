/**
 * Pinned 宿主上下文：PinnedApp 把固定窗口的状态与窗口级能力交给它承载的视图（TranslateView / OcrView）。
 *
 * ViewProps 只有 `host = 'pinned'` 与 `sessionId = 0`，表达不了「从哪里接着显示」：PinState 里的已有结果
 * （result_text / status_line / show_source / ocr_text…）、仍在进行的请求（request_id）、首帧就绪的回调与窗口控件
 * 都经这里传递。视图在 `host === 'pinned'` 时读取：
 *
 * - `state`：pinned_init 返回的 PinState；
 * - `ready()`：首帧内容已经**同步**渲染完（回放完毕或静态结果已写入）时调用一次，PinnedApp 随即测量并
 *   `pinned_ready`（Rust 在命令内同一帧显示固定窗口、隐藏面板）。视图没有用 `usePinnedReadyHold()` 声明
 *   自己负责时，PinnedApp 在挂载后自动调用（适合首帧不需要等待的视图）；
 * - `usePinnedChrome()`：视图把窗口控件（置顶开关、✕）并进自己的头部时调用，PinnedApp 就不再另画一条标题栏；
 *   视图的头部要带 `data-tauri-drag-region`（整条可拖动）。OcrView 可照此并入。
 */
import { createContext, useContext, useLayoutEffect, type ReactNode } from 'react'
import type { PinState } from '../shared/ipc/types'

export interface PinnedHost {
  readonly pinnedId: string
  readonly state: PinState
  /** 视图首帧已同步渲染完，可以测量并 pinned_ready（只有第一次调用生效） */
  ready(): void
  /**
   * 视图声明「首帧就绪由我负责」（需要先回放流时）：在布局阶段调用（`usePinnedReadyHold`），PinnedApp 就不会在挂载后
   * 自动 ready，改等视图调用 `ready()`
   */
  holdReady(): void
  /** 视图已把窗口控件并进自己的头部 */
  claimChrome(): void
}

/** 宿主（窗口生命周期内身份不变：视图的挂载 effect 依赖它） */
export const PinnedContext = createContext<PinnedHost | null>(null)

/** 窗口控件（置顶开关、✕）：随置顶状态变化，单独一个上下文，不牵动宿主的身份 */
export const PinnedChromeContext = createContext<ReactNode>(null)

/** 所在的固定窗口（面板里为 null） */
export function usePinnedHost(): PinnedHost | null {
  return useContext(PinnedContext)
}

/**
 * 取窗口控件并声明「头部由视图负责」。在布局阶段声明：PinnedApp 的同步首帧（flushSync）里就能去掉自己的标题栏，
 * 测量到的高度与显示出来的一致
 */
export function usePinnedChrome(): ReactNode | null {
  const host = useContext(PinnedContext)
  const chrome = useContext(PinnedChromeContext)
  useLayoutEffect(() => host?.claimChrome(), [host])
  return host ? chrome : null
}

/** 视图声明自己负责调用 `ready()`（例如要等 stream_attach 回放完）。在布局阶段声明，早于 PinnedApp 的挂载 effect */
export function usePinnedReadyHold(): void {
  const host = useContext(PinnedContext)
  useLayoutEffect(() => host?.holdReady(), [host])
}
