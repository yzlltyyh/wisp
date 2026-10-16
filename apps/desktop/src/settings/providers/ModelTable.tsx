/**
 * 模型列表：搜索、启用开关、vision 三态（手动设置记为 user 来源，允许手动覆盖）、
 * reasoning 开关、推理档位（OpenAI 系推理模型：默认（low）= null，另有 low / minimal / none）、上游已移除 / 已弃用标记、
 * 手动添加、重置能力缓存、删除。列表虚拟化（500 个模型滚动不掉帧）：只渲染视口内的行，行高固定。
 * 「模型 ID」是这张表最主要的信息：它占满剩余宽度，徽标（默认 / 已弃用 / 上游已移除）与非默认的推理档位放在 ID 下面的
 * 副行，不和 ID 抢宽度；推理档位不再占一整列（大多数行用不到），在这一行的 ⋯ 菜单里选（只对开了推理的行出现）。
 * 列标题写明「图片」「推理」两列图标的含义（不用等 tooltip）；vision 三态是 roving tabindex 的单选组。
 * 「全部停用」跳过被默认模型 / 风格引用的模型并提示（否则默认模型随之失效）。
 * 文字识别服务（PaddleOCR，`service`）的模型不是 LLM：没有「图片」「推理」两列与能力缓存，只有启用、名称与删除；
 * `noteFor` 可以给某些模型加一句副行说明（例如 PP-StructureV3 较慢）。
 */
import { useVirtualizer } from '@tanstack/react-virtual'
import { Brain, Eye, EyeOff, Plus, ScanEye, Trash2, Undo2, MoreHorizontal } from 'lucide-react'
import type { MenuEntry } from '../../shared/ui'
import { useMemo, useRef, useState } from 'react'
import { useT } from '../../shared/i18n/useT'
import type { ModelEntry, ReasoningEffort, Vision } from '../../shared/ipc/types'
import {
  Badge,
  Button,
  DropdownMenu,
  EmptyState,
  IconButton,
  SearchField,
  Switch,
  TextField,
  Tooltip,
  cx,
  Icon,
} from '../../shared/ui'
import { radioGroupKeys, rovingTabIndex } from '../lib/radio'
import { notify } from '../store'
import { newModelEntry } from './model'

/** 行高（与 --h-row 一致） */
const ROW_HEIGHT = 44
/** 列表最多显示这么多行，其余内部滚动 */
const MAX_VISIBLE_ROWS = 8

export interface ModelTableProps {
  models: readonly ModelEntry[]
  onChange: (models: ModelEntry[]) => void
  /** 推理档位只对 OpenAI 系（thinking_off = openai_effort）有效 */
  showEffort: boolean
  onResetCaps?: (modelId: string) => void
  /** 默认值引用的模型（不能删除，停用时提示） */
  referenced?: ReadonlySet<string>
  /** 文字识别服务的模型：不显示「图片」「推理」两列 */
  service?: boolean
  /** 模型 ID 下面的副行说明（没有时返回 null） */
  noteFor?: (m: ModelEntry) => string | null
}

const VISION_ORDER: readonly Vision[] = ['yes', 'unknown', 'no']
/** 推理档位：null = 默认（low），其余三档显式写入 */
const EFFORTS: ReadonlyArray<ReasoningEffort | null> = [null, 'low', 'minimal', 'none']
const VISION_ICON = { yes: Eye, unknown: ScanEye, no: EyeOff } as const

