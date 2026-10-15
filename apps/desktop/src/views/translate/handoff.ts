/**
 * 视图实例之间的会话内选择交接。TranslatePayload 只描述「显示什么」，不携带用户在本会话里临时做的
 * 选择——手动指定的目标语言、临时换的模型、原文对照是否展开（只对当前会话生效、不写入设置）。
 * 同一会话里换视图实例时（输入 → 翻译；新请求要求改宽度时经 switchView 重建翻译视图）按 request_id 交接这些选择。
 *
 * 只在同一窗口内有效；只保留最近几条（读取不删除：StrictMode 下状态初始化函数会被调用两次）。
 */
import type { Lang, ModelRef } from '../../shared/ipc/types'

export interface Handoff {
  target?: Lang | null
  model?: ModelRef | null
  showSource?: boolean
  /** 交换方向时强制显示的源语言（检测不确定时「把原文当作另一种语言」，text.ts planSwap） */
  forcedSource?: Lang | null
  /**
   * 输入视图交给翻译视图的正文区高度（输入框的高度，px）与对应的骨架行数：翻译视图在第一次流式结束前不低于它，
   * 交接时卡片不会先缩再长
   */
  body?: { readonly px: number; readonly lines: number }
}

const MAX = 4
const pending = new Map<string, Handoff>()

export function putHandoff(requestId: string, value: Handoff): void {
  pending.set(requestId, value)
  while (pending.size > MAX) {
    const oldest = pending.keys().next().value
    if (oldest === undefined) break
    pending.delete(oldest)
  }
}

/** 读取交接的选择（没有时为 null） */
export function readHandoff(requestId: string | null): Handoff | null {
  if (!requestId) return null
  return pending.get(requestId) ?? null
}

/** 请求还没发出时（输入视图 → 长文本确认）交接用的键 */
export function pendingKey(sessionId: number): string {
  return `session:${sessionId}`
}
