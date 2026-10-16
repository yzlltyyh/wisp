/**
 * 状态行的提示（StatusLine 的 `note`）：同一时刻只放一条，按优先级取。正文区与译文区各一条优先级链；图标、文案与
 * 语气在 NOTE_SPECS，视图只负责画出来（短句放不下时省略号截断，全文在 title 里）。
 */
import {
  BrainCircuit,
  CircleAlert,
  Info,
  Pencil,
  PencilLine,
  Zap,
  type LucideIcon,
} from 'lucide-react'
import type { MessageKey } from '../../shared/i18n/i18n'
import type { RunState } from './useRun'

export type NoteKind =
  'exhausted' | 'truncated' | 'partial' | 'edited' | 'stale' | 'cached' | 'param_degraded'

export interface NoteSpec {
  readonly icon: LucideIcon
  readonly label: MessageKey
  /** 警示色：结果不完整、需要用户留意 */
  readonly tone?: 'warning'
}

export const NOTE_SPECS = {
  // 思考耗尽、已截断：与翻译视图同一句话、同一个图标
  exhausted: { icon: BrainCircuit, label: 'panel.status.thinking_exhausted', tone: 'warning' },
  truncated: { icon: CircleAlert, label: 'panel.status.truncated', tone: 'warning' },
  // 文档解析有区域没能识别（结果不完整）：一句安静的提示，不用警示色——识别出的部分照常可用
  partial: { icon: Info, label: 'ocr.meta.partial' },
  edited: { icon: Pencil, label: 'ocr.meta.edited' },
  stale: { icon: PencilLine, label: 'ocr.translation.stale' },
  cached: { icon: Zap, label: 'panel.status.cached' },
  param_degraded: { icon: Info, label: 'panel.status.param_degraded' },
} as const satisfies Record<NoteKind, NoteSpec>

/** 思考耗尽：结束时停在输出上限、却没有任何文字（额度都花在了思考上） */
export function isExhausted(s: RunState): boolean {
  return s.phase === 'done' && s.done?.truncated === 'max_tokens' && !s.hasText
}

/**
 * 正文区：已截断 > 部分区域未能识别 > 已编辑 > 来自缓存 > 参数降级。识别进行中头部已显示「识别中…」，状态行留空
 * （不重复）；思考耗尽且没有结果时正文区的空状态已经说明原因，状态行只留模型 · 耗时（不再用琥珀色重复）
 */
export function mainNote(s: RunState, edited: boolean): NoteKind | null {
  if (s.phase === 'waiting' || s.phase === 'streaming' || isExhausted(s)) return null
  if (s.done?.truncated) return 'truncated'
  if (s.notices.includes('partial')) return 'partial'
  if (edited) return 'edited'
  if (s.done?.cached) return 'cached'
  if (s.notices.includes('param_degraded')) return 'param_degraded'
  return null
}

/** 译文区：思考耗尽 > 已截断 > 原文已修改 > 来自缓存 > 参数降级 */
export function translationNote(s: RunState, stale: boolean): NoteKind | null {
  if (isExhausted(s)) return 'exhausted'
  if (s.done?.truncated) return 'truncated'
  if (stale) return 'stale'
  if (s.done?.cached) return 'cached'
  if (s.notices.includes('param_degraded')) return 'param_degraded'
  return null
}
