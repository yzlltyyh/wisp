/**
 * 离线模型的快照（`LocalModelsSnapshot`）：第一次有组件要用时才 `offline_models_get`（Rust 这时才读模型库存）并订阅
 * `offline_models:changed`（下载进度、安装 / 删除、加载 / 释放；Rust 节流后广播给全部窗口），之后在模块级缓存。
 *
 * 只有离线模型分组、引导的离线入口与面板的「离线模型未下载」卡片会用到：没有用到的窗口不订阅、不读盘。
 * 订阅只是事件监听，没有任何定时器或轮询。
 */
import { useEffect, useSyncExternalStore } from 'react'
import { api, on } from '../ipc'
import type { LocalModelId, LocalModelsSnapshot } from '../ipc/types'

let snapshot: LocalModelsSnapshot | null = null
let error: unknown = null
let subscribed = false
let fetching = false
const listeners = new Set<() => void>()

function notify(): void {
  for (const l of listeners) l()
}

/**
 * 面板卡片在就地下载完成后已经自动重新发起过请求的模型。每一轮下载最多自动重试一次：模型装好了、请求却仍被判为
 * 未就绪时（例如推理组件随后又缺了），新挂载的卡片不会再自动重试，避免「完成 → 重试 → 未就绪 → 重试」的循环。
 * 模型重新进入下载（不再是「已安装」）时清除，下一轮下载完成照常自动重试
 */
const retried = new Set<LocalModelId>()

/** 卡片要自动重试一次：这一轮还没有重试过时返回 true 并记下 */
export function claimAutoRetry(id: LocalModelId): boolean {
  if (retried.has(id)) return false
  retried.add(id)
  return true
}

/** 换上一份新快照（事件、命令的返回值） */
export function setOfflineSnapshot(next: LocalModelsSnapshot): void {
  snapshot = next
  error = null
  for (const m of next.models) if (m.state.state !== 'installed') retried.delete(m.id)
  notify()
}

function start(): void {
  // 先订阅再拉取：拉取途中的变化不会丢（事件晚到时以事件为准）
  if (!subscribed) {
    subscribed = true
    on('offline_models:changed', setOfflineSnapshot).catch((e: unknown) => {
      subscribed = false
      console.warn('[offline] offline_models:changed', e)
    })
  }
  if (fetching || snapshot) return
  fetching = true
  api.offlineModelsGet().then(
    (s) => {
      if (!snapshot) setOfflineSnapshot(s)
    },
    (e: unknown) => {
      fetching = false
      error = e
      notify()
      console.warn('[offline] offline_models_get', e)
    },
  )
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

const get = (): LocalModelsSnapshot | null => snapshot
const getError = (): unknown => error

/**
 * 离线模型的快照（还没读到时为 null）。`enabled = false` 时不发起读取（例如面板里只有离线模型未就绪时才需要）
 */
export function useOfflineModels(enabled = true): LocalModelsSnapshot | null {
  const snap = useSyncExternalStore(subscribe, get, get)
  useEffect(() => {
    if (enabled) start()
  }, [enabled])
  return enabled ? snap : null
}

/** 读取失败的原因（设置页显示「重试」）；没有失败时为 null */
export function useOfflineError(): unknown {
  return useSyncExternalStore(subscribe, getError, getError)
}

/** 读取失败后重试 */
export function retryOfflineModels(): void {
  error = null
  notify()
  start()
}

/** 测试用：清空缓存（下次使用时重新读取、重新订阅），或直接放入一份快照 */
export function resetOfflineModels(next: LocalModelsSnapshot | null = null): void {
  snapshot = next
  error = null
  retried.clear()
  subscribed = false
  fetching = false
  notify()
}
