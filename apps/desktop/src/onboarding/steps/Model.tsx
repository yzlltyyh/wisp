/**
 * 引导第 2 步：接入模型。
 *
 * 选模板（4 张大卡片：OpenAI——默认 Responses、可切换为 Chat；Anthropic；Gemini；「其他服务」展开为其余 12 个模板）→
 * base_url（预填，有区域变体时可选区域）+ Key → [测试连接]（分步结果；无效 Key 时 Key 字段就地提示并滚入视野）→
 * 自动拉取模型列表 → 分别选择翻译与 OCR 的默认模型（OCR 只列视觉模型）→「保存并继续」写入 Key、Provider 与 defaults。
 * 已经接入过服务时显示摘要，可以再添加一个。整步都可以跳过。
 *
 * - 草稿状态在引导窗口里（steps/modelDraft.ts）：离开这一步（上一步 / 跳过这一步 / 步骤条）再回来，填过的内容、测试结果、
 *   选好的默认模型都还在；再点同一张卡片也回到这份草稿。为测试写入过、最后没保存的 Key 在换模板、离开窗口时删掉；
 * - 只有一个返回入口：表单与「其他服务」阶段，底栏的「上一步」回到卡片（表单从「其他服务」进来的回到那里），
 *   正文里不再另放「← 返回」；
 * - 重新测试不会改掉用户选好的默认模型：拉到模型后只补上还没选、或已经不在启用列表里的那一项；连接（协议 + base_url）
 *   没变、已经有模型时不再重新拉取；
 * - 拉到模型后默认模型卡片立即展开，展开过程中滚动跟随（lib/reveal.ts），不等弹簧结束；
 * - 阶段切换只有新阶段的进入动画（stepSwitch 的 initial / animate），旧阶段立即卸载；切换后焦点交给新阶段的标题。
 */
import { Check, ChevronRight, Plus } from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useEffect, useRef, useState, type ReactNode } from 'react'
import { useT } from '../../shared/i18n/useT'
import { api } from '../../shared/ipc'
import { isUsableModel } from '../../shared/models'
import type { ModelRef, Provider, ProviderTemplate } from '../../shared/ipc/types'
import { adaptPreset, presets, usePreset } from '../../shared/motion/presets'
import { useReducedMotionPref } from '../../shared/motion/useReducedMotionPref'
import { Button, Field, Icon, SegmentedControl, Select, Skeleton, TextField } from '../../shared/ui'
import { ipcErrorText } from '../../settings/lib/errors'
import { formatList } from '../../settings/lib/format'
import { revealGrowing } from '../../settings/lib/reveal'
import { DefaultModelFields } from '../../settings/providers/DefaultModels'
import { probedModel, useModelFetch, useProviderTest } from '../../settings/providers/hooks'
import { KeyField } from '../../settings/providers/KeyField'
import type { KeyEntry } from '../../settings/providers/keyEntry'
import {
  fillDefaults,
  hostOf,
  isFeaturedTemplate,
  localizedDraft,
  monogram,
  OPENAI_TEMPLATES,
  placeholderRange,
  providerName,
  replaceProvider,
  templateName,
  uniqueName,
} from '../../settings/providers/model'
import { FeaturedCards } from '../../settings/providers/TemplateCard'
import { TemplateTile } from '../../settings/providers/TemplatePicker'
import { loadTemplates } from '../../settings/providers/templates'
import { isAuthFailure, TestReport, TestVerdictBadge } from '../../settings/providers/TestReport'
import { UrlPreview } from '../../settings/providers/UrlPreview'
import {
  currentSettings,
  notify,
  patchSettings,
  useRuntime,
  useSettings,
} from '../../settings/store'
import {
  focusStepTitle,
  FooterSwap,
  SkipStep,
  StepFooter,
  StepLayout,
  type StepProps,
} from '../shared'
import {
  fetchFingerprint,
  type ModelDefaults,
  type ModelDraftStore,
  type ModelPhase,
} from './modelDraft'

const PHASES: readonly ModelPhase[] = ['summary', 'choose', 'others', 'form']

/** 同一张卡片：OpenAI 的两个模板共用一张 */
function sameCard(a: string, b: string): boolean {
  return a === b || (OPENAI_TEMPLATES.includes(a) && OPENAI_TEMPLATES.includes(b))
}

