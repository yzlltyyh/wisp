/**
 * 面板视图接口：视图注册表（views.ts）与 ViewProps。
 *
 * 外壳（PanelShell）负责会话、prepare 同步路径、进入 / 退出 / 交叉淡入动效与高度上报；
 * 视图只关心自己的载荷与交互，通过 props 上的回调请求外壳做窗口级的事。
 * 同一组件也被 Pinned 窗口复用（`host === 'pinned'`）。
 */
import type { ComponentType } from 'react'
import type { FrameMode, HideReason, PanelPayload, PanelView, Placement } from '../shared/ipc/types'

/** 从 PanelPayload 联合中取出某个视图的载荷 */
export type PayloadOf<V extends PanelView> = Extract<PanelPayload, { kind: V }>

/**
 * 面板会话阶段：
 * - `preparing`：收到 panel:prepare，已同步渲染（卡片 opacity 0）并测量，等待 panel_ready 返回；
 *   这一阶段视图**不得**依赖 rAF / ResizeObserver / Motion 回调 / setTimeout；
 * - `entering`：panel_ready 已返回，正在播放进入动画；
 * - `shown`：进入动画结束；
 * - `exiting`：正在播放退出动画，结束后外壳调用 panel_hide；
 * - `hidden`：窗口已隐藏（停掉所有 rAF、断开 ResizeObserver）。
 */
export type PanelPhase = 'preparing' | 'entering' | 'shown' | 'exiting' | 'hidden'

/** 视图宿主：面板（随用随关）或固定窗口（不自动关闭、可缩放） */
export type ViewHost = 'panel' | 'pinned'

export interface ViewProps<V extends PanelView = PanelView> {
  /**
   * 当前会话 id：调用 translate_start / ocr_start / panel_pin / panel_activate 等命令时传入。
   * Pinned 宿主下为 0：固定窗口没有面板会话，translate_start / ocr_start 传 `session_id: 0`，Rust 按调用方窗口
   * （`pinned-<id>`）归属请求，窗口关闭时收尾；`StartAck.width` 恒为 null
   */
  readonly sessionId: number
  /** 本视图的载荷（已按 kind 收窄） */
  readonly payload: PayloadOf<V>
  readonly phase: PanelPhase
  /** 面板当前是否持有键盘焦点（PanelShown.focused / panel:focus_changed） */
  readonly focused: boolean
  readonly host: ViewHost
  readonly frame: FrameMode
  readonly placement: Placement
  /** 卡片最大高度（逻辑 px） */
  readonly maxHeight: number
  /** 请求关闭：外壳先播放退出动画，再调用 panel_hide(reason)。Pinned 宿主下为关闭窗口 */
  requestHide(reason: HideReason): void
  /**
   * 视图内切换到另一个视图（输入 → 翻译）：120 ms 交叉淡入，宽度在内容完全透明的那一帧改（width 取 StartAck.width，
   * null 表示不变；外壳负责调用 panel_set_width）。会话不变。
   * 剪贴板条目 → 翻译**不**用它：走 `trigger_action(translate_selection, text, origin: clipboard)`，由 Rust 发起
   * crossfade 的 prepare（新会话）。
   */
  switchView(payload: PanelPayload, options?: { width?: number | null }): void
  /** 翻译 / OCR 视图在 pointerdown 时调用：让非激活面板成为 key（panel_activate） */
  activate(): void
}

export type PanelViewComponent<V extends PanelView> = ComponentType<ViewProps<V>>

export interface ViewDefinition<V extends PanelView> {
  readonly view: V
  readonly Component: PanelViewComponent<V>
  /** 初始估计高度（逻辑 px），只作兜底；真实高度以同步测量为准 */
  readonly estimatedHeight: number
  /** 显示后是否持有键盘焦点（参考值，以 PanelPrepare.focused 为准） */
  readonly takesFocus: boolean
}

export type ViewRegistry = { readonly [V in PanelView]: ViewDefinition<V> }
