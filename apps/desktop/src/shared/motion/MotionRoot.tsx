/**
 * 动效根组件：`LazyMotion(domAnimation, strict)` + `MotionConfig`。不使用 layoutId / domMax。
 *
 * `reducedMotion` 取 Rust 合并后的偏好（`always` / `never`），随 `appearance:changed` 实时切换；
 * 减少动效时 Motion 会跳过 transform 动画，预设另按 `reducedPolicy` 把时长压到 ≤ 100 ms（presets.ts）。
 *
 * 每个 React 窗口（面板 / 固定窗口 / 设置 / 引导）的根都是它，所以强调色派生令牌（styles/accent.ts）也在这里安装：
 * 模块加载时按 boot 写入一次（早于首帧，不碰 IPC），挂载时再订阅 `appearance:changed`（此时传输层已就绪）。
 */
import { LazyMotion, MotionConfig, domAnimation } from 'motion/react'
import { useLayoutEffect, type ReactNode } from 'react'
import { applyCurrentAccent, installAccentTokens } from '../styles/accent'
import { spring } from './tokens'
import { useReducedMotionPref } from './useReducedMotionPref'

applyCurrentAccent()

export function MotionRoot({ children }: { children: ReactNode }) {
  useLayoutEffect(installAccentTokens, [])
  const reduced = useReducedMotionPref()
  return (
    <LazyMotion features={domAnimation} strict>
      <MotionConfig reducedMotion={reduced ? 'always' : 'never'} transition={spring.snappy}>
        {children}
      </MotionConfig>
    </LazyMotion>
  )
}
