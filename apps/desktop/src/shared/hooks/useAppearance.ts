/**
 * 外观状态（主题 / 减少动效 / 字号 / 界面语言 / 平台）的响应式快照。
 *
 * `boot`（shared/ipc/boot.ts）是窗口创建时的值，`appearance:changed` 时由 `applyAppearance` 原地改写，但它不是响应式的。
 * 这里维护一个外部 store：第一次被订阅时监听 `appearance:changed`（全窗口只有一个监听），事件到达后更新快照并通知
 * React 重渲染（`useSyncExternalStore`）。直接从事件载荷取值，不依赖 `applyAppearance` 与本监听的执行顺序。
 *
 * 只依赖 `ipc/boot` 与 `ipc/events`（不经 `shared/ipc` 桶），Overlay 等轻量入口也可以使用。
 */
import { useSyncExternalStore } from 'react'
import { boot } from '../ipc/boot'
import { on } from '../ipc/events'
import type { AppearanceState, OsKind, ResolvedMaterial, ResolvedTheme } from '../ipc/types'

export interface AppearanceSnapshot {
  readonly theme: ResolvedTheme
  readonly accent: string
  readonly reduceMotion: boolean
  readonly reduceTransparency: boolean
  readonly material: ResolvedMaterial
  /** 正文字号（逻辑 px，12–18） */
  readonly fontSize: number
  readonly os: OsKind
  readonly uiLanguage: 'zh-CN' | 'en-US'
}

/** 设置「字号」的默认值：等于 tokens.css 的 `--text-body`，此时不写内联变量 */
const DEFAULT_FONT_SIZE = 14

let snapshot: AppearanceSnapshot = fromBoot()
const listeners = new Set<() => void>()
let subscribed = false

applyFontSize(snapshot.fontSize)

function fromBoot(): AppearanceSnapshot {
  return {
    theme: boot.theme,
    accent: boot.accent,
    reduceMotion: boot.reduce_motion,
    reduceTransparency: boot.reduce_transparency,
    material: boot.material,
    fontSize: boot.font_size,
    os: boot.os,
    uiLanguage: boot.ui_language,
  }
}

function fromEvent(a: AppearanceState): AppearanceSnapshot {
  return {
    theme: a.theme,
    accent: a.accent,
    reduceMotion: a.reduce_motion,
    reduceTransparency: a.reduce_transparency,
    material: a.material,
    fontSize: a.font_size,
    os: a.os,
    uiLanguage: a.ui_language,
  }
}

/** 正文字号写到 `<html>` 的 `--text-body`（只影响正文；UI 字号与间距不变） */
function applyFontSize(px: number): void {
  if (typeof document === 'undefined') return
  const root = document.documentElement.style
  if (Number.isFinite(px) && px !== DEFAULT_FONT_SIZE) root.setProperty('--text-body', `${px}px`)
  else root.removeProperty('--text-body')
}

function ensureSubscribed(): void {
  if (subscribed) return
  subscribed = true
  void on('appearance:changed', (a) => setAppearance(fromEvent(a)))
}

/** 写入新快照并通知订阅者（事件处理与测试使用） */
export function setAppearance(next: AppearanceSnapshot): void {
  if (sameAppearance(snapshot, next)) return
  const fontChanged = next.fontSize !== snapshot.fontSize
  snapshot = next
  if (fontChanged) applyFontSize(next.fontSize)
  for (const l of listeners) l()
}

function sameAppearance(a: AppearanceSnapshot, b: AppearanceSnapshot): boolean {
  return (Object.keys(a) as Array<keyof AppearanceSnapshot>).every((k) => a[k] === b[k])
}

export function subscribeAppearance(listener: () => void): () => void {
  ensureSubscribed()
  listeners.add(listener)
  return () => listeners.delete(listener)
}

export function getAppearance(): AppearanceSnapshot {
  return snapshot
}

/** 当前外观（响应 `appearance:changed`） */
export function useAppearance(): AppearanceSnapshot {
  return useSyncExternalStore(subscribeAppearance, getAppearance, getAppearance)
}
