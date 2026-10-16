/**
 * 结构化结果预览（`./structured/Preview`：表格与公式的渲染，非首屏 chunk）的加载与缓存，与剪贴板的 previewModule 同一做法：
 * 不用 `React.lazy` + Suspense（第一次渲染会挂起一次，React 19 对 Suspense 的显示有 300 ms 节流），而是把解析好的模块放在
 * 模块级变量里——结构化模式开始识别时就预取，结果出来时通常已经到了，预览与可编辑文本在同一次提交里切换；还没到时先显示
 * 源码，模块到达后再换成预览。面板 WebView 常驻，模块只加载一次。
 */
import { useEffect, useSyncExternalStore } from 'react'
import type * as Structured from './structured/Preview'

export type StructuredModule = typeof Structured

let loaded: StructuredModule | null = null
let pending: Promise<StructuredModule> | null = null
const listeners = new Set<() => void>()

/** 开始加载（幂等） */
export function preloadStructured(): Promise<StructuredModule> {
  if (loaded) return Promise.resolve(loaded)
  pending ??= import('./structured/Preview').then(
    (mod) => {
      loaded = mod
      for (const l of listeners) l()
      return mod
    },
    (error: unknown) => {
      // 加载失败（开发服务器重启等）：下次再试，期间照常显示源码
      pending = null
      throw error
    },
  )
  return pending
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

const get = (): StructuredModule | null => loaded

/** 预览模块；还没加载时为 null（`want` 为 true 时顺带发起加载） */
export function useStructuredModule(want: boolean): StructuredModule | null {
  const mod = useSyncExternalStore(subscribe, get, get)
  useEffect(() => {
    if (want && !mod) preloadStructured().catch(() => undefined)
  }, [want, mod])
  return mod
}
