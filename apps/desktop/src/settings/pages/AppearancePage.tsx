/**
 * 设置 → 外观：主题、强调色（跟随系统 / 8 个预设 / 自定义）、正文字号（12–18）、
 * 翻译面板宽度上下限（320–480 / 440–720）、剪贴板宽度（360–560）、高度上限（40–80%）、材质（系统 / 纯色）、
 * 减少动效（三态）、Overlay 飞入。所有窗口经 `appearance:changed` 实时响应；顶部的小预览随改动即时变化。
 * 滑块拖动时本地即时更新，停顿后才写入（合并写入队列）。
 */
import { Check, Copy, Monitor, Moon, Pin, Pipette, Sun } from 'lucide-react'
import { useRef } from 'react'
import { useAppearance } from '../../shared/hooks/useAppearance'
import { useT } from '../../shared/i18n/useT'
import type { MaterialPref, ReduceMotionPref, Settings, ThemePref } from '../../shared/ipc/types'
import {
  Field,
  Icon,
  IconButton,
  LangBar,
  Section,
  Slider,
  StatusLine,
  StyleChip,
} from '../../shared/ui'
import { Page, SegmentRow, SwitchRow } from '../components/Page'
import { hexColor } from '../lib/color'
import { at } from '../lib/patch'
import { radioGroupKeys, rovingTabIndex } from '../lib/radio'
import { patchSettings, queuePatch, useSettings } from '../store'

/**
 * 预设强调色的色板（只用于显示色块）：与 Rust `app::appearance::preset_accent` 的取值一致（浅色 / 深色各一档），
 * 实际生效的颜色由 Rust 解析后经 `appearance:changed` 下发。
 */
const PRESETS: ReadonlyArray<{ id: string; light: string; dark: string }> = [
  { id: 'blue', light: '#3B82F6', dark: '#60A5FA' },
  { id: 'purple', light: '#8B5CF6', dark: '#A78BFA' },
  { id: 'pink', light: '#EC4899', dark: '#F472B6' },
  { id: 'red', light: '#EF4444', dark: '#F87171' },
  { id: 'orange', light: '#F97316', dark: '#FB923C' },
  { id: 'yellow', light: '#CA8A04', dark: '#FACC15' },
  { id: 'green', light: '#16A34A', dark: '#4ADE80' },
  { id: 'graphite', light: '#6B7280', dark: '#9CA3AF' },
]

const HEX = /^#[0-9a-f]{6}$/i
const PERCENT = 100
/** 自定义取色器的初值：当前实际生效的强调色（令牌 --accent）；读不到时用默认的蓝色预设 */
function currentAccentHex(theme: 'light' | 'dark'): string {
  const fromToken =
    typeof getComputedStyle === 'function'
      ? hexColor(getComputedStyle(document.documentElement).getPropertyValue('--accent'))
      : ''
  const blue = PRESETS[0] as (typeof PRESETS)[number]
  return fromToken || (theme === 'dark' ? blue.dark : blue.light)
}

