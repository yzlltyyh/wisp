/**
 * 设置 → 翻译。
 *
 * - 语言：主目标语言（默认中文）与第二语言（原文已是主目标语言时译成它）；默认风格；
 * - 风格管理：排序（决定 chip 顺序与 ⌘1–9）、新建（立即出现在 chip 末尾）、编辑 / 还原 / 删除（风格编辑器子页）；
 * - 术语表；
 * - 面板行为：关闭条件（Esc 永远有效，另可选点击外部 / 鼠标离开 N px）、面板位置（跟随选区 / 光标；输入模式可居中）、
 *   长文本阈值、默认显示原文、结果缓存。
 * 列表 ⇄ 风格编辑器的切换只有新视图的进入动画；进入编辑器滚到顶部，返回时恢复列表的滚动位置并把焦点还给那一行。
 * 深链锚点：`styles` 滚到风格列表，`style:<id>` 打开风格编辑器。
 */
import { m } from 'motion/react'
import { useEffect, useRef, useState } from 'react'
import { usePlatform } from '../../shared/hooks/usePlatform'
import { useT } from '../../shared/i18n/useT'
import { api } from '../../shared/ipc'
import type { InputAnchor, Lang, Style, TranslateAnchor } from '../../shared/ipc/types'
import { adaptPreset, presets } from '../../shared/motion/presets'
import { useReducedMotionPref } from '../../shared/motion/useReducedMotionPref'
import { formatHotkey } from '../../shared/keys/formatHotkey'
import { Field, NumberField, Section, Switch } from '../../shared/ui'
import { useSettingsNav } from '../context'
import { Page, SegmentRow, SelectRow, SwitchRow } from '../components/Page'
import { ipcErrorText } from '../lib/errors'
import { langPairPatch } from '../lib/langs'
import { at } from '../lib/patch'
import { applyServerStyles } from '../lib/styleSave'
import { styleName } from '../lib/styles'
import { useSubviewNav } from '../lib/subview'
import { notify, patchSettings, setStyles, useRuntime, useSettings, useStyles } from '../store'
import { GlossaryEditor } from '../translate/GlossaryEditor'
import { StyleEditor } from '../translate/StyleEditor'
import { StyleList } from '../translate/StyleList'

const LANGS: readonly Lang[] = ['zh-CN', 'en', 'ja', 'ko']

/** translate.* 的数值范围（wisp_types::defaults::ranges） */
const LONG_TEXT = { min: 500, max: 20_000, step: 500 } as const
const MOUSE_LEAVE = { min: 80, max: 400, step: 10 } as const