export function Model({
  onNext,
  onBack,
  onSkipAll,
  canBack,
  store,
}: StepProps & { store: ModelDraftStore }) {
  const t = useT()
  const settings = useSettings()
  const reduced = useReducedMotionPref()
  const configured = (settings?.providers.length ?? 0) > 0 && settings?.defaults.translate !== null
  const { state, update, key } = store
  const phase: ModelPhase = state.phase ?? (configured ? 'summary' : 'choose')
  const draft = state.draft
  const defaults = state.defaults
  const [dir, setDir] = useState<1 | -1>(1)
  const [entered, setEntered] = useState(false)
  const [templates, setTemplates] = useState<ProviderTemplate[] | null>(null)
  const [creating, setCreating] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  const host = useRef<HTMLDivElement>(null)
  const draftId = draft?.provider.id ?? ''

  useEffect(() => {
    let alive = true
    loadTemplates()
      .then((list) => alive && setTemplates(list))
      .catch(() => alive && setTemplates([]))
    return () => {
      alive = false
    }
  }, [])

  // 换阶段之后焦点交给新阶段的标题（表单里 Key 输入框自动获得焦点时不动）
  const phaseKey = phase === 'form' ? `form:${draftId}` : phase
  const lastPhase = useRef(phaseKey)
  useEffect(() => {
    if (lastPhase.current === phaseKey) return
    lastPhase.current = phaseKey
    focusStepTitle(host.current?.closest('.ob-step') ?? null)
  }, [phaseKey])

  const go = (next: ModelPhase) => {
    setDir(PHASES.indexOf(next) >= PHASES.indexOf(phase) ? 1 : -1)
    setEntered(true)
    update({ phase: next })
  }

  const pick = async (tpl: ProviderTemplate) => {
    // 再点同一张卡片：回到上次填了一半的草稿
    if (draft && sameCard(draft.template.id, tpl.id)) {
      go('form')
      return
    }
    setCreating(tpl.id)
    try {
      const fresh = localizedDraft(await api.providerNew(tpl.id), tpl)
      const provider = { ...fresh, name: uniqueName(fresh.name, settings?.providers ?? []) }
      update({
        draft: { provider, template: tpl },
        defaults: { translate: null, ocr: null },
        fetchedFor: null,
      })
      go('form')
    } catch (error) {
      notify(ipcErrorText(error, 'provider_new'), 'danger')
    } finally {
      setCreating(null)
    }
  }

  const save = async () => {
    const current = currentSettings()
    if (!draft || !current) return
    setSaving(true)
    try {
      // 输入框里的 Key 先写入（不需要先按回车）；失败就地报错，不保存
      if (!(await key.commit())) return
      const providers = replaceProvider(current, draft.provider)
      const next = {
        translate: defaults.translate ?? current.defaults.translate,
        ocr: defaults.ocr ?? current.defaults.ocr,
      }
      if (await patchSettings({ providers, defaults: next })) {
        key.markAdded()
        store.reset()
        onNext()
      }
    } finally {
      setSaving(false)
    }
  }

  /** 阶段内的「上一步」：表单 → 卡片（或「其他服务」）→ 摘要；摘要 / 第一次的卡片 → 上一个大步骤 */
  const phaseBack: (() => void) | null =
    phase === 'form' && draft
      ? () => go(isFeaturedTemplate(draft.template.id) ? 'choose' : 'others')
      : phase === 'others'
        ? () => go('choose')
        : phase === 'choose' && configured
          ? () => go('summary')
          : null

  const ready = draft !== null && defaults.translate !== null
  const motion = adaptPreset(presets.stepSwitch(dir), 'fade', reduced)

  let body: ReactNode
  let next: ReactNode
  if (phase === 'summary' && settings) {
    body = <Summary onAdd={() => go('choose')} />
    next = (
      <Button
        variant="primary"
        size="md"
        onClick={onNext}
        data-default
        data-testid="onboarding-next"
      >
        {t('common.action.next')}
      </Button>
    )
  } else if (phase === 'others') {
    body = (
      <div className="ob-others" data-testid="onboarding-others">
        {templates === null ? (
          <Skeleton lines={4} />
        ) : (
          <div className="st-tiles ob-tiles">
            {templates
              .filter((tpl) => !isFeaturedTemplate(tpl.id))
              .map((tpl) => (
                <TemplateTile
                  key={tpl.id}
                  template={tpl}
                  onPick={(x) => void pick(x)}
                  busy={creating !== null}
                />
              ))}
          </div>
        )}
      </div>
    )
    next = <SkipStep onClick={onNext} />
  } else if (phase === 'form' && draft) {
    body = (
      <ProviderSetup
        draft={draft.provider}
        template={draft.template}
        templates={templates ?? []}
        keyEntry={key}
        defaults={defaults}
        fetchedFor={state.fetchedFor}
        onFetched={(fp) => update({ fetchedFor: fp })}
        onDefaults={(d) => update({ defaults: d })}
        onChange={(provider) => update({ draft: { ...draft, provider } })}
        onSwitchTemplate={(tpl) =>
          update({ draft: { provider: { ...draft.provider, ...switched(tpl) }, template: tpl } })
        }
      />
    )
    // 「跳过这一步」⇄「保存并继续」交叉淡入（宽度按较宽的那个，不跳）
    next = (
      <FooterSwap
        active={ready ? 'b' : 'a'}
        a={<SkipStep onClick={onNext} />}
        b={
          <Button
            variant="primary"
            size="md"
            loading={saving}
            onClick={() => void save()}
            data-default={ready || undefined}
            data-testid="onboarding-save"
          >
            {t('onboarding.model.save')}
          </Button>
        }
      />
    )
  } else {
    body = (
      <div data-testid="onboarding-cards">
        <FeaturedCards
          templates={templates}
          busyId={creating}
          onPick={(tpl) => void pick(tpl)}
          onOthers={() => go('others')}
        />
      </div>
    )
    next = <SkipStep onClick={onNext} />
  }

  return (
    <StepLayout
      // 填写表单时服务头部就是这一屏的标题（窗口只有 580 高，也让出说明文字的高度）
      title={phase === 'form' ? undefined : t('onboarding.model.title')}
      description={phase === 'form' ? undefined : t('onboarding.model.description')}
      data-testid="onboarding-model"
      footer={
        <StepFooter
          onSkipAll={onSkipAll}
          onBack={phaseBack ?? onBack}
          canBack={phaseBack !== null || canBack}
          next={next}
        />
      }
    >
      <m.div
        ref={host}
        key={phaseKey}
        initial={entered ? motion.initial : false}
        animate={motion.animate}
      >
        {body}
      </m.div>
    </StepLayout>
  )
}

