/**
 * StatusLine：面板底部的弱信息行——「模型 · 耗时 · tokens」或「已取消」「已复制」等。
 * meta 字号、--fg-2（仍满足 AA，--fg-3 不用于信息文字）、等宽数字；放不下时末项省略。
 * 状态类提示（截断、思考耗尽、参数降级）用 `note`，带一个小图标，语气比错误卡片轻得多（param_unsupported 只在这里提示）。
 */
import type { LucideIcon } from 'lucide-react'
import type { ReactNode } from 'react'
import { formatDuration, formatTokens } from '../i18n/format'
import { useT } from '../i18n/useT'
import type { Usage } from '../ipc/types'
import { cx } from './cx'
import { Icon } from './Icon'

export interface StatusLineProps {
  /** 模型显示名 */
  model?: string | null
  elapsedMs?: number | null
  usage?: Usage | null
  /** 覆盖整行的状态（取消后只显示「已取消」，不显示 token） */
  state?: 'cancelled' | null
  /** 附加的弱提示（截断、缓存命中、参数降级……） */
  note?: { icon?: LucideIcon; text: ReactNode; tone?: 'default' | 'warning' } | null
  /** 行尾附加内容（例如 InlineToast「已复制」） */
  trailing?: ReactNode
  className?: string
  'data-testid'?: string
}

export function StatusLine({
  model,
  elapsedMs,
  usage,
  state = null,
  note = null,
  trailing,
  className,
  'data-testid': testId = 'status-line',
}: StatusLineProps) {
  const t = useT()
  const parts: Array<{ id: string; text: string }> = []
  if (state === 'cancelled') parts.push({ id: 'state', text: t('panel.status.cancelled') })
  else {
    if (model) parts.push({ id: 'model', text: model })
    if (elapsedMs !== null && elapsedMs !== undefined)
      parts.push({ id: 'elapsed', text: formatDuration(elapsedMs) })
    if (usage) parts.push({ id: 'tokens', text: formatTokens(usage.output_tokens) })
  }
  return (
    <div className={cx('ui-status', className)} data-testid={testId}>
      {parts.length > 0 ? (
        <span className="ui-status-parts">
          {parts.map((p) => (
            <span key={p.id} className="ui-status-part">
              {p.text}
            </span>
          ))}
        </span>
      ) : null}
      {note ? (
        <span className="ui-status-note" data-tone={note.tone ?? 'default'}>
          {note.icon ? <Icon icon={note.icon} size={12} /> : null}
          {note.text}
        </span>
      ) : null}
      {trailing}
    </div>
  )
}
