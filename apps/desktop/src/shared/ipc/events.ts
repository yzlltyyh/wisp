/**
 * 事件订阅。事件只用于低频广播；流式输出走 Channel（见 `Transport.openStream`）。
 */
import { getTransport } from './runtime'
import type { Unlisten } from './transport'
import type { EventMap, EventName } from './types'

export type EventListener<K extends EventName> = (payload: EventMap[K]) => void

/** 订阅发给本窗口（或广播）的事件；返回的 Promise resolve 为取消函数 */
export function on<K extends EventName>(event: K, cb: EventListener<K>): Promise<Unlisten> {
  return getTransport().listen(event, cb)
}

/**
 * 同时订阅多个事件；返回的取消函数可以在订阅完成之前调用（组件卸载早于订阅完成时不泄漏）。
 * 适合在 useEffect 里使用：`useEffect(() => onMany({ 'panel:prepare': … }), [])`。
 */
export function onMany(handlers: { [K in EventName]?: EventListener<K> }): Unlisten {
  let disposed = false
  const unlisteners: Unlisten[] = []
  for (const name of Object.keys(handlers) as EventName[]) {
    const cb = handlers[name] as EventListener<typeof name> | undefined
    if (!cb) continue
    void on(name, cb).then((un) => {
      if (disposed) un()
      else unlisteners.push(un)
    })
  }
  return () => {
    disposed = true
    for (const un of unlisteners.splice(0)) un()
  }
}

/**
 * 全部事件名（`check-contract` 与 mock 使用）。顺序与 Rust 的 `wisp_types::event::ALL` 一致，
 * wisp-types 的 golden 测试（`event_names_match_frontend`）逐项比对，任何一边增删都会失败。
 */
export const EVENT_NAMES = [
  'panel:prepare',
  'panel:hide_request',
  'panel:focus_changed',
  'panel:latency',
  'clipboard:changed',
  'settings:changed',
  'appearance:changed',
  'permissions:changed',
  'hotkeys:changed',
  'hotkey:test_received',
  'update:status',
  'onboarding:hotkey',
  'overlay:session',
  'window:navigate',
  'window:close_requested',
  'overlay:end',
  'ocr_history:changed',
  'sync:status',
  'glossary:changed',
  'clipboard:queue',
] as const satisfies readonly EventName[]

type AssertNever<Never extends never> = Never
export type MissingEventNames = AssertNever<Exclude<EventName, (typeof EVENT_NAMES)[number]>>