/** OpenAI 卡片的 Responses ⇄ Chat 切换：保留草稿 id 与 Key，只换协议与模板预填的 quirks */
function switched(tpl: ProviderTemplate): Partial<Provider> {
  return { protocol: tpl.provider.protocol, quirks: tpl.provider.quirks, template_id: tpl.id }
}

function ProviderSetup({
  draft,
  template,
  templates,
  keyEntry,
  defaults,
  fetchedFor,
  onFetched,
  onDefaults,
  onChange,
  onSwitchTemplate,
}: {
  draft: Provider
  template: ProviderTemplate
  templates: readonly ProviderTemplate[]
  keyEntry: KeyEntry
  defaults: ModelDefaults
  /** 最近一次成功拉取模型时的连接指纹 */
  fetchedFor: string | null
  onFetched: (fingerprint: string) => void
  onDefaults: (d: ModelDefaults) => void
  onChange: (p: Provider) => void
  onSwitchTemplate: (tpl: ProviderTemplate) => void
}) {
  const t = useT()
  const runtime = useRuntime()
  const settings = useSettings()
  const sectionExpand = usePreset('sectionExpand')
  const test = useProviderTest(draft)
  const fetcher = useModelFetch(draft, (models) => {
    const next = { ...draft, models }
    onChange(next)
    onFetched(fetchFingerprint(next))
    if (!settings) return
    // 已经选好、而且还在新列表里（启用；OCR 还要能看图）的默认模型保持不动，只补上空着或已经失效的那一项
    const stillValid = (ref: ModelRef | null, vision: boolean) =>
      ref !== null &&
      ref.provider_id === next.id &&
      next.models.some(
        (x) => x.id === ref.model_id && isUsableModel(x) && (!vision || x.vision !== 'no'),
      )
    const kept: ModelDefaults = {
      translate: stillValid(defaults.translate, false) ? defaults.translate : null,
      ocr: stillValid(defaults.ocr, true) ? defaults.ocr : null,
    }
    // 空着的默认翻译模型优先补上测试连接探测成功的那个（/models 的第一项常是嵌入模型）
    const filled = fillDefaults({ ...settings, defaults: kept }, next, probedModel(next))
    onDefaults(filled ? { translate: filled.translate, ocr: filled.ocr } : kept)
  })
  const baseInput = useRef<HTMLInputElement>(null)
  const defaultsRef = useRef<HTMLDivElement>(null)
  const reduced = useReducedMotionPref()
  const isOpenAi = OPENAI_TEMPLATES.includes(template.id)
  const regions = template.region_variants
  const region = regions.find((r) => r.base_url === draft.base_url)?.base_url ?? null

  const runTest = async () => {
    // 草稿里的 Key 直接交给 Rust（只用于这一次请求，不经钥匙串）；保存时才写入凭据存储
    const run = await test.start(keyEntry.probeKey())
    // 还没有模型、或连接（协议 / base_url）变了才采用新列表：只是重新验证一遍时不动已经拉到的列表与选择。
    // 列表直接用测试顺带返回的 fetched_models（与 provider_models 同一套合并），不再多发一次 GET /models、多读一次 Key
    const stale = draft.models.length === 0 || fetchedFor !== fetchFingerprint(draft)
    if (
      run.phase === 'done' &&
      run.report.overall !== 'failed' &&
      run.report.fetched_models.length > 0 &&
      stale
    )
      fetcher.apply(run.report.fetched_models)
  }

  // 自动焦点：Base URL 还空着（中转站 / 自定义兼容端点）时先填它，已经预填时直接填 Key（只在表单出现时决定一次）
  const [needsBaseUrl] = useState(() => draft.base_url.trim() === '')
  useEffect(() => {
    if (needsBaseUrl) baseInput.current?.focus()
  }, [needsBaseUrl])

  const chooseRegion = (url: string) => {
    onChange({ ...draft, base_url: url })
    const range = placeholderRange(url)
    if (range) {
      requestAnimationFrame(() => {
        baseInput.current?.focus()
        baseInput.current?.setSelectionRange(range[0], range[1])
      })
    }
  }

  const name = providerName(draft)
  const haveModels = draft.models.length > 0
  const keyPresent = keyEntry.present || !template.key_required

  return (
    <div className="ob-setup" data-testid="onboarding-setup">
      <div className="ob-setup-head">
        <span className="st-logo ob-setup-logo" data-tone="accent" aria-hidden="true">
          {monogram(name)}
        </span>
        <span className="ob-setup-title">
          <h1 className="ob-title" tabIndex={-1} data-step-title>
            {isOpenAi ? t('settings.providers.template.openai') : templateName(template)}
          </h1>
          <span>{hostOf(draft.base_url)}</span>
        </span>
        {test.stale ? null : <TestVerdictBadge run={test.run} />}
      </div>
      <div className="ui-section-card ob-setup-card">
        {isOpenAi ? (
          <Field
            label={t('settings.providers.protocol')}
            description={t('onboarding.model.openai_protocol')}
          >
            <SegmentedControl<'openai-responses' | 'openai'>
              label={t('settings.providers.protocol')}
              value={template.id === 'openai' ? 'openai' : 'openai-responses'}
              options={[
                // i18n-ignore：协议名原样显示
                { value: 'openai-responses', label: 'Responses' },
                // i18n-ignore：协议名原样显示
                { value: 'openai', label: 'Chat' },
              ]}
              onChange={(id) => {
                const tpl = templates.find((x) => x.id === id)
                if (tpl) onSwitchTemplate(tpl)
              }}
              className="st-segmented"
            />
          </Field>
        ) : null}
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
            />
          </Field>
        ) : null}
        <Field label={t('settings.providers.base_url')} stacked>
          <TextField
            ref={baseInput}
            value={draft.base_url}
            onChange={(v) => onChange({ ...draft, base_url: v.trim() })}
            placeholder={t('settings.providers.base_url_placeholder')}
            aria-label={t('settings.providers.base_url')}
            className="st-mono"
            invalid={placeholderRange(draft.base_url) !== null}
            data-testid="onboarding-base-url"
          />
          <UrlPreview
            protocol={draft.protocol}
            baseUrl={draft.base_url}
            modelId={draft.models.find((x) => x.enabled)?.id ?? null}
          />
        </Field>
        <Field label={t('settings.providers.key')} stacked>
          <KeyField
            entry={keyEntry}
            required={template.key_required}
            keychain={runtime?.keychain ?? 'available'}
            focusOnMount={template.key_required && !needsBaseUrl}
            draftHint={t('onboarding.model.key_hint')}
            onSubmit={() => void runTest()}
            rejected={!test.stale && isAuthFailure(test.run)}
          />
        </Field>
        <Field label={t('settings.providers.test')} stacked>
          <div className="st-test-bar">
            <Button
              size="sm"
              variant={
                keyPresent &&
                (test.stale || !(test.run.phase === 'done' && test.run.report.overall === 'ok'))
                  ? 'primary'
                  : 'secondary'
              }
              loading={test.run.phase === 'running' || fetcher.loading || keyEntry.busy}
              disabled={!draft.base_url.trim()}
              onClick={() => void runTest()}
              data-testid="onboarding-test"
            >
              {test.run.phase === 'idle'
                ? t('settings.providers.test_run')
                : t('settings.providers.test_again')}
            </Button>
            {fetcher.error ? (
              <span className="st-field-hint" data-tone="warning">
                {fetcher.error}
              </span>
            ) : null}
          </div>
          <TestReport
            run={test.run}
            stale={test.stale}
            providerName={name}
            onRetry={() => void runTest()}
          />
        </Field>
      </div>
      <AnimatePresence initial={false}>
        {haveModels ? (
          <m.div
            key="defaults"
            className="st-clip"
            {...sectionExpand}
            // 展开与滚动同时进行：每一帧露出正在长高的下沿（不等弹簧结束再滚）
            onUpdate={(latest) =>
              revealGrowing(defaultsRef.current?.parentElement ?? null, latest.height)
            }
            onAnimationComplete={() => {
              // 减少动效时没有逐帧的高度：到位后滚入视野
              if (reduced) defaultsRef.current?.scrollIntoView?.({ block: 'nearest' })
            }}
          >
            <div
              ref={defaultsRef}
              className="ui-section-card ob-setup-card ob-defaults"
              data-testid="onboarding-defaults"
            >
              <DefaultModelFields
                providers={[draft]}
                translate={defaults.translate}
                ocr={defaults.ocr}
                onChange={(which, ref) => onDefaults({ ...defaults, [which]: ref })}
                testId="onboarding-defaults"
              />
            </div>
          </m.div>
        ) : null}
      </AnimatePresence>
    </div>
  )
}