export default function AppearancePage() {
  const t = useT()
  const settings = useSettings()
  const look = useAppearance()
  const swatchRefs = useRef<Array<HTMLButtonElement | null>>([])
  if (!settings) return null
  const a = settings.appearance
  const custom = HEX.test(a.accent) ? a.accent : null
  const queue = (path: string, v: unknown) => queuePatch(path, at(path, v))
  /** 单选组的选项：跟随系统 + 8 个预设（自定义取色是单独的控件，不在单选组里） */
  const accents = ['system', ...PRESETS.map((p) => p.id)]
  const checked = accents.indexOf(a.accent)
  const swatchProps = (i: number) => ({
    ref: (el: HTMLButtonElement | null) => {
      swatchRefs.current[i] = el
    },
    tabIndex: rovingTabIndex(i, checked),
  })

  return (
    <Page title={t('settings.nav.appearance')} description={t('settings.appearance.description')}>
      <Preview fontSize={a.font_size} model={previewModelLabel(settings)} />

      <Section title={t('settings.appearance.color')}>
        <SegmentRow<ThemePref>
          label={t('settings.appearance.theme')}
          value={a.theme}
          options={[
            { value: 'system', label: t('settings.appearance.theme_system'), icon: Monitor },
            { value: 'light', label: t('settings.appearance.theme_light'), icon: Sun },
            { value: 'dark', label: t('settings.appearance.theme_dark'), icon: Moon },
          ]}
          onChange={(v) => void patchSettings(at('appearance.theme', v))}
          ariaLabel={t('settings.appearance.theme')}
          data-testid="appearance-theme"
        />
        <Field
          label={t('settings.appearance.accent')}
          description={t('settings.appearance.accent_desc')}
          stacked
        >
          <div className="st-swatches" data-testid="appearance-accent">
            <div
              className="st-swatch-group"
              role="radiogroup"
              tabIndex={-1}
              aria-label={t('settings.appearance.accent')}
              onKeyDown={(e) =>
                radioGroupKeys(
                  accents.length,
                  checked,
                  (i) => void patchSettings(at('appearance.accent', accents[i])),
                  (i) => swatchRefs.current[i]?.focus(),
                )(e)
              }
            >
              <button
                {...swatchProps(0)}
                type="button"
                // 自绘的单选（色块，原生 radio 画不出），键盘按 WAI-ARIA radiogroup（lib/radio.ts）
                // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role
                role="radio"
                aria-checked={a.accent === 'system'}
                className="st-swatch st-swatch-system"
                title={t('settings.appearance.accent_system')}
                aria-label={t('settings.appearance.accent_system')}
                onClick={() => void patchSettings(at('appearance.accent', 'system'))}
                data-testid="accent-system"
              >
                {a.accent === 'system' ? <Icon icon={Check} size={12} /> : null}
              </button>
              {PRESETS.map((p, i) => (
                <button
                  key={p.id}
                  {...swatchProps(i + 1)}
                  type="button"
                  // 自绘的单选（色块，原生 radio 画不出），键盘按 WAI-ARIA radiogroup（lib/radio.ts）
                  // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role
                  role="radio"
                  aria-checked={a.accent === p.id}
                  className="st-swatch"
                  style={{ ['--swatch' as string]: look.theme === 'dark' ? p.dark : p.light }}
                  title={t(
                    `settings.appearance.accents.${p.id}` as 'settings.appearance.accents.blue',
                  )}
                  aria-label={t(
                    `settings.appearance.accents.${p.id}` as 'settings.appearance.accents.blue',
                  )}
                  onClick={() => void patchSettings(at('appearance.accent', p.id))}
                  data-testid={`accent-${p.id}`}
                >
                  {a.accent === p.id ? <Icon icon={Check} size={12} /> : null}
                </button>
              ))}
            </div>
            <label
              className="st-swatch st-swatch-custom"
              data-checked={custom !== null || undefined}
              style={custom ? { ['--swatch' as string]: custom } : undefined}
              title={t('settings.appearance.accent_custom')}
            >
              <input
                type="color"
                className="st-color-input"
                value={custom ?? (hexColor(look.accent) || currentAccentHex(look.theme))}
                aria-label={t('settings.appearance.accent_custom')}
                onChange={(e) => queue('appearance.accent', e.target.value.toUpperCase())}
                data-testid="accent-custom"
              />
              <Icon icon={custom ? Check : Pipette} size={12} />
            </label>
            <span className="st-swatch-label">
              {a.accent === 'system'
                ? t('settings.appearance.accent_system')
                : custom
                  ? custom.toUpperCase()
                  : t(
                      `settings.appearance.accents.${a.accent}` as 'settings.appearance.accents.blue',
                    )}
            </span>
          </div>
        </Field>
        <SegmentRow<MaterialPref>
          label={t('settings.appearance.material')}
          description={t('settings.appearance.material_desc')}
          value={a.material}
          options={[
            { value: 'system', label: t('settings.appearance.material_system') },
            { value: 'solid', label: t('settings.appearance.material_solid') },
          ]}
          onChange={(v) => void patchSettings(at('appearance.material', v))}
          ariaLabel={t('settings.appearance.material')}
        />
      </Section>

      <Section title={t('settings.appearance.text')}>
        <Field
          label={t('settings.appearance.font_size')}
          description={t('settings.appearance.font_size_desc')}
        >
          <Slider
            value={a.font_size}
            min={12}
            max={18}
            step={1}
            label={t('settings.appearance.font_size')}
            valueLabel={t('settings.unit.px_value', { n: a.font_size })}
            onChange={(v) => queue('appearance.font_size', v)}
            className="st-slider"
            data-testid="appearance-font"
          />
        </Field>
      </Section>

      <Section
        title={t('settings.appearance.panel')}
        description={t('settings.appearance.panel_desc')}
      >
        <Field label={t('settings.appearance.min_width')}>
          <Slider
            value={a.translate_min_width}
            min={320}
            max={480}
            step={10}
            label={t('settings.appearance.min_width')}
            valueLabel={t('settings.unit.px_value', { n: a.translate_min_width })}
            onChange={(v) => {
              queue('appearance.translate_min_width', v)
              if (a.translate_max_width < v) queue('appearance.translate_max_width', v)
            }}
            className="st-slider"
          />
        </Field>
        <Field label={t('settings.appearance.max_width')}>
          <Slider
            value={a.translate_max_width}
            min={440}
            max={720}
            step={10}
            label={t('settings.appearance.max_width')}
            valueLabel={t('settings.unit.px_value', { n: a.translate_max_width })}
            onChange={(v) =>
              queue('appearance.translate_max_width', Math.max(v, a.translate_min_width))
            }
            className="st-slider"
          />
        </Field>
        <Field label={t('settings.appearance.clipboard_width')}>
          <Slider
            value={a.clipboard_width}
            min={360}
            max={560}
            step={10}
            label={t('settings.appearance.clipboard_width')}
            valueLabel={t('settings.unit.px_value', { n: a.clipboard_width })}
            onChange={(v) => queue('appearance.clipboard_width', v)}
            className="st-slider"
          />
        </Field>
        <Field
          label={t('settings.appearance.max_height')}
          description={t('settings.appearance.max_height_desc')}
        >
          <Slider
            value={Math.round(a.max_height_ratio * PERCENT)}
            min={40}
            max={80}
            step={5}
            label={t('settings.appearance.max_height')}
            valueLabel={`${Math.round(a.max_height_ratio * PERCENT)}%`}
            onChange={(v) => queue('appearance.max_height_ratio', v / PERCENT)}
            className="st-slider"
          />
        </Field>
      </Section>

      <Section title={t('settings.appearance.motion')}>
        <SegmentRow<ReduceMotionPref>
          label={t('settings.appearance.reduce_motion')}
          description={t('settings.appearance.reduce_motion_desc')}
          value={a.reduce_motion}
          options={[
            { value: 'system', label: t('settings.appearance.reduce_system') },
            { value: 'on', label: t('settings.appearance.reduce_on') },
            { value: 'off', label: t('settings.appearance.reduce_off') },
          ]}
          onChange={(v) => void patchSettings(at('appearance.reduce_motion', v))}
          ariaLabel={t('settings.appearance.reduce_motion')}
          data-testid="appearance-motion"
        />
        <SwitchRow
          label={t('settings.ocr.fly')}
          description={t('settings.ocr.fly_desc')}
          checked={settings.ocr.fly_animation}
          onChange={(v) => void patchSettings(at('ocr.fly_animation', v))}
          ariaLabel={t('settings.ocr.fly')}
        />
      </Section>
    </Page>
  )
}

