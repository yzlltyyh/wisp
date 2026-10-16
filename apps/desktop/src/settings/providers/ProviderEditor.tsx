/**
 * 服务编辑器：名称、协议、区域、base_url（实时最终 URL）、Key（只进钥匙串）、测试连接（分步结果）、
 * 模型列表（拉取 / 合并 / 手动添加）、高级（自定义请求头、quirks、超时）、删除（原地二次确认）。
 *
 * - 已保存的服务：每次改动直接写入（文本框停顿后提交，开关与下拉立即提交；按 id 只写这一个服务，见 persist.ts），
 *   没有「保存」按钮；
 * - 新建的服务（模板生成的草稿）：改动只在草稿里，Key 也是草稿的一部分（keyEntry.ts）；「添加服务」时先写 Key、
 *   再写入 settings 并补上缺失的默认模型。添加后同一个编辑器原地变成「已保存」（不重新挂载、不播切换动画）；
 * - 测试连接 / 拉取模型：已保存的服务先写入输入框里还没保存的 Key（不需要先按回车）；草稿把 Key 直接交给 Rust
 *   （只用于这一次请求，不经钥匙串）；
 * - Esc：焦点在输入框里时只退出输入；否则返回列表。草稿有改动时先在底栏原地确认「放弃这个服务？」；
 *   在编辑器里再点一次侧栏的「模型与服务」同样走这条返回逻辑（context.ts 的 useReselect）；
 * - 滚动后顶部的紧凑标题显示服务名（useSubviewTitle）。
 */
import { ArrowLeft, Plus, RefreshCw, Trash2, X } from 'lucide-react'
import { m } from 'motion/react'
import { useLayoutEffect, useMemo, useRef, useState } from 'react'
import { useT } from '../../shared/i18n/useT'
import { api } from '../../shared/ipc'
import type {
  MaxTokensField,
  ModelEntry,
  Protocol,
  Provider,
  ProviderTemplate,
  ThinkingOff,
} from '../../shared/ipc/types'
import { useKeyScope } from '../../shared/keys/useKeyScope'
import { usePreset } from '../../shared/motion/presets'
import {
  Button,
  Field,
  IconButton,
  NumberField,
  Section,
  Select,
  Switch,
  TextField,
} from '../../shared/ui'
import { useReselect, useSubviewTitle } from '../context'
import { DangerRow, useConfirmKeys } from '../components/Confirm'
import { Disclosure } from '../components/Disclosure'
import { ipcErrorText } from '../lib/errors'
import { useDraft } from '../lib/useDraft'
import { settleWrites } from '../lib/writes'
import { applySnapshot, dropQueued, notify, useRuntime, useSettings, useStyles } from '../store'
import { useModelFetch, useProviderTest } from './hooks'
import { KeyField } from './KeyField'
import { useKeyEntry } from './keyEntry'
import { ProviderLogo } from './logos'
import { ModelTable } from './ModelTable'
import { providerQueueKey } from './persist'
import {
  hostOf,
  placeholderRange,
  PROTOCOLS,
  providerName,
  providerNeedsKey,
  providerSubtitle,
} from './model'
import { isAuthFailure, TestReport, TestVerdictBadge } from './TestReport'
import { UrlPreview } from './UrlPreview'

export interface ProviderEditorProps {
  provider: Provider
  template: ProviderTemplate | null
  isNew: boolean
  /** 写入改动：已保存的服务按 id 保存（provider_save），草稿只改本地 */
  onChange: (next: Provider) => void
  /** 新建：添加到设置；返回是否成功 */
  onAdd?: () => Promise<boolean>
  /** 新建：添加成功、Key 已转为已保存之后（切到已保存服务的视图） */
  onAdded?: () => void
  /** 返回列表（新建时为取消，草稿的 Key 已由编辑器清理） */
  onBack: () => void
  /** 删除成功之后 */
  onDeleted?: () => void
}

const THINKING_OFF: readonly ThinkingOff[] = [
  'none',
  'openai_effort',
  'anthropic_effort',
  'gemini_level',
  'deepseek',
  'qwen',
  'glm',
  'kimi',
  'minimax',
]

const MS_PER_S = 1000
/** 超时的取值范围（秒；Rust validate 的 1 000–600 000 ms） */
const TIMEOUT_S = { min: 1, max: 600 } as const

