/**
 * 风格编辑器：
 * - 名称（自定义）、输出格式、词典式开关、temperature（「发送 temperature」开关 + 打开后展开的滑块；关掉 = null 不发送）、
 *   绑定模型（失效时回退到默认模型并提示）、风格快捷键（同一个录制器与冲突检测，与 5 个动作互斥；含义是「用这个风格执行
 *   划词翻译」）；
 * - system 模板（可用 {{source_lang}} {{target_lang}}；点变量插入到光标处；不认识的变量提示会原样保留）与结尾指令
 *   （留空时用「自然」风格的）；装配时自动拼上硬规则与术语，用户去不掉（预览可见）；
 * - 「预览最终提示词」：`style_preview`（按当前语言与术语表装配一次）；
 * - 内置风格可以改并「还原默认」（删除 override）；自定义风格可以删除——都在那一行原地二次确认。
 * 改动即时保存：同一个风格的 `style_save` 串行、以最近一次服务器返回的风格为底合并字段（lib/styleSave.ts），文本停顿后提交。
 * Esc：焦点在输入框里时只退出输入，否则返回风格列表。
 */
import { ArrowLeft, Eye, RotateCcw, Trash2, TriangleAlert } from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useRef, useState } from 'react'
import { usePlatform } from '../../shared/hooks/usePlatform'
import { useT } from '../../shared/i18n/useT'
import { isOcrService } from '../../shared/offline/catalog'
import { api } from '../../shared/ipc'
import type {
  HotkeyStatus,
  OutputFormat,
  PromptPreview,
  Provider,
  Style,
} from '../../shared/ipc/types'
import { formatHotkey } from '../../shared/keys/formatHotkey'
import { useKeyScope } from '../../shared/keys/useKeyScope'
import { usePreset } from '../../shared/motion/presets'
import {
  Badge,
  Button,
  Field,
  HotkeyRecorder,
  Icon,
  IconButton,
  Section,
  SegmentedControl,
  Select,
  Slider,
  Switch,
  TextArea,
  TextField,
} from '../../shared/ui'
import { useReselect, useSubviewTitle } from '../context'
import { DangerRow } from '../components/Confirm'
import { ipcErrorText } from '../lib/errors'
import { formatList } from '../lib/format'
import { applyServerStyles, saveStyleFields } from '../lib/styleSave'
import { modelRefState, STYLE_VARIABLES, styleName, unknownVariables } from '../lib/styles'
import { useDraft } from '../lib/useDraft'
import { notify, setHotkeys } from '../store'
import { parseRefKey, providerName, refKey } from '../providers/model'

/** 自定义风格默认 temperature 由 Rust 决定（wisp_types::CUSTOM_STYLE_TEMPERATURE）；这里只是滑块打开时的初值 */
const TEMPERATURE_STEP = 0.05
const PREVIEW_SAMPLE_MAX = 2000
/** 滑块停顿多久后写入（不是动效时长） */
const TEMPERATURE_COMMIT_MS = 250
/** 预览里的消息角色名（协议字段名，原样显示） */
const ROLE_SYSTEM = 'system'
const ROLE_USER = 'user'

