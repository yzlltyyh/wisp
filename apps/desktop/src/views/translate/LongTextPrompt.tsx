/**
 * 长文本确认卡片：原文超过设置的阈值（默认 3000 字）时先确认再翻译。
 * 只显示字数、估算 token 与（有最近输出速度时）预计耗时，不显示金额；超过 8000 字额外提示「译文可能被截断」，但不拒绝
 * （数字与字数一样按界面语言加千分位）。
 * 出现动效 presets.cardIn；⌘↩ 继续、Esc 关闭（外壳）。
 */
import { FileText, TriangleAlert } from 'lucide-react'
import { m } from 'motion/react'
import { formatChars, formatDuration, formatNumber } from '../../shared/i18n/format'
import { useT } from '../../shared/i18n/useT'
import type { LongTextInfo } from '../../shared/ipc/types'
import { useKeyScope } from '../../shared/keys/useKeyScope'
import { usePreset } from '../../shared/motion/presets'
import { Button, Icon, Kbd } from '../../shared/ui'
import { LONG_TEXT_HARD_CHARS } from './text'
import { loadTranslateMessages } from './i18n'

loadTranslateMessages()

const SECOND_MS = 1000

export interface LongTextPromptProps {
  info: LongTextInfo
  onContinue: () => void
  onCancel: () => void
}

export function LongTextPrompt({ info, onContinue, onCancel }: LongTextPromptProps) {
  const t = useT()
  const enter = usePreset('cardIn')
  useKeyScope('translate-long-text', [{ combo: 'Mod+Enter', allowInInput: true, run: onContinue }])
  const hard = info.chars > LONG_TEXT_HARD_CHARS
  const detail = t('translate.long_text.detail', {
    chars: formatChars(info.chars),
    tokens: formatNumber(info.est_tokens),
  })
  const eta =
    info.est_seconds !== null && info.est_seconds > 0
      ? t('translate.long_text.eta', { time: formatDuration(info.est_seconds * SECOND_MS) })
      : null
  return (
    <m.section
      className="tv-longtext"
      aria-label={t('translate.long_text.title')}
      data-testid="tv-long-text"
      data-hard={hard || undefined}
      {...enter}
    >
      <div className="tv-longtext-head">
        <span className="tv-longtext-icon" aria-hidden="true">
          <Icon icon={FileText} size={16} />
        </span>
        <div className="tv-longtext-text">
          <p className="tv-longtext-title">{t('translate.long_text.title')}</p>
          <p className="tv-longtext-detail">
            {detail}
            {eta ? ` · ${eta}` : null}
          </p>
          {hard ? (
            <p className="tv-longtext-warning" data-testid="tv-long-text-warning">
              <Icon icon={TriangleAlert} size={12} />
              {t('translate.long_text.truncate_warning', {
                limit: formatChars(LONG_TEXT_HARD_CHARS),
              })}
            </p>
          ) : null}
        </div>
      </div>
      <div className="tv-longtext-actions">
        <Button
          variant="primary"
          size="sm"
          onClick={onContinue}
          data-testid="tv-long-text-continue"
        >
          {t('translate.long_text.continue')}
          <Kbd combo="Mod+Enter" variant="inline" />
        </Button>
        <Button variant="secondary" size="sm" onClick={onCancel} data-testid="tv-long-text-cancel">
          {t('translate.long_text.cancel')}
        </Button>
      </div>
    </m.section>
  )
}