/** 预览里的模型名找不到默认翻译模型时用的显示名（mock 与截图里的同一个模型） */
const PREVIEW_MODEL_FALLBACK = 'GPT-5.4 mini'

/**
 * 预览状态行的模型名：与面板状态行同一种显示名——默认翻译模型的 `label`（空白时用 id），与 Rust 的
 * `StreamEvent::Start.model_label` 同一条规则（wisp-llm `start_event`）；还没有配置默认模型时用固定的示例名
 */
export function previewModelLabel(settings: Pick<Settings, 'defaults' | 'providers'>): string {
  const ref = settings.defaults.translate
  const model = ref
    ? settings.providers
        .find((p) => p.id === ref.provider_id)
        ?.models.find((m) => m.id === ref.model_id)
    : undefined
  if (!model) return PREVIEW_MODEL_FALLBACK
  return model.label?.trim() ? model.label : model.id
}

/**
 * 小预览：一张缩小的翻译卡片，用**真实的**面板组件（LangBar、StyleChip、StatusLine、IconButton）以静态属性渲染——
 * 强调色只出现在它在面板里真会用到的地方（焦点环、选中态、主按钮），不会画出面板里并不存在的实心标签 / 按钮；
 * 正文字号、明暗跟随当前设置。方向随界面语言：中文界面「英语 → 中文」配中文译文，英文界面「中文 → 英语」配英文译文。
 * 整块 inert + aria-hidden：只是图示，不可聚焦、不可点
 */
function Preview({ fontSize, model }: { fontSize: number; model: string }) {
  const t = useT()
  const { uiLanguage } = useAppearance()
  const zh = uiLanguage === 'zh-CN'
  const noop = () => undefined
  return (
    <div className="st-preview-stage" aria-hidden="true" inert data-testid="appearance-preview">
      <div className="st-preview-card">
        <div className="st-preview-head">
          <LangBar
            source={zh ? 'en' : 'zh-CN'}
            target={zh ? 'zh-CN' : 'en'}
            onSwap={noop}
            onPickTarget={noop}
            data-testid="appearance-preview-langbar"
          />
          <span className="st-preview-spacer" />
          <StyleChip name={t('styles.natural')} onClick={noop} />
        </div>
        <p className="st-preview-body ui-stream" style={{ fontSize }}>
          {t('settings.appearance.preview_text')}
        </p>
        <div className="st-preview-foot">
          <StatusLine model={model} elapsedMs={800} />
          <IconButton icon={Copy} label={t('common.action.copy')} onClick={noop} />
          <IconButton icon={Pin} label={t('common.action.pin')} onClick={noop} />
        </div>
      </div>
    </div>
  )
}