export function StyleEditor({
  style,
  natural,
  providers,
  hotkey,
  onBack,
}: {
  style: Style
  /** 「自然」风格（结尾指令的占位） */
  natural: Style | null
  providers: readonly Provider[]
  hotkey: HotkeyStatus | undefined
  onBack: () => void
}) {
  const t = useT()
  const os = usePlatform()
  const [preview, setPreview] = useState<PromptPreview | null>(null)
  const [previewing, setPreviewing] = useState(false)
  const [sample, setSample] = useState(() => t('settings.styles.preview_sample'))
  const system = useRef<HTMLTextAreaElement>(null)
  const expand = usePreset('sectionExpand')

  const save = (patch: Partial<Style>) =>
    void saveStyleFields(style.id, patch, (error) =>
      notify(ipcErrorText(error, 'style_save'), 'danger'),
    )

  // 名称清空或带首尾空格：保存规范化后的值，并把输入框改成它（返回值，见 useDraft；外部值没变时草稿不会自己跟上）
  const name = useDraft(style.name, (v) => {
    const next = v.trim() || style.name
    if (next !== style.name) save({ name: next })
    return next
  })
  const template = useDraft(style.system_template, (v) => save({ system_template: v }))
  const tail = useDraft(style.tail_instruction, (v) => save({ tail_instruction: v }))
  const temperature = useDraft(style.temperature, (v) => save({ temperature: v }), {
    delayMs: TEMPERATURE_COMMIT_MS,
  })

  // 再点一次侧栏的「翻译」：回到风格列表；滚动后的紧凑标题显示风格名
  useReselect(onBack)
  useSubviewTitle(style.builtin ? styleName(style) : name.value.trim() || styleName(style))

  // Esc：输入框里先退出输入，否则返回风格列表（确认条打开时由它先处理）
  useKeyScope('style-editor', [
    {
      combo: 'Escape',
      run: (_e, ctx) => {
        if (ctx.inInput) {
          const el = document.activeElement
          if (el instanceof HTMLElement) el.blur()
        } else onBack()
        return true
      },
    },
  ])

  const unknown = unknownVariables(template.value)
  const ref = style.model_ref ? modelRefState(style.model_ref, providers) : null
  const modelOptions = [
    { value: '', label: t('settings.styles.model_default') },
    // 文字识别服务（PaddleOCR）不能翻译，不能绑定给风格
    ...providers
      .filter((p) => !isOcrService(p))
      .flatMap((p) =>
        p.models
          .filter((mm) => mm.enabled)
          .map((mm) => ({
            value: refKey({ provider_id: p.id, model_id: mm.id }) as string,
            label: mm.label?.trim() || mm.id,
            hint: providerName(p),
          })),
      ),
  ]
  // 绑定的模型已失效：仍然把它显示出来（标红），而不是假装没绑定
  if (style.model_ref && ref !== 'ok') {
    modelOptions.push({
      value: refKey(style.model_ref) as string,
      label: style.model_ref.model_id,
      hint: t('settings.styles.model_unavailable'),
    })
  }

  const insertVariable = (v: string) => {
    const el = system.current
    const token = `{{${v}}}`
    const text = template.value
    const start = el?.selectionStart ?? text.length
    const end = el?.selectionEnd ?? text.length
    template.set(text.slice(0, start) + token + text.slice(end))
    requestAnimationFrame(() => {
      el?.focus()
      el?.setSelectionRange(start + token.length, start + token.length)
    })
  }

  const runPreview = async () => {
    template.flush()
    tail.flush()
    setPreviewing(true)
    try {
      setPreview(
        await api.stylePreview(
          { ...style, system_template: template.value, tail_instruction: tail.value },
          sample.slice(0, PREVIEW_SAMPLE_MAX),
        ),
      )
    } catch (error) {
      notify(ipcErrorText(error, 'style_preview'), 'danger')
    } finally {
      setPreviewing(false)
    }
  }

  const reset = async () => {
    try {
      applyServerStyles(await api.styleReset(style.id))
      notify(t('settings.styles.reset_done', { name: styleName(style) }), 'success')
    } catch (error) {
      notify(ipcErrorText(error, 'style_reset'), 'danger')
    }
  }
  const remove = async () => {
    try {
      applyServerStyles(await api.styleDelete(style.id))
      notify(t('settings.styles.deleted', { name: styleName(style) }), 'success')
      onBack()
    } catch (error) {
      notify(ipcErrorText(error, 'style_delete'), 'danger')
    }
  }

  const setHotkey = async (combo: string | null, force: boolean) => {
    try {
      setHotkeys(await api.hotkeySet({ kind: 'style', style_id: style.id }, combo ?? 'none', force))
      // 风格列表里的 hotkey 字段随之更新（Rust 写入 styles.json；先在本地反映出来）
      applyServerStyles((await api.stylesList()) ?? [])
    } catch (error) {
      // 拒绝时 Rust 下发 `hotkeys.conflict.*` 的 key；带参数的文案在这里补上组合与风格名（同快捷键页）
      notify(
        ipcErrorText(error, 'hotkey_set', {
          combo: combo ? formatHotkey(combo, os) : '',
          action: styleName(style),
        }),
        'danger',
      )
    }
  }

  const sending = temperature.value !== null

  return (
    <div className="st-editor" data-testid="style-editor">
      <div className="st-editor-head">
        <IconButton
          icon={ArrowLeft}
          label={t('settings.styles.back')}
          onClick={onBack}
          data-testid="style-back"
        />
        <span className="st-editor-title">
          <h2 tabIndex={-1} data-subview-title>
            {style.builtin ? styleName(style) : name.value || styleName(style)}
          </h2>
          <span>
            {style.builtin
              ? t(`styles.desc.${style.id}` as 'styles.desc.natural')
              : t('styles.custom')}
          </span>
        </span>
        {style.builtin && style.modified ? (
          <Badge tone="neutral">{t('settings.styles.modified')}</Badge>
        ) : null}
      </div>

      <Section title={t('settings.styles.basics')}>
        {!style.builtin ? (
          <Field label={t('settings.styles.name')}>
            <TextField
              value={name.value}
              onChange={name.set}
              onBlur={name.flush}
              onEnter={name.flush}
              aria-label={t('settings.styles.name')}
              className="st-input-md"
              data-testid="style-name"
            />
          </Field>
        ) : null}
        <Field label={t('settings.styles.output')} description={t('settings.styles.output_desc')}>
          <SegmentedControl<OutputFormat>
            label={t('settings.styles.output')}
            value={style.output}
            options={[
              { value: 'plain_text', label: t('settings.styles.output_plain') },
              { value: 'markdown_lite', label: t('settings.styles.output_markdown') },
            ]}
            onChange={(v) => save({ output: v })}
            className="st-segmented"
          />
        </Field>
        <Field
          label={t('settings.styles.dictionary')}
          description={t('settings.styles.dictionary_desc')}
        >
          <Switch
            checked={style.dictionary_mode}
            aria-label={t('settings.styles.dictionary')}
            onChange={(v) => save({ dictionary_mode: v })}
            data-testid="style-dictionary"
          />
        </Field>
        <Field
          label={t('settings.styles.temperature_send')}
          description={
            sending ? t('settings.styles.temperature_desc') : t('settings.styles.temperature_off')
          }
        >
          <Switch
            checked={sending}
            aria-label={t('settings.styles.temperature_send')}
            onChange={(v) => {
              // 滑块停顿中的值不能在之后提交、把刚关掉的开关又打开
              temperature.discard()
              // 打开时从内置「自然」风格的取值开始（与自定义风格的默认值同源，均由 Rust 提供）
              const initial = natural?.temperature ?? 0
              save({ temperature: v ? (style.temperature ?? initial) : null })
            }}
            data-testid="style-temperature-switch"
          />
        </Field>
        <AnimatePresence initial={false}>
          {temperature.value !== null ? (
            <m.div key="temperature" className="st-clip st-subrows" {...expand}>
              <Field label={t('settings.styles.temperature')}>
                <Slider
                  value={temperature.value}
                  min={0}
                  max={1}
                  step={TEMPERATURE_STEP}
                  label={t('settings.styles.temperature')}
                  valueLabel={temperature.value.toFixed(2)}
                  onChange={temperature.set}
                  className="st-slider"
                  data-testid="style-temperature"
                />
              </Field>
            </m.div>
          ) : null}
        </AnimatePresence>
        <Field
          label={t('settings.styles.model')}
          description={t('settings.styles.model_desc')}
          message={ref && ref !== 'ok' ? t(`settings.styles.model_${ref}`) : null}
          messageTone="warning"
        >
          <Select<string>
            label={t('settings.styles.model')}
            value={refKey(style.model_ref) ?? ''}
            options={modelOptions}
            invalid={ref !== null && ref !== 'ok'}
            onChange={(v) => save({ model_ref: v ? parseRefKey(v) : null })}
            className="st-select st-select-wide"
            data-testid="style-model"
          />
        </Field>
        <Field label={t('settings.styles.hotkey')} description={t('settings.styles.hotkey_desc')}>
          <HotkeyRecorder
            target={{ kind: 'style', style_id: style.id }}
            value={hotkey?.effective ?? style.hotkey}
            label={t('hotkeys.recorder.label', { action: styleName(style) })}
            onChange={(combo, force) => void setHotkey(combo, force)}
            data-testid="style-hotkey"
          />
        </Field>
      </Section>

      <Section title={t('settings.styles.prompt')} description={t('settings.styles.prompt_desc')}>
        <Field label={t('settings.styles.system')} stacked>
          <TextArea
            ref={system}
            value={template.value}
            onChange={template.set}
            onBlur={template.flush}
            minRows={6}
            maxRows={16}
            aria-label={t('settings.styles.system')}
            className="st-mono st-prompt"
            data-testid="style-system"
          />
          <div className="st-vars">
            <span className="st-vars-label">{t('settings.styles.variables')}</span>
            {STYLE_VARIABLES.map((v) => (
              <button
                key={v}
                type="button"
                className="st-var"
                onClick={() => insertVariable(v)}
                title={t(`settings.styles.var.${v}`)}
              >
                {`{{${v}}}`}
              </button>
            ))}
          </div>
          {unknown.length > 0 ? (
            <p className="st-field-hint" data-tone="warning" data-testid="style-unknown-vars">
              <Icon icon={TriangleAlert} size={12} />
              {t('settings.styles.unknown_vars', {
                count: unknown.length,
                vars: formatList(unknown.map((v) => `{{${v}}}`)),
              })}
            </p>
          ) : null}
        </Field>
        <Field
          label={t('settings.styles.tail')}
          description={t('settings.styles.tail_desc')}
          stacked
        >
          <TextArea
            value={tail.value}
            onChange={tail.set}
            onBlur={tail.flush}
            minRows={2}
            maxRows={6}
            placeholder={natural?.tail_instruction || undefined}
            aria-label={t('settings.styles.tail')}
            className="st-mono st-prompt"
            data-testid="style-tail"
          />
        </Field>
        <Field
          label={t('settings.styles.preview')}
          description={t('settings.styles.preview_desc')}
          stacked
        >
          <div className="st-preview-bar">
            <TextField
              value={sample}
              onChange={setSample}
              onEnter={() => void runPreview()}
              aria-label={t('settings.styles.preview_sample_label')}
              className="st-grow"
            />
            <Button
              size="sm"
              icon={Eye}
              loading={previewing}
              onClick={() => void runPreview()}
              data-testid="style-preview"
            >
              {t('settings.styles.preview_run')}
            </Button>
          </div>
          <AnimatePresence initial={false}>
            {preview ? (
              <m.div key="preview" className="st-clip" {...expand}>
                <div className="st-preview" data-testid="style-preview-result">
                  {/* i18n-ignore：消息角色名（system / user）原样显示 */}
                  <span className="st-preview-role">{ROLE_SYSTEM}</span>
                  <pre className="st-preview-text">{preview.system}</pre>
                  <span className="st-preview-role">{ROLE_USER}</span>
                  <pre className="st-preview-text">{preview.user}</pre>
                </div>
              </m.div>
            ) : null}
          </AnimatePresence>
        </Field>
      </Section>

      <Section>
        {style.builtin ? (
          <DangerRow
            label={t('settings.styles.reset')}
            description={t('settings.styles.reset_desc')}
            action={t('settings.styles.reset')}
            confirm={t('settings.styles.reset_confirm', { name: styleName(style) })}
            icon={RotateCcw}
            tone="default"
            disabled={!style.modified}
            onConfirm={() => void reset()}
            data-testid="style-reset"
          />
        ) : (
          <DangerRow
            label={t('settings.styles.delete')}
            description={t('settings.styles.delete_desc')}
            action={t('settings.styles.delete_action')}
            confirm={t('settings.styles.delete_confirm', { name: styleName(style) })}
            confirmLabel={t('common.action.delete')}
            icon={Trash2}
            onConfirm={() => void remove()}
            data-testid="style-delete"
          />
        )}
      </Section>
    </div>
  )
}
