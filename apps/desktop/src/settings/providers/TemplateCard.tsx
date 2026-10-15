/**
 * 模板大卡片：引导「接入模型」的 4 张卡片与设置里服务列表为空时的快捷入口共用同一套视觉——
 * 头像（两字母，aria-hidden）、名称、一句说明、右侧箭头；按下有 presets.press 的缩放反馈（减少动效时没有）。
 * `FEATURED_CARDS` 给出卡片顺序与文案 key（OpenAI 一张卡片，表单里再切换 Responses / Chat）。
 */
import { ChevronRight, Sparkles } from 'lucide-react'
import { m } from 'motion/react'
import type { ReactNode } from 'react'
import { useT } from '../../shared/i18n/useT'
import type { ProviderTemplate } from '../../shared/ipc/types'
import { presets } from '../../shared/motion/presets'
import { useReducedMotionPref } from '../../shared/motion/useReducedMotionPref'
import { Icon } from '../../shared/ui'
import { FEATURED_TEMPLATES, monogram, OPENAI_TEMPLATES, templateName } from './model'

export function TemplateCard({
  title,
  detail,
  mark,
  neutral = false,
  loading = false,
  disabled = false,
  onClick,
  testId,
}: {
  title: string
  detail: string
  mark: ReactNode
  neutral?: boolean
  loading?: boolean
  disabled?: boolean
  onClick: () => void
  testId: string
}) {
  const reduced = useReducedMotionPref()
  return (
    <m.button
      type="button"
      className="st-bigcard"
      onClick={onClick}
      disabled={disabled}
      data-loading={loading || undefined}
      data-testid={testId}
      whileTap={reduced ? undefined : presets.press}
    >
      <span
        className="st-logo st-bigcard-logo"
        data-tone={neutral ? 'neutral' : 'accent'}
        aria-hidden="true"
      >
        {mark}
      </span>
      <span className="st-bigcard-text">
        <b>{title}</b>
        <span>{detail}</span>
      </span>
      <Icon icon={ChevronRight} size={14} className="st-bigcard-chevron" />
    </m.button>
  )
}

/**
 * 4 张卡片：OpenAI（Responses，表单里可切换为 Chat）、Anthropic、Gemini 与「其他服务」。
 * `busyId`：正在生成草稿的模板（显示加载态，其他卡片暂时不可点）
 */
export function FeaturedCards({
  templates,
  busyId,
  onPick,
  onOthers,
}: {
  templates: readonly ProviderTemplate[] | null
  busyId: string | null
  onPick: (tpl: ProviderTemplate) => void
  onOthers: () => void
}) {
  const t = useT()
  return (
    <div className="st-bigcards" data-testid="template-cards">
      {FEATURED_TEMPLATES.map((id) => {
        const tpl = templates?.find((x) => x.id === id)
        const title = tpl
          ? OPENAI_TEMPLATES.includes(id)
            ? t('settings.providers.template.openai')
            : templateName(tpl)
          : ''
        return (
          <TemplateCard
            key={id}
            title={title}
            detail={t(`settings.providers.card.${id}`)}
            mark={title ? monogram(title) : ''}
            loading={busyId === id}
            disabled={!tpl || busyId !== null}
            onClick={() => tpl && onPick(tpl)}
            testId={`card-${id}`}
          />
        )
      })}
      <TemplateCard
        title={t('settings.providers.card_others')}
        detail={t('settings.providers.card_others_detail')}
        mark={<Icon icon={Sparkles} size={16} />}
        neutral
        disabled={busyId !== null}
        onClick={onOthers}
        testId="card-others"
      />
    </div>
  )
}
