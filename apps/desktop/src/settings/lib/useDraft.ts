/**
 * 文本类设置的「草稿 + 延迟提交」：输入时只改本地草稿（不重渲染整页、不连发命令），停顿后或失焦 / 回车时提交。
 * 外部值变化（热加载、其他窗口修改）且本地没有未提交的输入时，草稿跟随外部值。
 * 挂载期间登记到 writes.ts：关窗（⌘W）、窗口失焦 / 隐藏、指针离开窗口时立即提交（WebView 被销毁时卸载清理不会执行）。
 *
 * - `commit` 可以返回规范化后的值（例如名称清空后退回原名、去掉首尾空格）：与输入不同时草稿改成它——外部值没有变化时
 *   草稿不会自己跟上，输入框会一直停在规范化之前的样子；
 * - `reset(value)`：不提交，把草稿设为某个值；
 * - `discard()`：丢掉还没提交的输入（例如关掉「发送温度参数」时，滑块停顿中的值不能再把它打开）。
 */
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'
import { registerFlusher } from './writes'

/** 停止输入多久后提交（不是动效时长） */
export const COMMIT_DELAY_MS = 450

export interface Draft<T> {
  value: T
  /** 输入时调用 */
  set: (next: T) => void
  /** 立即提交（失焦、回车） */
  flush: () => void
  /** 不提交，把草稿设为 `value`（没有未提交的输入） */
  reset: (value: T) => void
  /** 丢掉未提交的输入，回到外部值 */
  discard: () => void
  /** 是否有未提交的输入 */
  dirty: boolean
}

type TimerRef = { current: ReturnType<typeof setTimeout> | null }

function cancelTimer(timer: TimerRef): void {
  if (timer.current !== null) clearTimeout(timer.current)
  timer.current = null
}

export function useDraft<T>(
  external: T,
  /** 提交；返回值（可选）是规范化后的值，与输入不同时草稿改成它 */
  commit: (value: T) => T | void,
  options: { delayMs?: number; equals?: (a: T, b: T) => boolean } = {},
): Draft<T> {
  const { delayMs = COMMIT_DELAY_MS, equals = Object.is } = options
  const [value, setValue] = useState(external)
  const [dirty, setDirty] = useState(false)
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const latest = useRef({ value, external, commit, equals, dirty })
  useLayoutEffect(() => {
    latest.current = { value, external, commit, equals, dirty }
  })

  // 外部值变化且没有未提交的输入：跟随
  const [seen, setSeen] = useState(external)
  if (!equals(seen, external)) {
    setSeen(external)
    if (!dirty) setValue(external)
  }

  const flush = useCallback(() => {
    cancelTimer(timer)
    const l = latest.current
    if (!l.dirty) return
    // 同一个 tick 里的第二次 flush（⌘W：先失焦提交一次，再统一提交一次）不能再提交一遍：在重渲染之前就记为已提交
    latest.current = { ...l, dirty: false }
    setDirty(false)
    if (l.equals(l.value, l.external)) return
    const normalized = l.commit(l.value)
    if (normalized !== undefined && !l.equals(normalized as T, l.value)) {
      latest.current = { ...latest.current, value: normalized as T }
      setValue(normalized as T)
    }
  }, [])

  const set = useCallback(
    (next: T) => {
      setValue(next)
      setDirty(true)
      latest.current = { ...latest.current, value: next, dirty: true }
      cancelTimer(timer)
      timer.current = setTimeout(flush, delayMs)
    },
    [delayMs, flush],
  )

  const reset = useCallback((next: T) => {
    cancelTimer(timer)
    latest.current = { ...latest.current, value: next, dirty: false }
    setValue(next)
    setDirty(false)
  }, [])

  const discard = useCallback(() => {
    cancelTimer(timer)
    const l = latest.current
    latest.current = { ...l, value: l.external, dirty: false }
    setValue(l.external)
    setDirty(false)
  }, [])

  // 卸载时把未提交的输入写出去（切换分组、关闭编辑器）；挂载期间可被统一提交（关窗前）
  useEffect(() => {
    const unregister = registerFlusher(flush)
    return () => {
      unregister()
      flush()
    }
  }, [flush])

  return { value, set, flush, reset, discard, dirty }
}