export function ModelTable({
  models,
  onChange,
  showEffort,
  onResetCaps,
  referenced,
  service = false,
  noteFor,
}: ModelTableProps) {
  const t = useT()
  const [query, setQuery] = useState('')
  const [adding, setAdding] = useState('')
  const scroller = useRef<HTMLDivElement>(null)
  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase()
    if (!q) return models.map((m, i) => ({ m, i }))
    return models
      .map((m, i) => ({ m, i }))
      .filter(
        ({ m }) => m.id.toLowerCase().includes(q) || (m.label ?? '').toLowerCase().includes(q),
      )
  }, [models, query])
  // oxlint-disable-next-line react/incompatible-library -- 本项目不用 React Compiler；与 shared/ui/VirtualList 相同
  const virtualizer = useVirtualizer({
    count: filtered.length,
    getScrollElement: () => scroller.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 6,
    initialRect: { width: 560, height: Math.min(filtered.length, MAX_VISIBLE_ROWS) * ROW_HEIGHT },
  })
  const enabledCount = models.filter((m) => m.enabled).length

  const update = (index: number, patch: Partial<ModelEntry>) => {
    onChange(models.map((m, i) => (i === index ? { ...m, ...patch } : m)))
  }
  const add = () => {
    const id = adding.trim()
    if (!id) return
    if (models.some((m) => m.id === id)) {
      setQuery(id)
      setAdding('')
      return
    }
    onChange([...models, newModelEntry(id)])
    setAdding('')
  }
  const setAll = (enabled: boolean) => {
    if (enabled) {
      onChange(models.map((m) => ({ ...m, enabled: true })))
      return
    }
    // 被默认模型 / 风格引用的保持启用，否则默认翻译 / OCR 模型会随之失效
    const kept = models.filter((m) => m.enabled && referenced?.has(m.id)).length
    onChange(models.map((m) => (referenced?.has(m.id) ? m : { ...m, enabled: false })))
    if (kept > 0) notify(t('settings.models.kept_referenced', { count: kept, n: kept }), 'info')
  }
  const height = Math.min(Math.max(filtered.length, 1), MAX_VISIBLE_ROWS) * ROW_HEIGHT
  const effortLabel = (level: ReasoningEffort | null) =>
    level === null ? t('settings.models.effort_default') : t(`settings.models.effort_${level}`)

  return (
    <div className="st-models" data-testid="model-table">
      <div className="st-models-bar">
        <SearchField
          value={query}
          onChange={setQuery}
          placeholder={t('settings.models.search')}
          className="st-grow"
          data-testid="model-search"
        />
        <span className="st-models-count st-tabular">
          {t('settings.models.count', { enabled: enabledCount, total: models.length })}
        </span>
        <DropdownMenu
          label={t('common.action.more')}
          items={[
            { id: 'all', label: t('settings.models.enable_all'), onSelect: () => setAll(true) },
            { id: 'none', label: t('settings.models.disable_all'), onSelect: () => setAll(false) },
          ]}
          renderTrigger={(p) => (
            <IconButton {...p} icon={MoreHorizontal} label={t('common.action.more')} size="sm" />
          )}
        />
      </div>
      {models.length === 0 ? (
        <EmptyState
          compact
          title={t('settings.models.empty')}
          detail={t('settings.models.empty_detail')}
          data-testid="model-empty"
        />
      ) : filtered.length === 0 ? (
        <div className="st-models-none">{t('common.state.no_results')}</div>
      ) : (
        <>
          <div className="st-models-head" aria-hidden="true">
            <span className="st-models-head-switch" />
            <span className="st-model-name">{t('settings.models.col_model')}</span>
            {service ? null : (
              <>
                <span className="st-models-head-vision">{t('settings.models.col_vision')}</span>
                <span className="st-models-head-reasoning">
                  {t('settings.models.col_reasoning')}
                </span>
              </>
            )}
            <span className="st-models-head-more" />
          </div>
          <div ref={scroller} className="st-models-scroll" style={{ height }}>
            <ul
              className="st-models-inner"
              style={{ height: virtualizer.getTotalSize() }}
              aria-label={t('settings.models.title')}
            >
              {virtualizer.getVirtualItems().map((row) => {
                const item = filtered[row.index]
                if (!item) return null
                const { m, i } = item
                const isRef = referenced?.has(m.id) ?? false
                const badge = m.missing_upstream ? (
                  <Badge tone="warning">{t('settings.models.missing_upstream')}</Badge>
                ) : m.deprecated ? (
                  <Badge tone="neutral">{t('settings.models.deprecated')}</Badge>
                ) : isRef ? (
                  <Badge tone="accent">{t('settings.models.default')}</Badge>
                ) : null
                const effortOn = !service && showEffort && m.reasoning
                const extra = noteFor?.(m) ?? null
                const effortNote =
                  effortOn && m.reasoning_effort ? (
                    <span className="st-model-effort">
                      {t('settings.models.effort_note', { level: effortLabel(m.reasoning_effort) })}
                    </span>
                  ) : extra ? (
                    <span className="st-model-effort" data-testid={`model-note-${m.id}`}>
                      {extra}
                    </span>
                  ) : null
                const effortItems: MenuEntry[] = effortOn
                  ? [
                      { type: 'label', id: 'effort-label', label: t('settings.models.effort') },
                      ...EFFORTS.map((level) => ({
                        id: `effort-${level ?? 'default'}`,
                        label: effortLabel(level),
                        checked: (m.reasoning_effort ?? null) === level,
                        onSelect: () => update(i, { reasoning_effort: level }),
                      })),
                      { type: 'separator', id: 'effort-sep' },
                    ]
                  : []
                return (
                  <li
                    key={m.id}
                    className="st-model-row"
                    data-disabled={!m.enabled || undefined}
                    data-testid={`model-row-${m.id}`}
                    style={{ transform: `translateY(${row.start}px)` }}
                  >
                    <Switch
                      size="sm"
                      checked={m.enabled}
                      aria-label={t('settings.models.enable', { model: m.id })}
                      onChange={(v) => update(i, { enabled: v })}
                      data-testid={`model-enable-${m.id}`}
                    />
                    <span className="st-model-name">
                      <span className="st-model-id" title={m.id}>
                        {m.label?.trim() || m.id}
                      </span>
                      {badge || effortNote ? (
                        <span className="st-model-meta">
                          {badge}
                          {effortNote}
                        </span>
                      ) : null}
                    </span>
                    {service ? null : (
                      <VisionToggle
                        value={m.vision}
                        model={m.id}
                        onChange={(v) => update(i, { vision: v, vision_source: 'user' })}
                      />
                    )}
                    {service ? null : (
                      <span className="st-model-reasoning">
                        <Tooltip content={t('settings.models.reasoning_tip')}>
                          <button
                            type="button"
                            className="st-chip-toggle"
                            aria-pressed={m.reasoning}
                            aria-label={t('settings.models.reasoning')}
                            onClick={() => update(i, { reasoning: !m.reasoning })}
                            data-testid={`model-reasoning-${m.id}`}
                          >
                            <Icon icon={Brain} size={14} />
                          </button>
                        </Tooltip>
                      </span>
                    )}
                    <DropdownMenu
                      label={t('common.action.more')}
                      align="end"
                      items={[
                        ...effortItems,
                        ...(onResetCaps && !service
                          ? [
                              {
                                id: 'caps',
                                label: t('settings.models.reset_caps'),
                                icon: Undo2,
                                onSelect: () => onResetCaps(m.id),
                              },
                            ]
                          : []),
                        {
                          id: 'delete',
                          label: t('settings.models.remove'),
                          icon: Trash2,
                          danger: true,
                          disabled: isRef,
                          hint: isRef ? t('settings.models.remove_ref') : undefined,
                          onSelect: () => onChange(models.filter((_, k) => k !== i)),
                        },
                      ]}
                      renderTrigger={(p) => (
                        <IconButton
                          {...p}
                          icon={MoreHorizontal}
                          label={t('common.action.more')}
                          size="sm"
                          noTooltip
                          data-testid={`model-more-${m.id}`}
                        />
                      )}
                    />
                  </li>
                )
              })}
            </ul>
          </div>
        </>
      )}
      <div className="st-models-add">
        <TextField
          value={adding}
          onChange={setAdding}
          placeholder={t('settings.models.add_placeholder')}
          aria-label={t('settings.models.add_placeholder')}
          className="st-grow st-mono"
          onEnter={add}
          data-testid="model-add-input"
        />
        <Button
          size="sm"
          icon={Plus}
          disabled={!adding.trim()}
          onClick={add}
          data-testid="model-add"
        >
          {t('settings.models.add')}
        </Button>
      </div>
    </div>
  )
}