/** 已经接入过服务：显示当前的默认模型，可以再添加一个 */
function Summary({ onAdd }: { onAdd: () => void }) {
  const t = useT()
  const settings = useSettings()
  if (!settings) return null
  const d = settings.defaults
  const providerOf = (ref: ModelRef | null) =>
    settings.providers.find((p) => p.id === ref?.provider_id)
  const row = (label: string, ref: ModelRef | null) => {
    const p = providerOf(ref)
    return (
      <div className="ob-summary-row">
        <span className="ob-summary-label">{label}</span>
        <span className="ob-summary-value">
          {ref ? (
            <>
              <b>{ref.model_id}</b>
              <span>{p ? providerName(p) : ''}</span>
            </>
          ) : (
            <span>{t('settings.defaults.choose')}</span>
          )}
        </span>
      </div>
    )
  }
  return (
    <div className="ob-summary" data-testid="onboarding-summary">
      <div className="ob-summary-head">
        <span className="ob-summary-check">
          <Icon icon={Check} size={16} />
        </span>
        <span>
          {t('onboarding.model.configured', {
            names: formatList(settings.providers.map((p) => providerName(p))),
          })}
        </span>
      </div>
      {row(t('settings.defaults.translate'), d.translate)}
      {row(t('settings.defaults.ocr'), d.ocr)}
      <button
        type="button"
        className="ob-summary-add"
        onClick={onAdd}
        data-testid="onboarding-add-another"
      >
        <Icon icon={Plus} size={14} />
        <span className="st-grow">{t('onboarding.model.add_another')}</span>
        <Icon icon={ChevronRight} size={14} className="st-bigcard-chevron" />
      </button>
    </div>
  )
}
