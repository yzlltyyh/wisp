/**
 * mock 的内存事件总线。
 * 投递在宏任务里进行（与 Tauri 经 IPC 投递一致：emit 返回时监听者尚未执行），载荷逐个结构化克隆。
 */
import type { Unlisten } from '../transport'
import type { EventMap, EventName } from '../types'

type AnyListener = (payload: unknown) => void
type Hook = (event: EventName, payload: unknown) => void

export class MockBus {
  private readonly listeners = new Map<EventName, Set<AnyListener>>()
  /** 还没有监听者时排队的事件（只用于场景 setup 早于挂载的情况） */
  private readonly waiting = new Map<EventName, unknown[]>()
  private readonly hooks: Hook[] = []
  private inFlight = 0

  listen<K extends EventName>(event: K, cb: (payload: EventMap[K]) => void): Unlisten {
    let set = this.listeners.get(event)
    if (!set) this.listeners.set(event, (set = new Set()))
    const listener = cb as AnyListener
    set.add(listener)
    const queued = this.waiting.get(event)
    if (queued) {
      this.waiting.delete(event)
      for (const payload of queued) this.emit(event, payload as EventMap[K])
    }
    return () => {
      set.delete(listener)
    }
  }

  /** 立即（下一个宏任务）投递；没有监听者时丢弃（与真实行为一致） */
  emit<K extends EventName>(event: K, payload: EventMap[K]): void {
    for (const hook of this.hooks) hook(event, payload)
    const set = this.listeners.get(event)
    if (!set || set.size === 0) return
    this.inFlight++
    setTimeout(() => {
      this.inFlight--
      // 复制一份：监听者可能在回调里取消订阅
      for (const cb of Array.from(set)) cb(structuredClone(payload))
    }, 0)
  }

  /** 有监听者就立即投递，否则排队到第一个监听者注册时 */
  emitWhenListened<K extends EventName>(event: K, payload: EventMap[K]): void {
    if (this.listeners.get(event)?.size) {
      this.emit(event, payload)
      return
    }
    const queue = this.waiting.get(event) ?? []
    queue.push(payload)
    this.waiting.set(event, queue)
  }

  /** 在投递前观察事件（模拟窗口用它拦截 panel:prepare） */
  addHook(hook: Hook): void {
    this.hooks.push(hook)
  }

  hasListener(event: EventName): boolean {
    return (this.listeners.get(event)?.size ?? 0) > 0
  }

  /** 尚未投递的事件数（含排队中的） */
  get pending(): number {
    let queued = 0
    for (const q of this.waiting.values()) queued += q.length
    return this.inFlight + queued
  }
}