function VisionToggle({
  value,
  model,
  onChange,
}: {
  value: Vision
  model: string
  onChange: (v: Vision) => void
}) {
  const t = useT()
  const refs = useRef<Array<HTMLButtonElement | null>>([])
  const checked = VISION_ORDER.indexOf(value)
  return (
    <span
      className="st-vision"
      role="radiogroup"
      tabIndex={-1}
      aria-label={t('settings.models.vision_label', { model })}
      data-testid={`model-vision-${model}`}
      onKeyDown={(e) =>
        radioGroupKeys(
          VISION_ORDER.length,
          checked,
          (i) => onChange(VISION_ORDER[i] as Vision),
          (i) => refs.current[i]?.focus(),
        )(e)
      }
    >
      {VISION_ORDER.map((v, i) => (
        <Tooltip key={v} content={t(`settings.models.vision.${v}`)}>
          <button
            ref={(el) => {
              refs.current[i] = el
            }}
            type="button"
            // 自绘的单选（图标三态，原生 radio 画不出），键盘按 WAI-ARIA radiogroup（lib/radio.ts）
            // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role
            role="radio"
            aria-checked={value === v}
            aria-label={t(`settings.models.vision.${v}`)}
            tabIndex={rovingTabIndex(i, checked)}
            className={cx('st-vision-item')}
            data-value={v}
            onClick={() => onChange(v)}
          >
            <Icon icon={VISION_ICON[v]} size={13} />
          </button>
        </Tooltip>
      ))}
    </span>
  )
}
