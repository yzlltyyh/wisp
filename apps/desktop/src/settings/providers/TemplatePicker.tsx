/**
 * 选择服务模板（16 个模板按 Rust 给出的顺序）。设置里按「官方 / 其他兼容服务 / 本机」分组展示全部模板；
 * 有区域变体的模板在编辑器里选区域。引导页另用 4 张大卡片，见 onboarding。
 */
import { ArrowLeft, ChevronRight } from 'lucide-react'
import { useT } from '../../shared/i18n/useT'
import type { ProviderTemplate, TemplateGroup } from '../../shared/ipc/types'
import { useKeyScope } from '../../shared/keys/useKeyScope'
import { Icon, IconButton, Section, Skeleton } from '../../shared/ui'
import { useReselect, useSubviewTitle } from '../context'
import { hostOf, monogram, OPENAI_TEMPLATES, templateName } from './model'

const GROUPS: ReadonlyArray<{
  id: 'official' | 'other' | 'local'
  groups: readonly TemplateGroup[]
}> = [
  { id: 'official', groups: ['openai', 'anthropic', 'gemini'] },
  { id: 'other', groups: ['other'] },
  { id: 'local', groups: ['local'] },
]

/** host 只在「.」之后可以换行（最多两行，不在单词中间断开） */
function breakableHost(host: string) {
  const parts = host.split('.')
  return parts.map((part, i) => (
    // oxlint-disable-next-line react/no-array-index-key -- 片段按位置排列，没有稳定 id
    <span key={i}>
      {i < parts.length - 1 ? `${part}.` : part}
      {i < parts.length - 1 ? <wbr /> : null}
    </span>
  ))
}

export function TemplateTile({
  template,
  onPick,
  busy = false,
}: {
  template: ProviderTemplate
  onPick: (t: ProviderTemplate) => void
  busy?: boolean
}) {
  const t = useT()
  const name = templateName(template)
  const host = hostOf(template.provider.base_url)
  // OpenAI 的两个模板 host 相同：副标题写协议（并标出推荐的那个），一眼看出区别
  const sub = OPENAI_TEMPLATES.includes(template.id)
    ? t(`settings.providers.template_sub.${template.id as 'openai'}`)
    : null
  return (
    <button
      type="button"
      className="st-tile"
      onClick={() => onPick(template)}
      disabled={busy}
      data-testid={`template-${template.id}`}
    >
      <span className="st-logo" data-tone="neutral" aria-hidden="true">
        {monogram(name)}
      </span>
      <span className="st-tile-text">
        <b>{name}</b>
        <span>
          {sub ?? (host ? breakableHost(host) : t('settings.providers.template_custom_host'))}
        </span>
      </span>
      <Icon icon={ChevronRight} size={14} className="st-tile-chevron" />
    </button>
  )
}

export function TemplatePicker({
  templates,
  onPick,
  onBack,
  busyId,
}: {
  templates: readonly ProviderTemplate[] | null
  onPick: (t: ProviderTemplate) => void
  onBack: () => void
  busyId: string | null
}) {
  const t = useT()
  // Esc 返回列表（子页的统一约定）；再点一次侧栏的「模型与服务」同样返回
  useKeyScope('template-picker', [{ combo: 'Escape', run: () => onBack() }])
  useReselect(onBack)
  useSubviewTitle(t('settings.providers.pick_title'))
  return (
    <div className="st-editor" data-testid="template-picker">
      <div className="st-editor-head">
        <IconButton
          icon={ArrowLeft}
          label={t('settings.providers.back')}
          onClick={onBack}
          data-testid="picker-back"
        />
        <span className="st-editor-title">
          <h2 tabIndex={-1} data-subview-title>
            {t('settings.providers.pick_title')}
          </h2>
          <span>{t('settings.providers.pick_desc')}</span>
        </span>
      </div>
      {templates === null ? (
        <div className="st-skeleton-card">
          <Skeleton lines={4} />
        </div>
      ) : (
        GROUPS.map((g) => {
          const list = templates.filter((tpl) => g.groups.includes(tpl.group))
          if (list.length === 0) return null
          return (
            <Section key={g.id} title={t(`settings.providers.group.${g.id}`)}>
              <div className="st-tiles">
                {list.map((tpl) => (
                  <TemplateTile
                    key={tpl.id}
                    template={tpl}
                    onPick={onPick}
                    busy={busyId !== null}
                  />
                ))}
              </div>
            </Section>
          )
        })
      )}
    </div>
  )
}