export function ProviderEditor({
  provider,
  template,
  isNew,
  onChange,
  onAdd,
  onAdded,
  onBack,
  onDeleted,
}: ProviderEditorProps) {
  const t = useT()
  const runtime = useRuntime()
  const settings = useSettings()
  const styles = useStyles()
  const [adding, setAdding] = useState(false)
  const [confirmDiscard, setConfirmDiscard] = useState(false)
  const baseInput = useRef<HTMLInputElement>(null)
  const discardCancel = useRef<HTMLButtonElement>(null)
  const footRef = useRef<HTMLDivElement>(null)
  /** 草稿刚生成时的样子：有没有改动以它为准 */
  const [pristine] = useState(() => JSON.stringify(provider))
  /** 新建且 Base URL 空着（中转站 / 自定义兼容端点）：先填 Base URL，焦点给它而不是 Key（只在打开时决定一次） */
  const [needsBaseUrl] = useState(() => isNew && provider.base_url.trim() === '')
  // layout 阶段聚焦：子视图导航（lib/subview.ts）看到输入框已有焦点，就不再把焦点移到标题
  useLayoutEffect(() => {
    if (needsBaseUrl) baseInput.current?.focus({ preventScroll: true })
  }, [needsBaseUrl])
  const hasKey = runtime?.has_key[provider.id] ?? false
  const keyRequired = providerNeedsKey(provider, template)
  const key = useKeyEntry({ providerId: provider.id, hasKey, draft: isNew })
  const test = useProviderTest(provider)
  const fetcher = useModelFetch(provider, (models) => onChange({ ...provider, models }))
  const fade = usePreset('footerIn')

  const name = useDraft(provider.name, (v) => onChange({ ...provider, name: v }))
  const baseUrl = useDraft(provider.base_url, (v) => onChange({ ...provider, base_url: v.trim() }))

  const referenced = useMemo(() => {
    const ids = new Set<string>()
    const d = settings?.defaults
    if (d?.translate?.provider_id === provider.id) ids.add(d.translate.model_id)
    if (d?.ocr?.provider_id === provider.id) ids.add(d.ocr.model_id)
    for (const s of styles)
      if (s.model_ref?.provider_id === provider.id) ids.add(s.model_ref.model_id)
    return ids
  }, [settings?.defaults, styles, provider.id])

  /**
   * 测试 / 拉取之前：提交文本草稿；已保存的服务把输入框里还没保存的 Key 先写入（失败就地报错并中止）。
   * 返回交给 Rust 的 Key（草稿 = 输入框里的 Key；已保存 = null，Rust 从钥匙串读）；中止时为 undefined
   */
  const prepare = async (): Promise<string | null | undefined> => {
    name.flush()
    baseUrl.flush()
    if (key.draft) return key.probeKey()
    return (await key.commit()) ? null : undefined
  }

  const runTest = async () => {
    const probe = await prepare()
    if (probe === undefined) return
    const run = await test.start(probe)
    // 第一次测试成功、还没有模型时，直接采用测试顺带返回的模型列表（之后由用户手动刷新）：不再另发一次 GET /models
    if (
      run.phase === 'done' &&
      run.report.overall !== 'failed' &&
      run.report.fetched_models.length > 0 &&
      provider.models.length === 0
    ) {
      fetcher.apply(run.report.fetched_models)
    }
  }

  const fetchModels = async () => {
    const probe = await prepare()
    if (probe === undefined) return
    await fetcher.fetchModels(probe)
  }

  const remove = async () => {
    // 先丢掉这个服务还没发出的改动（删除之后再发只会得到「已删除」的报错），再等其他在途写入落盘
    dropQueued(providerQueueKey(provider.id))
    await settleWrites()
    try {
      applySnapshot(await api.providerDelete(provider.id))
      notify(t('settings.providers.deleted', { name: providerName(provider) }), 'success')
      onDeleted?.()
    } catch (error) {
      notify(ipcErrorText(error, 'provider_delete'), 'danger')
    }
  }

  const regions = template?.region_variants ?? []
  const region = regions.find((r) => r.base_url === provider.base_url)?.base_url ?? null
  const chooseRegion = (url: string) => {
    onChange({ ...provider, base_url: url })
    // 业务空间端点带 {WorkspaceId} 占位：聚焦并选中占位片段，直接输入即可替换
    const range = placeholderRange(url)
    if (range) {
      requestAnimationFrame(() => {
        const el = baseInput.current
        el?.focus()
        el?.setSelectionRange(range[0], range[1])
      })
    }
  }

  const add = async () => {
    name.flush()
    baseUrl.flush()
    setAdding(true)
    try {
      if (!(await key.commit())) return
      if (await onAdd?.()) {
        // 先把 Key 记为已保存（之后卸载 / 变成已保存的服务都不会再删它），再切视图
        key.markAdded()
        onAdded?.()
      }
    } finally {
      setAdding(false)
    }
  }

  /** 取消新建：回到选模板（草稿的 Key 从没写进凭据存储，不用清理） */
  const cancelNew = () => {
    setConfirmDiscard(false)
    onBack()
  }

  const draftDirty =
    isNew &&
    (key.dirty ||
      JSON.stringify({ ...provider, name: name.value, base_url: baseUrl.value.trim() }) !==
        pristine)

  /** 返回：草稿有改动时原地确认，新建的草稿直接放弃，已保存的服务回到列表 */
  const requestBack = () => {
    if (isNew && draftDirty) setConfirmDiscard(true)
    else if (isNew) cancelNew()
    else onBack()
  }
  // Esc：输入框里先退出输入；否则返回。确认条打开时由它的 overlay 作用域先处理
  useKeyScope('provider-editor', [
    {
      combo: 'Escape',
      run: (_e, ctx) => {
        if (ctx.inInput) {
          const el = document.activeElement
          if (el instanceof HTMLElement) el.blur()
          return true
        }
        requestBack()
        return true
      },
    },
  ])
  // 再点一次侧栏当前分组：与返回相同
  useReselect(requestBack)
  useConfirmKeys(confirmDiscard, () => setConfirmDiscard(false), {
    cancel: discardCancel,
    container: footRef,
  })

  const display = providerName({ ...provider, name: name.value })
  useSubviewTitle(display)
  const host = hostOf(baseUrl.value)
  const openAiLike = provider.protocol === 'openai_chat' || provider.protocol === 'openai_responses'
  const rejected = !test.stale && isAuthFailure(test.run)
  const keyPresent = key.present || !keyRequired

  return (
    <div className="st-editor" data-testid="provider-editor" data-new={isNew || undefined}>
      <div className="st-editor-head">
        <IconButton
          icon={isNew ? X : ArrowLeft}
          label={isNew ? t('common.action.cancel') : t('settings.providers.back')}
          onClick={isNew ? cancelNew : onBack}
          data-testid="provider-back"
        />
        <ProviderLogo templateId={provider.template_id} name={display} size={20} />
        <span className="st-editor-title">
          <h2 tabIndex={-1} data-subview-title>
            {display}
          </h2>
          <span>{providerSubtitle(display, template, host)}</span>
        </span>
        {test.stale ? null : <TestVerdictBadge run={test.run} />}
      </div>

      <Section title={t('settings.providers.connection')}>
        <Field label={t('settings.providers.name')}>
          <TextField
            value={name.value}
            onChange={name.set}
            onBlur={name.flush}
            onEnter={name.flush}
            placeholder={provider.id}
            aria-label={t('settings.providers.name')}
            className="st-input-md"
            data-testid="provider-name"
          />
        </Field>
        <Field
          label={t('settings.providers.protocol')}
          description={t(`settings.providers.protocol_desc.${provider.protocol}`)}
        >
          <Select<Protocol>
            label={t('settings.providers.protocol')}
            value={provider.protocol}
            options={PROTOCOLS.map((p) => ({
              value: p,
              label: t(`settings.providers.protocols.${p}`),
            }))}
            onChange={(v) => onChange({ ...provider, protocol: v })}
            className="st-select"
            data-testid="provider-protocol"
          />
        </Field>
        {regions.length > 0 ? (
          <Field label={t('settings.providers.region_label')}>
            <Select<string>
              label={t('settings.providers.region_label')}
              value={region}
              placeholder={t('settings.providers.region_custom')}
              options={regions.map((r) => ({
                value: r.base_url,
                label: t(r.label_key as 'settings.providers.region.cn'),
              }))}
              onChange={chooseRegion}
              className="st-select"
              data-testid="provider-region"
            />
          </Field>
        ) : null}
        {/* 「以 # 结尾…」只在需要时由 UrlPreview 的提示说一次（自动补了版本段 / 已按原样请求），不在说明里重复 */}
        <Field label={t('settings.providers.base_url')} stacked>
          <TextField
            ref={baseInput}
            value={baseUrl.value}
            onChange={baseUrl.set}
            onBlur={baseUrl.flush}
            onEnter={baseUrl.flush}
            placeholder={t('settings.providers.base_url_placeholder')}
            aria-label={t('settings.providers.base_url')}
            className="st-mono"
            invalid={placeholderRange(baseUrl.value) !== null}
            data-testid="provider-base-url"
          />
          <UrlPreview
            protocol={provider.protocol}
            baseUrl={baseUrl.value}
            modelId={provider.models.find((x) => x.enabled)?.id ?? null}
          />
        </Field>
        <Field
          label={t('settings.providers.key')}
          description={
            keyRequired
              ? t('settings.providers.key_desc')
              : t('settings.providers.key_desc_optional')
          }
          stacked
        >
          <KeyField
            entry={key}
            required={keyRequired}
            keychain={runtime?.keychain ?? 'available'}
            focusOnMount={isNew && keyRequired && !needsBaseUrl}
            draftHint={t('settings.providers.key_draft_hint')}
            onSubmit={() => void runTest()}
            rejected={rejected}
          />
        </Field>
        <Field
          label={t('settings.providers.test')}
          description={t('settings.providers.test_desc')}
          stacked
        >
          <div className="st-test-bar">
            <Button
              size="sm"
              variant={
                keyPresent &&
                (test.stale || !(test.run.phase === 'done' && test.run.report.overall === 'ok'))
                  ? 'primary'
                  : 'secondary'
              }
              loading={test.run.phase === 'running' || key.busy}
              disabled={!baseUrl.value.trim()}
              onClick={() => void runTest()}
              data-testid="provider-test"
            >
              {test.run.phase === 'idle'
                ? t('settings.providers.test_run')
                : t('settings.providers.test_again')}
            </Button>
          </div>
          <TestReport
            run={test.run}
            stale={test.stale}
            providerName={display}
            onRetry={() => void runTest()}
          />
        </Field>
      </Section>

      <Section
        title={t('settings.models.title')}
        description={t('settings.models.description')}
        action={
          <Button
            size="sm"
            variant="ghost"
            icon={RefreshCw}
            loading={fetcher.loading}
            disabled={!baseUrl.value.trim()}
            onClick={() => void fetchModels()}
            data-testid="provider-fetch-models"
          >
            {t('settings.models.fetch')}
          </Button>
        }
      >
        {fetcher.error ? (
          // 拉取失败（常见：钥匙串授权框里点了「拒绝」）：原因旁边直接给「重试」
          <div className="st-card-error-row" role="alert" data-testid="provider-fetch-error">
            <p className="st-card-error">{fetcher.error}</p>
            <Button
              size="sm"
              variant="ghost"
              icon={RefreshCw}
              loading={fetcher.loading}
              onClick={() => void fetchModels()}
            >
              {t('errors.action.retry')}
            </Button>
          </div>
        ) : null}
        <ModelTable
          models={provider.models}
          showEffort={provider.quirks.thinking_off === 'openai_effort'}
          referenced={referenced}
          onChange={(models: ModelEntry[]) => onChange({ ...provider, models })}
          onResetCaps={
            isNew
              ? undefined
              : (modelId) =>
                  void api
                    .modelCapsReset(provider.id, modelId)
                    .then(() => notify(t('settings.models.caps_reset_done'), 'success'))
                    .catch((e) => notify(ipcErrorText(e, 'model_caps_reset'), 'danger'))
          }
        />
      </Section>

      <Section>
        <Disclosure title={t('settings.providers.advanced')} data-testid="provider-advanced">
          <HeadersEditor provider={provider} onChange={onChange} />
          <Field
            label={t('settings.providers.thinking_off')}
            description={t('settings.providers.thinking_off_desc')}
          >
            <Select<ThinkingOff>
              label={t('settings.providers.thinking_off')}
              value={provider.quirks.thinking_off}
              options={THINKING_OFF.map((v) => ({
                value: v,
                label: t(`settings.providers.thinking.${v}`),
              }))}
              onChange={(v) =>
                onChange({ ...provider, quirks: { ...provider.quirks, thinking_off: v } })
              }
              className="st-select"
            />
          </Field>
          <Field
            label={t('settings.providers.max_tokens_field')}
            description={t('settings.providers.max_tokens_field_desc')}
          >
            <Select<'auto' | MaxTokensField>
              label={t('settings.providers.max_tokens_field')}
              value={provider.quirks.max_tokens_field ?? 'auto'}
              options={[
                { value: 'auto', label: t('settings.providers.auto') },
                // i18n-ignore：请求参数名原样显示
                { value: 'max_completion_tokens', label: 'max_completion_tokens' },
                // i18n-ignore：请求参数名原样显示
                { value: 'max_tokens', label: 'max_tokens' },
              ]}
              onChange={(v) =>
                onChange({
                  ...provider,
                  quirks: { ...provider.quirks, max_tokens_field: v === 'auto' ? null : v },
                })
              }
              className="st-select"
            />
          </Field>
          <Field
            label={t('settings.providers.merge_system')}
            description={t('settings.providers.merge_system_desc')}
          >
            <Switch
              checked={provider.quirks.merge_system_into_user}
              aria-label={t('settings.providers.merge_system')}
              onChange={(v) =>
                onChange({ ...provider, quirks: { ...provider.quirks, merge_system_into_user: v } })
              }
            />
          </Field>
          {openAiLike ? (
            <Field
              label={t('settings.providers.strip_think')}
              description={t('settings.providers.strip_think_desc')}
            >
              <Switch
                checked={provider.quirks.strip_think_tags}
                aria-label={t('settings.providers.strip_think')}
                onChange={(v) =>
                  onChange({ ...provider, quirks: { ...provider.quirks, strip_think_tags: v } })
                }
              />
            </Field>
          ) : null}
          {provider.protocol === 'anthropic_messages' ? (
            <Field
              label={t('settings.providers.dual_auth')}
              description={t('settings.providers.dual_auth_desc')}
            >
              <Select<'auto' | 'on' | 'off'>
                label={t('settings.providers.dual_auth')}
                value={
                  provider.quirks.dual_auth_header === null
                    ? 'auto'
                    : provider.quirks.dual_auth_header
                      ? 'on'
                      : 'off'
                }
                options={[
                  { value: 'auto', label: t('settings.providers.auto') },
                  { value: 'on', label: t('common.state.on') },
                  { value: 'off', label: t('common.state.off') },
                ]}
                onChange={(v) =>
                  onChange({
                    ...provider,
                    quirks: {
                      ...provider.quirks,
                      dual_auth_header: v === 'auto' ? null : v === 'on',
                    },
                  })
                }
                className="st-select"
              />
            </Field>
          ) : null}
          {provider.protocol === 'gemini' ? (
            <Field
              label={t('settings.providers.gemini_key_query')}
              description={t('settings.providers.gemini_key_query_desc')}
            >
              <Switch
                checked={provider.quirks.gemini_key_in_query}
                aria-label={t('settings.providers.gemini_key_query')}
                onChange={(v) =>
                  onChange({ ...provider, quirks: { ...provider.quirks, gemini_key_in_query: v } })
                }
              />
            </Field>
          ) : null}
          {(['connect_ms', 'first_token_ms', 'idle_ms'] as const).map((k) => (
            <Field key={k} label={t(`settings.providers.timeout.${k}`)}>
              <NumberField
                value={Math.round(provider.timeouts[k] / MS_PER_S)}
                min={TIMEOUT_S.min}
                max={TIMEOUT_S.max}
                unit={t('settings.unit.seconds')}
                aria-label={t(`settings.providers.timeout.${k}`)}
                className="st-number"
                onChange={(v) =>
                  onChange({ ...provider, timeouts: { ...provider.timeouts, [k]: v * MS_PER_S } })
                }
              />
            </Field>
          ))}
        </Disclosure>
      </Section>

      {isNew ? (
        <div
          ref={footRef}
          className="st-editor-foot"
          data-confirm={confirmDiscard || undefined}
          data-testid="provider-new-foot"
        >
          {confirmDiscard ? (
            <m.span
              key="discard"
              className="st-foot-confirm"
              role="alert"
              data-testid="provider-discard-confirm"
              {...fade}
            >
              <span className="st-foot-confirm-text">
                {t('settings.providers.discard_confirm')}
              </span>
              <Button ref={discardCancel} onClick={() => setConfirmDiscard(false)}>
                {t('settings.providers.keep_editing')}
              </Button>
              <Button variant="danger" onClick={cancelNew} data-testid="provider-discard-ok">
                {t('settings.providers.discard')}
              </Button>
            </m.span>
          ) : (
            <>
              <Button variant="ghost" onClick={cancelNew} data-testid="provider-cancel">
                {t('common.action.cancel')}
              </Button>
              <span className="st-grow" />
              <Button
                variant="primary"
                size="md"
                icon={Plus}
                loading={adding}
                disabled={!baseUrl.value.trim()}
                onClick={() => void add()}
                data-testid="provider-add"
              >
                {t('settings.providers.add_this')}
              </Button>
            </>
          )}
        </div>
      ) : (
        <Section>
          <DangerRow
            label={t('settings.providers.delete')}
            description={t('settings.providers.delete_desc')}
            action={t('settings.providers.delete_action')}
            confirm={t('settings.providers.delete_confirm', { name: display })}
            confirmLabel={t('common.action.delete')}
            icon={Trash2}
            onConfirm={() => void remove()}
            data-testid="provider-delete"
          />
        </Section>
      )}
    </div>
  )
}

