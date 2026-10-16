/**
 * 设置 → OCR：默认模式（纯文本 / 段落 / 结构化，与面板同一组名字：common.ocr_mode）、图片长边上限（512–4096，默认 2048）、
 * 图片细节（original 只发给 OpenAI 官方端点）、识别后自动复制、截图翻译时附原文、窗口识别、飞入动效、
 * OCR 默认模型的快捷入口（只列视觉模型）、识别历史（开关、上限、列表与回看、关开关时询问是否清空，OcrHistory.tsx）。
 *
 * 锚点 `ocr_history`（OCR 面板 ⋯ 菜单「识别历史…」）：进入分组后把识别历史滚到可见位置、焦点给
 * 分组标题，并短暂高亮（`data-flash`，减少动效时不闪）。
 */
import { Trash2 } from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useEffect, useRef, useState } from 'react'
import { useT } from '../../shared/i18n/useT'
import type { SettingsAnchor } from '../../shared/ipc/sections'
import type { ImageDetail, OcrMode } from '../../shared/ipc/types'
import { usePreset } from '../../shared/motion/presets'
import { duration, ms } from '../../shared/motion/tokens'
import { useReducedMotionPref } from '../../shared/motion/useReducedMotionPref'
import { Button, Field, NumberField, Section, Switch } from '../../shared/ui'
import { useSettingsNav } from '../context'
import { DangerRow } from '../components/Confirm'
import { Page, SegmentRow, SelectRow, SwitchRow } from '../components/Page'
import { at } from '../lib/patch'
import { notify, patchSettings, useSettings } from '../store'
import { engineSupports, ocrEngineOf } from '../../shared/offline/catalog'
import { EngineField } from '../providers/EngineField'
import { ocrOptions } from '../providers/model'
import { OcrHistoryList, TurnOffPrompt, useOcrHistory } from './OcrHistory'

const EDGES = ['1024', '1536', '2048', '3072', '4096'] as const
const HISTORY = { min: 10, max: 500, step: 10 } as const
/** 识别历史的锚点（OCR 面板 ⋯ 菜单「识别历史…」） */
const ANCHOR_OCR_HISTORY: SettingsAnchor = 'ocr_history'
/** 锚点高亮的总时长（ms）：settings.css 的 st-anchor-flash，延迟 slow + 时长 slow × 4.5，再留一帧余量 */
const FLASH_MS = ms(duration.slow * 5.5) + 16

