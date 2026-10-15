/**
 * 减少动效偏好。
 *
 * 三档（跟随系统 / 始终减少 / 始终完整）由 Rust 合并成最终的布尔值：创建窗口时在 `boot.reduce_motion`，
 * 之后随 `appearance:changed` 更新（`<html data-reduced-motion>` 由 ipc/boot 同步写入，CSS 侧据此生效）。
 */
import { getAppearance, useAppearance } from '../hooks/useAppearance'

/** 当前是否减少动效（响应式） */
export function useReducedMotionPref(): boolean {
  return useAppearance().reduceMotion
}

/** 非 React 代码读取当前值（例如命令式动画、Writer） */
export function prefersReducedMotion(): boolean {
  return getAppearance().reduceMotion
}
