/**
 * 风格的保存（`style_save` 写整个对象）：同一个风格的写入**串行**，并且总以「最近一次服务器返回的风格 + 还没写出去的字段」
 * 为准，不会出现「textarea 失焦提交模板，返回之前又点了开关，用旧模板为底再发一次把模板改回去」的丢失更新。
 *
 * - `saveStyleFields(id, patch)`：本地立即生效（乐观），排进这个风格的队列；在途时只合并字段，返回后再发下一份；
 * - 服务器返回的风格列表经 `applyServerStyles` 写回 store：还在排队的字段重新叠在上面（界面不回跳）；
 * - 失败：提示，没有后续排队时重新取一次风格列表回到服务器状态。
 * 在途的写入登记到 writes.ts（关窗前等它们返回）。
 */
import { api } from '../../shared/ipc'
import type { Style } from '../../shared/ipc/types'
import { setStyles, useSettingsStore } from '../store'
import { trackWrite } from './writes'

interface Slot {
  /** 还没发出的字段 */
  queued: Partial<Style> | null
  /** 在途的那一轮（串行） */
  running: Promise<void> | null
}

const slots = new Map<string, Slot>()

function currentStyles(): Style[] {
  return useSettingsStore.getState().snap?.styles ?? []
}

/** 服务器返回的风格列表写回 store，还在排队的字段叠在上面 */
export function applyServerStyles(list: readonly Style[]): void {
  setStyles(
    list.map((s) => {
      const queued = slots.get(s.id)?.queued
      return queued ? { ...s, ...queued } : s
    }),
  )
}

/** 改风格的若干字段；返回这一轮（含之后合并进来的）写完的 Promise */
export function saveStyleFields(
  id: string,
  patch: Partial<Style>,
  onError: (error: unknown) => void,
): Promise<void> {
  // 乐观更新：开关、分段、下拉立即反映
  setStyles(currentStyles().map((s) => (s.id === id ? { ...s, ...patch } : s)))
  let slot = slots.get(id)
  if (!slot) {
    slot = { queued: null, running: null }
    slots.set(id, slot)
  }
  slot.queued = { ...slot.queued, ...patch }
  if (!slot.running) slot.running = trackWrite(pump(id, slot, onError))
  return slot.running
}

async function pump(id: string, slot: Slot, onError: (error: unknown) => void): Promise<void> {
  try {
    while (slot.queued) {
      const patch = slot.queued
      slot.queued = null
      // 底稿取 store 里的当前风格：它就是「最近一次服务器返回的 + 之后的乐观改动」
      const base = currentStyles().find((s) => s.id === id)
      if (!base) return
      try {
        // oxlint-disable-next-line no-await-in-loop -- 有意串行：同一个风格一次只有一份在途
        applyServerStyles(await api.styleSave({ ...base, ...patch }))
      } catch (error) {
        onError(error)
        if (!slot.queued) {
          // oxlint-disable-next-line no-await-in-loop -- 失败后回到服务器状态
          const fresh = await api.stylesList().catch(() => null)
          if (fresh) applyServerStyles(fresh)
        }
      }
    }
  } finally {
    slot.running = null
    if (!slot.queued) slots.delete(id)
  }
}

/** 测试用 */
export function resetStyleSaves(): void {
  slots.clear()
}