export default function OcrPage() {
  const t = useT()
  const nav = useSettingsNav()
  const settings = useSettings()
  const reduced = useReducedMotionPref()
  const history = useOcrHistory()
  const historyRef = useRef<HTMLDivElement>(null)
  const expand = usePreset('sectionExpand')
  const collapse = usePreset('collapse')
  /** 关掉开关时有已保存的记录：询问是否一并清空 */
  const [askClear, setAskClear] = useState(false)

  // 深链锚点：等列表读完（高度定下来）再滚到识别历史、焦点给分组标题、短暂高亮。高亮的移除不挂在本 effect 的清理上
  // （consumeFocus 让 anchored 立即变回 false，清理会先跑、高亮就永远留着，下一次深链也不会重播）：由不随依赖变化的
  // 计时器按动画时长（令牌算出）移除；每次进入先去掉、强制回流再设置，已经停在 OCR 页时再次深链同样重播
  const flashTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  useEffect(
    () => () => {
      if (flashTimer.current !== null) clearTimeout(flashTimer.current)
    },
    [],
  )
  const anchored = settings !== null && history.loaded && nav.focus === ANCHOR_OCR_HISTORY
  useEffect(() => {
    if (!anchored) return
    nav.consumeFocus()
    const el = historyRef.current
    if (!el) return
    el.scrollIntoView?.({ block: 'start', behavior: reduced ? 'auto' : 'smooth' })
    const title = el.querySelector<HTMLElement>('.ui-section-title')
    if (title) {
      title.tabIndex = -1
      title.focus({ preventScroll: true })
    }
    if (reduced) return
    if (flashTimer.current !== null) clearTimeout(flashTimer.current)
    delete el.dataset.flash
    void el.offsetWidth
    el.dataset.flash = ''
    flashTimer.current = setTimeout(() => {
      flashTimer.current = null
      delete el.dataset.flash
    }, FLASH_MS)
  }, [anchored, nav, reduced])

  if (!settings) return null
  const o = settings.ocr
  const options = ocrOptions(settings.providers)
  // 当前识别引擎不支持「结构化」（PP-OCRv6 与云端的 PP-OCRv6 只识别文本行）：这一项置灰并说明，识别时按段落输出
  const engine = ocrEngineOf(
    null,
    settings.offline.ocr_model,
    settings.defaults.ocr,
    settings.providers,
  )
  const markdownOk = engineSupports(engine, 'markdown')
  const shownMode = o.default_mode === 'translate' ? 'lines' : o.default_mode
  const edge = String(o.max_edge_px)
  const edgeOptions = (EDGES as readonly string[]).includes(edge) ? EDGES : [...EDGES, edge]

  const clearHistory = async () => {
    if (await history.clear()) notify(t('settings.ocr.history_cleared'), 'success')
  }
  const setHistory = (v: boolean) => {
    void patchSettings(at('ocr.history', v))
    setAskClear(!v && history.items.length > 0)
  }
  const count = history.items.length
  /** 识别历史随同步发到其他设备（同步已开启、勾选了「识别历史」）：说明文字不再说只保存在本机 */
  const synced = settings.sync.enabled && settings.sync.content.ocr_history

  return (
    <Page title={t('settings.nav.ocr')} description={t('settings.ocr.description')}>
      <Section title={t('settings.ocr.recognition')}>
        <SegmentRow<Exclude<OcrMode, 'translate'>>
          label={t('settings.ocr.mode')}
          description={t(`settings.ocr.mode_desc.${shownMode}`)}
          message={
            markdownOk
              ? null
              : shownMode === 'markdown'
                ? t('settings.ocr.markdown_fallback')
                : t('settings.ocr.markdown_unsupported')
          }
          messageTone={shownMode === 'markdown' ? 'warning' : 'default'}
          value={shownMode}
          options={(['lines', 'paragraphs', 'markdown'] as const).map((v) => ({
            value: v,
            label: t(`common.ocr_mode.${v}`),
            disabled: v === 'markdown' && !markdownOk && shownMode !== 'markdown',
            title:
              v === 'markdown' && !markdownOk ? t('settings.ocr.markdown_unsupported') : undefined,
          }))}
          onChange={(v) => void patchSettings(at('ocr.default_mode', v))}
          ariaLabel={t('settings.ocr.mode')}
          data-testid="ocr-mode"
        />
        <EngineField task="ocr" offline label={t('settings.ocr.model')} data-testid="ocr-model" />
        {options.length === 0 && settings.offline.ocr_model === null ? (
          <Field label={t('settings.ocr.no_model')} description={t('settings.ocr.no_model_desc')}>
            <Button size="sm" onClick={() => nav.go('providers')}>
              {t('settings.ocr.go_providers')}
            </Button>
          </Field>
        ) : null}
        <SelectRow<string>
          label={t('settings.ocr.max_edge')}
          description={t('settings.ocr.max_edge_desc')}
          value={edge}
          options={edgeOptions.map((v) => ({
            value: v,
            label: t('settings.unit.px_value', { n: v }),
          }))}
          onChange={(v) => void patchSettings(at('ocr.max_edge_px', Number(v)))}
          ariaLabel={t('settings.ocr.max_edge')}
          data-testid="ocr-edge"
        />
        <SelectRow<ImageDetail>
          label={t('settings.ocr.detail')}
          description={t(`settings.ocr.detail_desc.${o.image_detail}`)}
          value={o.image_detail}
          options={(['high', 'auto', 'low', 'original'] as const).map((v) => ({
            value: v,
            label: t(`settings.ocr.details.${v}`),
            hint: v === 'original' ? t('settings.ocr.original_only') : undefined,
          }))}
          onChange={(v) => void patchSettings(at('ocr.image_detail', v))}
          ariaLabel={t('settings.ocr.detail')}
          data-testid="ocr-detail"
        />
      </Section>

      <Section title={t('settings.ocr.after')}>
        <SwitchRow
          label={t('settings.ocr.auto_copy')}
          description={t('settings.ocr.auto_copy_desc')}
          checked={o.auto_copy}
          onChange={(v) => void patchSettings(at('ocr.auto_copy', v))}
          ariaLabel={t('settings.ocr.auto_copy')}
          data-testid="ocr-auto-copy"
        />
        <SwitchRow
          label={t('settings.ocr.transcribe')}
          description={t('settings.ocr.transcribe_desc')}
          checked={o.direct_translate_transcribe}
          onChange={(v) => void patchSettings(at('ocr.direct_translate_transcribe', v))}
          ariaLabel={t('settings.ocr.transcribe')}
        />
      </Section>

      <Section title={t('settings.ocr.capture')}>
        <SwitchRow
          label={t('settings.ocr.window_detect')}
          description={t('settings.ocr.window_detect_desc')}
          checked={o.window_detect}
          onChange={(v) => void patchSettings(at('ocr.window_detect', v))}
          ariaLabel={t('settings.ocr.window_detect')}
        />
        <SwitchRow
          label={t('settings.ocr.fly')}
          description={t('settings.ocr.fly_desc')}
          checked={o.fly_animation}
          onChange={(v) => void patchSettings(at('ocr.fly_animation', v))}
          ariaLabel={t('settings.ocr.fly')}
        />
      </Section>

      <div ref={historyRef} className="st-anchor" data-testid="ocr-history-section">
        <Section
          title={t('settings.ocr.history')}
          description={t('settings.ocr.history_desc')}
          action={
            count > 0 ? (
              <span className="st-section-meta" data-testid="ocr-history-count">
                {history.more
                  ? t('settings.ocr.history_count_other', { n: `${count}+` })
                  : t('settings.ocr.history_count', { count, n: count })}
              </span>
            ) : null
          }
        >
          <Field label={t('settings.ocr.history_enable')}>
            <Switch
              checked={o.history}
              aria-label={t('settings.ocr.history_enable')}
              onChange={setHistory}
              data-testid="ocr-history"
            />
          </Field>
          <AnimatePresence initial={false}>
            {askClear && !o.history && count > 0 ? (
              <TurnOffPrompt
                key="off"
                count={count}
                onKeep={() => setAskClear(false)}
                onClear={() => {
                  setAskClear(false)
                  void clearHistory()
                }}
              />
            ) : null}
          </AnimatePresence>
          {/* 「最多保存」随开关展开 / 收起（与询问条同时，都是高度过渡），不在一帧里出现 / 消失 */}
          <AnimatePresence initial={false}>
            {o.history ? (
              <m.div key="limit" className="st-clip" {...expand} exit={collapse.exit}>
                <Field label={t('settings.ocr.history_limit')}>
                  <NumberField
                    value={o.history_limit}
                    min={HISTORY.min}
                    max={HISTORY.max}
                    step={HISTORY.step}
                    unit={t('settings.unit.items')}
                    aria-label={t('settings.ocr.history_limit')}
                    className="st-number"
                    onChange={(v) => void patchSettings(at('ocr.history_limit', v))}
                  />
                </Field>
              </m.div>
            ) : null}
          </AnimatePresence>
          <OcrHistoryList
            enabled={o.history}
            limit={o.history_limit}
            synced={synced}
            history={history}
          />
          {count > 0 ? (
            <DangerRow
              label={t('settings.ocr.history_clear')}
              description={t('settings.ocr.history_clear_desc')}
              action={t('settings.ocr.history_clear_action')}
              confirm={
                // 本机正在记录并同步识别历史时，清空会同步到其他设备；没在记录时清空只影响本机
                synced && o.history
                  ? t('settings.ocr.history_clear_confirm_synced')
                  : t('settings.ocr.history_clear_confirm')
              }
              confirmLabel={t('settings.ocr.history_clear_ok')}
              icon={Trash2}
              onConfirm={() => void clearHistory()}
              data-testid="ocr-history-clear"
            />
          ) : null}
        </Section>
      </div>
    </Page>
  )
}
