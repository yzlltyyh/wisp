/**
 * OCR 视图用到的设置快照：`settings_get` 一次，之后随 `settings:changed` 更新（低频广播）。
 *
 * 面板窗口常驻（预热后不销毁），快照在模块级缓存，每次打开 OCR 视图不重新拉取；视图挂载前还没拉到时为 null，
 * 视图里依赖它的部分（模型列表、风格菜单、自动复制）按 null 处理（不阻塞首帧）。
 */
import { useSyncExternalStore } from 'react'
import { api, on, type SettingsSnapshot } from '../../shared/ipc'

let snapshot: SettingsSnapshot | null = null
/** 已订阅 `settings:changed`（整个页面只订阅一次） */
let subscribed = false
/** 已发出 / 已完成 `settings_get`；失败时复位，下次订阅时**只重试拉取**（不重复订阅事件） */
let fetching = false
const listeners = new Set<() => void>()

function set(next: SettingsSnapshot): void {
  snapshot = next
  for (const l of listeners) l()
}

function start(): void {
  // 先订阅再拉取：拉取途中的变更不会丢（事件晚到时以事件为准）
  if (!subscribed) {
    subscribed = true
    on('settings:changed', set).catch((e: unknown) => {
      subscribed = false
      console.warn('[ocr] settings:changed', e)
    })
  }
  if (fetching || snapshot) return
  fetching = true
  api.settingsGet().then(
    (s) => {
      if (!snapshot) set(s)
    },
    (e: unknown) => {
      fetching = false
      console.warn('[ocr] settings_get', e)
    },
  )
}

function subscribe(listener: () => void): () => void {
  start()
  listeners.add(listener)
  return () => listeners.delete(listener)
}

const get = (): SettingsSnapshot | null => snapshot

/** 当前设置快照（还没拉到时为 null） */
export function useSettingsSnapshot(): SettingsSnapshot | null {
  return useSyncExternalStore(subscribe, get, get)
}

/** 测试用：清空缓存（下次订阅时重新拉取），或直接放入一份快照 */
export function resetSettingsSnapshot(next: SettingsSnapshot | null = null): void {
  snapshot = next
  fetching = false
}
