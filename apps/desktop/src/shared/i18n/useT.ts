/**
 * React 中取翻译函数。语言或文案表变化时（`appearance:changed` / 新命名空间注册）组件重渲染，
 * 返回的函数身份随之变化（memo 子组件因此能感知语言切换）。确保基础命名空间已注册。
 */
import { useMemo, useSyncExternalStore } from 'react'
import { localeSnapshot, subscribeLocale, t, tKey, type Locale } from './i18n'
import { loadCoreMessages } from './messages'

loadCoreMessages()

export type TFunction = typeof t

/** 为某个语言快照生成新的函数身份（快照只用来区分身份，查找时总是读当前语言） */
function bindT(_snapshot: string): TFunction {
  return (key, params) => t(key, params)
}

function bindTKey(_snapshot: string): typeof tKey {
  return ((key: string, params?: Parameters<typeof tKey>[1], fallback?: string) =>
    fallback === undefined ? tKey(key, params) : tKey(key, params, fallback)) as typeof tKey
}

/** 取 `t`（类型化 key） */
export function useT(): TFunction {
  const snapshot = useSyncExternalStore(subscribeLocale, localeSnapshot, localeSnapshot)
  // 语言切换后返回新的函数身份
  return useMemo(() => bindT(snapshot), [snapshot])
}

/** 取 `tKey`（运行期 key，例如 Rust 下发的 hint / reason_key） */
export function useTKey(): typeof tKey {
  const snapshot = useSyncExternalStore(subscribeLocale, localeSnapshot, localeSnapshot)
  return useMemo(() => bindTKey(snapshot), [snapshot])
}

/** 当前语言（响应式） */
export function useLocale(): Locale {
  const snapshot = useSyncExternalStore(subscribeLocale, localeSnapshot, localeSnapshot)
  return snapshot.slice(0, snapshot.indexOf('#')) as Locale
}