/** 自定义请求头：名字必须是合法 token（RFC 9110），值不能含换行；不合法的行标红、不写入 */
interface HeaderRow {
  id: number
  name: string
  value: string
}

let headerSeq = 0
const toHeaderRows = (headers: Record<string, string>): HeaderRow[] =>
  Object.entries(headers).map(([name, value]) => ({ id: ++headerSeq, name, value }))

function HeadersEditor({
  provider,
  onChange,
}: {
  provider: Provider
  onChange: (p: Provider) => void
}) {
  const t = useT()
  const [rows, setRows] = useState<HeaderRow[]>(() => toHeaderRows(provider.headers))
  // 切换 Provider 时在渲染中重置
  const [forId, setForId] = useState(provider.id)
  if (forId !== provider.id) {
    setForId(provider.id)
    setRows(toHeaderRows(provider.headers))
  }
  const validName = (n: string) => /^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/.test(n)
  const validValue = (v: string) => !/[\r\n\0]/.test(v)
  const commit = (next: HeaderRow[]) => {
    setRows(next)
    const ok = next.filter((r) => validName(r.name) && validValue(r.value))
    const headers = Object.fromEntries(ok.map((r) => [r.name, r.value]))
    if (JSON.stringify(headers) !== JSON.stringify(provider.headers))
      onChange({ ...provider, headers })
  }
  return (
    <Field
      label={t('settings.providers.headers')}
      description={t('settings.providers.headers_desc')}
      stacked
    >
      <div className="st-headers" data-testid="provider-headers">
        {rows.map(({ id, name: n, value: v }) => (
          <div key={id} className="st-header-row">
            <TextField
              value={n}
              onChange={(next) => commit(rows.map((r) => (r.id === id ? { ...r, name: next } : r)))}
              placeholder={t('settings.providers.header_name')}
              aria-label={t('settings.providers.header_name')}
              invalid={n !== '' && !validName(n)}
              className="st-mono st-header-name"
            />
            <TextField
              value={v}
              onChange={(next) =>
                commit(rows.map((r) => (r.id === id ? { ...r, value: next } : r)))
              }
              placeholder={t('settings.providers.header_value')}
              aria-label={t('settings.providers.header_value')}
              invalid={!validValue(v)}
              className="st-mono st-grow"
            />
            <IconButton
              icon={X}
              size="sm"
              label={t('common.action.remove')}
              onClick={() => commit(rows.filter((r) => r.id !== id))}
            />
          </div>
        ))}
        <Button
          size="sm"
          variant="ghost"
          icon={Plus}
          onClick={() => setRows([...rows, { id: ++headerSeq, name: '', value: '' }])}
        >
          {t('settings.providers.header_add')}
        </Button>
      </div>
    </Field>
  )
}
