/**
 * 预览代码（`./preview`：悬停卡片 + 全文预览，非首屏 chunk）的加载与缓存。
 *
 * 不用 `React.lazy` + Suspense：lazy 包装第一次渲染时即使模块早已下载也会挂起一次，React 19 对 Suspense 的显示有
 * 300 ms 节流，第一次按 Space / 第一次悬停会空白约 300 ms。这里把解析好的模块放在模块级变量里：
 * - 视图显示后空闲时预取（`preloadPreview`），之后 `usePreviewModule()` 同步拿到组件，打开预览与列表淡出在同一次提交；
 * - 预取之前就按了 Space：发起加载，视图在模块到达之前不淡出列表（不会出现空白帧），到达后再交叉淡入。
 *
 * 面板 WebView 常驻，模块只加载一次。
 */
import { useEffect, useSyncExternalStore } from 'react'
import type * as Preview from './preview'

export type PreviewModule = typeof Preview

let loaded: PreviewModule | null = null
let pending: Promise<PreviewModule> | null = null
const listeners = new Set<() => void>()

/** 开始加载（幂等）；已加载时立即返回 */
export function preloadPreview(): Promise<PreviewModule> {
  if (loaded) return Promise.resolve(loaded)
  pending ??= import('./preview').then(
    (mod) => {
      loaded = mod
      notify()
      return mod
    },
    (error: unknown) => {
      // 加载失败（开发服务器重启等）：下次再试
      pending = null
      throw error
    },
  )
  return pending
}

/** 已加载的模块（同步） */
export function previewModule(): PreviewModule | null {
  return loaded
}

function notify(): void {
  for (const listener of listeners) listener()
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

/** 预览模块；还没加载时为 null（`want` 为 true 时顺带发起加载） */
export function usePreviewModule(want = false): PreviewModule | null {
  const mod = useSyncExternalStore(subscribe, previewModule, previewModule)
  useEffect(() => {
    if (want && !mod) preloadPreview().catch(() => undefined)
  }, [want, mod])
  return mod
}
