/**
 * 视图之间的流交接：输入视图发起的请求由翻译视图接着显示（视图内切换）。
 *
 * 输入视图调用 `translate_start` 时把事件收进这里的缓冲；拿到 `StartAck` 之后 `switchView` 到翻译视图，翻译视图挂载时
 * 先在这里按 request_id 找——找到就同步回放缓冲并继续接收后续事件，不再经 IPC `stream_attach` 让 Rust 重放一遍
 * （否则同一个请求的每个 Delta 要经两条 Channel 各送一次）。找不到（Rust 发起的请求、Pinned）时调用方退回
 * `stream_attach`。
 *
 * 缓冲只保留最近的几个请求，被接走并结束后立即释放；接走它的视图在流结束之前就退订了（流式中被固定转给 Pinned、视图在
 * 收到 Error{cancelled} 之前卸载……）时，缓冲成为「孤儿」，终止事件一到立即释放——不会把整段流的 Delta 一直留在面板
 * WebView 里，也不随会话增长。
 */
import { api } from '../../shared/ipc'
import type { LlmError, StartAck, StreamEvent, TranslateReq } from '../../shared/ipc/types'

type Listener = (event: StreamEvent) => void

interface Entry {
  readonly events: StreamEvent[]
  readonly listeners: Set<Listener>
  done: boolean
  /** 被接走过、但在结束之前订阅者都已退订：终止事件到达时立即释放 */
  orphaned: boolean
}

/** 最多同时保留的缓冲（正常只有 0–1 个：交接完成后即释放） */
const MAX_ENTRIES = 4

const entries = new Map<string, Entry>()

function isTerminal(e: StreamEvent): boolean {
  return e.type === 'done' || e.type === 'error'
}

function register(id: string, entry: Entry): void {
  entries.set(id, entry)
  // 超出上限时丢掉最早的已结束缓冲（没人接的请求不会一直占着）
  for (const [key, e] of entries) {
    if (entries.size <= MAX_ENTRIES) break
    if (e.done || e.listeners.size === 0) entries.delete(key)
  }
}

/** 释放一条缓冲（按对象查找：登记在 request_id 之下，push 时不知道 id） */
function release(entry: Entry): void {
  for (const [key, e] of entries) {
    if (e === entry) {
      entries.delete(key)
      return
    }
  }
}

function createEntry(): { entry: Entry; push: Listener } {
  const entry: Entry = { events: [], listeners: new Set(), done: false, orphaned: false }
  const push: Listener = (event) => {
    if (entry.done) return
    entry.events.push(event)
    if (isTerminal(event)) entry.done = true
    for (const l of entry.listeners) l(event)
    // 没有人会再接它了：随终止事件一起释放
    if (entry.done && entry.orphaned && entry.listeners.size === 0) release(entry)
  }
  return { entry, push }
}

/**
 * 发起翻译请求并把事件缓冲起来，返回 StartAck（失败时抛出命令的错误）。长文本被 Rust 拦下（`ack.long_text`，
 * 没有请求、`request_id` 为空串）时不登记缓冲
 */
export async function startBuffered(req: TranslateReq): Promise<StartAck> {
  const { entry, push } = createEntry()
  const ack = await api.translateStart(req, push)
  if (!ack.long_text) register(ack.request_id, entry)
  return ack
}

let localSeq = 0

/**
 * 请求没能开始（命令本身失败）：登记一条只有 Error 的本地流，翻译视图照常接上并显示错误卡片。
 * 返回本地 id（不会与 Rust 的 request_id 冲突）
 */
export function failLocally(error: LlmError): string {
  const id = `local-${++localSeq}`
  const { entry, push } = createEntry()
  register(id, entry)
  push({ ...error, type: 'error' })
  return id
}

/** 本地 id（命令失败时登记的）：取消、Pinned 交接都不应发给 Rust */
export function isLocalRequest(id: string): boolean {
  return id.startsWith('local-')
}

/**
 * 接上缓冲中的请求：同步回放已有事件，之后的事件实时转发。没有这个请求时返回 null（调用方改用 stream_attach）。
 * 返回取消订阅函数；请求结束且没有订阅者时释放缓冲
 */
export function subscribeBuffered(id: string, listener: Listener): (() => void) | null {
  const entry = entries.get(id)
  if (!entry) return null
  entry.listeners.add(listener)
  for (const e of entry.events) listener(e)
  return () => {
    entry.listeners.delete(listener)
    // 还没结束就没人接了：标记为孤儿，终止事件到达时释放（push）。重新订阅（StrictMode）时 listeners 不为空，不释放
    if (!entry.done) {
      entry.orphaned = true
      return
    }
    // 结束且没人再接时释放。推迟到微任务：StrictMode 的「卸载 → 立即重新挂载」会在同一轮里重新订阅
    queueMicrotask(() => {
      if (entry.done && entry.listeners.size === 0 && entries.get(id) === entry) entries.delete(id)
    })
  }
}

/** 丢弃一条没人会接的缓冲（输入视图已关闭时才拿到的 StartAck） */
export function discardBuffered(id: string): void {
  entries.delete(id)
}

/** 测试用 */
export function bufferedCount(): number {
  return entries.size
}

export function clearBuffered(): void {
  entries.clear()
}
