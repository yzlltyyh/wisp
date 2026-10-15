/**
 * 面板会话 store。只存会话、视图、载荷与 UI 开关；**流式文本不进 store**。
 *
 * - `epoch`：进入动画的轮次。每次需要「进入」的 prepare 递增，卡片随之重新挂载（从 opacity 0 / y 起步）；
 *   原位交叉淡入（`transition = crossfade` 的 prepare、视图内 `switchView`）不递增，卡片保持不动；
 * - `viewKey`：视图身份。每次换视图递增，ViewSwitch 据此交叉淡入；
 * - `width` / `nextWidth`：宽度变化要等旧内容完全透明的那一帧才生效，由 ViewSwitch 调 `commitWidth`。
 */
import { create } from 'zustand'
import type { HideReason, PanelPayload, PanelPrepare } from '../../shared/ipc/types'
import type { PanelPhase } from '../types'

export interface PanelSessionState {
  prepare: PanelPrepare | null
  payload: PanelPayload | null
  phase: PanelPhase
  focused: boolean
  /** 当前生效的卡片宽度（逻辑 px） */
  width: number
  /** 等待在透明帧生效的新宽度 */
  nextWidth: number | null
  epoch: number
  viewKey: number
  /** 正在退出时的隐藏原因（Css 帧模式退出动画播完后随 panel_hide 上报） */
  exitReason: HideReason | null
  /** 需要进入动画的新会话 */
  begin(prepare: PanelPrepare): void
  /** 面板已显示时的原位交叉淡入（`transition = crossfade`） */
  crossfade(prepare: PanelPrepare): void
  setPhase(phase: PanelPhase): void
  setFocused(focused: boolean): void
  beginExit(reason: HideReason): void
  /** 视图内切换（输入 → 翻译、剪贴板 → 翻译） */
  switchPayload(payload: PanelPayload, width?: number | null): void
  /** 透明帧：让等待中的新宽度生效 */
  commitWidth(): void
  /** 隐藏后释放会话数据（视图随之卸载） */
  clear(): void
}

export const usePanelStore = create<PanelSessionState>()((set) => ({
  prepare: null,
  payload: null,
  phase: 'hidden',
  focused: false,
  width: 0,
  nextWidth: null,
  epoch: 0,
  viewKey: 0,
  exitReason: null,
  begin: (prepare) =>
    set((s) => ({
      prepare,
      payload: prepare.payload,
      phase: 'preparing',
      focused: prepare.focused,
      width: prepare.width,
      nextWidth: null,
      epoch: s.epoch + 1,
      viewKey: s.viewKey + 1,
      exitReason: null,
    })),
  crossfade: (prepare) =>
    set((s) => ({
      prepare,
      payload: prepare.payload,
      focused: prepare.focused,
      nextWidth: prepare.width !== s.width ? prepare.width : null,
      viewKey: s.viewKey + 1,
      exitReason: null,
    })),
  setPhase: (phase) => set({ phase }),
  setFocused: (focused) => set({ focused }),
  beginExit: (reason) => set({ phase: 'exiting', exitReason: reason }),
  switchPayload: (payload, width) =>
    set((s) => ({
      payload,
      viewKey: s.viewKey + 1,
      nextWidth: width !== null && width !== undefined && width !== s.width ? width : null,
    })),
  commitWidth: () =>
    set((s) => (s.nextWidth === null ? s : { width: s.nextWidth, nextWidth: null })),
  clear: () => set({ prepare: null, payload: null, nextWidth: null, exitReason: null }),
}))