export default function TranslatePage() {
  const t = useT()
  const os = usePlatform()
  const nav = useSettingsNav()
  const styles = useStyles()
  const settings = useSettings()
  const runtime = useRuntime()
  const reduced = useReducedMotionPref()
  const [editing, setEditing] = useState<string | null>(null)
  const [dir, setDir] = useState<1 | -1>(1)
  const [creating, setCreating] = useState(false)
  const listRef = useRef<HTMLDivElement>(null)
  const editingStyle = editing ? styles.find((s) => s.id === editing) : undefined
  const sub = useSubviewNav(editingStyle ? `style:${editingStyle.id}` : 'list')

  const open = (id: string | null, returnTo: string | null = null) => {
    sub.leave(returnTo)
    setDir(id ? 1 : -1)
    setEditing(id)
  }

  // 深链锚点（随分组一起出现：不播子视图的进入动画）
  useEffect(() => {
    const f = nav.focus
    if (!f) return
    if (f.startsWith('style:')) setEditing(f.slice('style:'.length))
    else if (f === 'styles') {
      setEditing(null)
      requestAnimationFrame(() => listRef.current?.scrollIntoView?.({ block: 'start' }))
    }
    nav.consumeFocus()
    // eslint-disable-next-line react/exhaustive-deps -- 只响应锚点变化
  }, [nav.focus])

  if (!settings) return null
  const tr = settings.translate
  const natural = styles.find((s) => s.id === 'natural') ?? null

  const reorder = async (ids: string[]) => {
    const before = styles
    const byId = new Map(styles.map((s) => [s.id, s]))
    setStyles(ids.flatMap((id) => (byId.get(id) ? [byId.get(id) as Style] : [])))
    try {
      applyServerStyles(await api.stylesReorder(ids))
    } catch (error) {
      setStyles(before)
      notify(ipcErrorText(error, 'styles_reorder'), 'danger')
    }
  }

  const create = async () => {
    setCreating(true)
    try {
      const n = styles.filter((s) => !s.builtin).length + 1
      // 不带 temperature / dictionary_mode：由 Rust 按自定义风格的默认值补齐（wisp_types::Style::custom）；
      // 模板从「自然」开始，结尾指令留空（装配时用「自然」的）
      const draft = {
        id: '',
        name: t('settings.styles.new_name', { n }),
        builtin: false,
        system_template: natural?.system_template ?? '',
        tail_instruction: '',
        output: 'plain_text',
        model_ref: null,
        hotkey: null,
        modified: false,
      } as unknown as Style
      const before = new Set(styles.map((s) => s.id))
      const next = await api.styleSave(draft)
      applyServerStyles(next)
      const created = next.find((s) => !before.has(s.id))
      if (created) open(created.id)
    } catch (error) {
      notify(ipcErrorText(error, 'style_save'), 'danger')
    } finally {
      setCreating(false)
    }
  }

  const motion = adaptPreset(presets.stepSwitch(dir), 'fade', reduced)
  // 只有新视图的进入动画（旧视图立即卸载）；分组首次显示时由分组切换的淡入负责
  const enter = { initial: sub.entered ? motion.initial : false, animate: motion.animate }

  return (
    <div ref={sub.host} className="st-subview-host">
      {editingStyle ? (
        <m.div key={`style:${editingStyle.id}`} className="st-subview" {...enter}>
          <StyleEditor
            style={editingStyle}
            natural={natural}
            providers={settings.providers}
            hotkey={runtime?.hotkeys.find(
              (h) => h.target.kind === 'style' && h.target.style_id === editingStyle.id,
            )}
            onBack={() => open(null)}
          />
        </m.div>
      ) : (
        <m.div key="list" className="st-subview" {...enter}>
          <Page
            title={t('settings.nav.translate')}
            description={t('settings.translate.description')}
          >
            <Section title={t('settings.translate.languages')}>
              <SelectRow<Lang>
                label={t('settings.translate.primary')}
                description={t('settings.translate.primary_desc')}
                value={tr.primary_lang}
                options={LANGS.map((l) => ({ value: l, label: t(`common.lang.name.${l}`) }))}
                // 改成当前的第二语言：两者对调（一次写入），不会出现「英文译成英文」
                onChange={(v) => void patchSettings(langPairPatch(tr, 'primary', v))}
                ariaLabel={t('settings.translate.primary')}
                data-testid="translate-primary"
              />
              <SelectRow<Lang>
                label={t('settings.translate.secondary')}
                description={t('settings.translate.secondary_desc', {
                  lang: t(`common.lang.name.${tr.primary_lang}`),
                })}
                // 两者相同（手改了 settings.json）：显示为待选择并标红，而不是一个空白的下拉框
                value={tr.secondary_lang === tr.primary_lang ? null : tr.secondary_lang}
                options={LANGS.filter((l) => l !== tr.primary_lang).map((l) => ({
                  value: l,
                  label: t(`common.lang.name.${l}`),
                }))}
                placeholder={t('settings.translate.secondary_choose')}
                invalid={tr.secondary_lang === tr.primary_lang}
                message={
                  tr.secondary_lang === tr.primary_lang
                    ? t('settings.translate.secondary_same')
                    : null
                }
                onChange={(v) => void patchSettings(langPairPatch(tr, 'secondary', v))}
                ariaLabel={t('settings.translate.secondary')}
                data-testid="translate-secondary"
              />
              <SelectRow<string>
                label={t('settings.translate.default_style')}
                description={t('settings.translate.default_style_desc')}
                value={tr.default_style}
                options={styles.map((s) => ({ value: s.id, label: styleName(s) }))}
                onChange={(v) => void patchSettings(at('translate.default_style', v))}
                ariaLabel={t('settings.translate.default_style')}
                data-testid="translate-default-style"
              />
            </Section>

            <div ref={listRef} className="st-anchor">
              <StyleList
                styles={styles}
                providers={settings.providers}
                defaultStyle={tr.default_style}
                onOpen={(id, keyboard) => open(id, keyboard ? `style:${id}` : null)}
                onReorder={(ids) => void reorder(ids)}
                onCreate={() => void create()}
                creating={creating}
              />
            </div>

            <GlossaryEditor />

            <Section
              title={t('settings.translate.panel')}
              description={t('settings.translate.panel_desc')}
            >
              <SwitchRow
                label={t('settings.translate.close_outside')}
                checked={tr.close_on_click_outside}
                onChange={(v) => void patchSettings(at('translate.close_on_click_outside', v))}
                ariaLabel={t('settings.translate.close_outside')}
                data-testid="translate-close-outside"
              />
              <Field
                label={t('settings.translate.close_leave')}
                description={t('settings.translate.close_leave_desc')}
              >
                {tr.close_on_mouse_leave ? (
                  <NumberField
                    value={tr.mouse_leave_px}
                    min={MOUSE_LEAVE.min}
                    max={MOUSE_LEAVE.max}
                    step={MOUSE_LEAVE.step}
                    unit={t('settings.unit.px')}
                    aria-label={t('settings.translate.close_leave_px')}
                    className="st-number"
                    onChange={(v) => void patchSettings(at('translate.mouse_leave_px', v))}
                  />
                ) : null}
                <Switch
                  checked={tr.close_on_mouse_leave}
                  aria-label={t('settings.translate.close_leave')}
                  onChange={(v) => void patchSettings(at('translate.close_on_mouse_leave', v))}
                  data-testid="translate-close-leave"
                />
              </Field>
              <SegmentRow<TranslateAnchor>
                label={t('settings.translate.anchor')}
                description={t('settings.translate.anchor_desc')}
                value={tr.anchor}
                options={[
                  { value: 'selection', label: t('settings.translate.anchor_selection') },
                  { value: 'cursor', label: t('settings.translate.anchor_cursor') },
                ]}
                onChange={(v) => void patchSettings(at('translate.anchor', v))}
                ariaLabel={t('settings.translate.anchor')}
              />
              <SegmentRow<InputAnchor>
                label={t('settings.translate.input_anchor')}
                value={tr.input_anchor}
                options={[
                  { value: 'cursor', label: t('settings.translate.anchor_cursor') },
                  { value: 'center', label: t('settings.translate.anchor_center') },
                ]}
                onChange={(v) => void patchSettings(at('translate.input_anchor', v))}
                ariaLabel={t('settings.translate.input_anchor')}
              />
              <Field
                label={t('settings.translate.long_text')}
                description={t('settings.translate.long_text_desc')}
              >
                <NumberField
                  value={tr.long_text_threshold}
                  min={LONG_TEXT.min}
                  max={LONG_TEXT.max}
                  step={LONG_TEXT.step}
                  unit={t('settings.unit.chars')}
                  aria-label={t('settings.translate.long_text')}
                  className="st-number"
                  onChange={(v) => void patchSettings(at('translate.long_text_threshold', v))}
                />
              </Field>
              <SwitchRow
                label={t('settings.translate.show_source')}
                checked={tr.show_source_by_default}
                onChange={(v) => void patchSettings(at('translate.show_source_by_default', v))}
                ariaLabel={t('settings.translate.show_source')}
              />
              <SwitchRow
                label={t('settings.translate.cache')}
                description={t('settings.translate.cache_desc', {
                  combo: formatHotkey('Mod+KeyR', os),
                })}
                checked={tr.cache_enabled}
                onChange={(v) => void patchSettings(at('translate.cache_enabled', v))}
                ariaLabel={t('settings.translate.cache')}
              />
            </Section>
          </Page>
        </m.div>
      )}
    </div>
  )
}
