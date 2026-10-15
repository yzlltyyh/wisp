/**
 * 设置 → 模型与服务。
 *
 * 三个子视图（列表 → 选模板 → 编辑器）。切换只有新视图的进入动画（presets.stepSwitch 的 initial / animate：前进 x 8→0、
 * 后退 x −8→0），旧视图立即卸载，不会两层内容叠在一起；进入子视图滚到顶部、返回时恢复列表的滚动位置并把焦点还给那一行
 * （lib/subview.ts）。新建的服务点「添加服务」后，编辑器原地变成已保存的服务（同一个 React key，不重新挂载）。
 * - 列表：服务（两字母头像——统一的颜色，不再兼表「有没有 Key」；名称；host 与模型数分开截断；状态只在有结论时显示：
 *   未设置 Key / 本次测试结果 / 配置改过需重新测试，从没测过时不显示「未测试」）、默认模型（翻译 / OCR）；
 *   孤立 Key 与凭据存储状态的提示；空状态是与引导一致的 2 × 2 模板卡片（第四张「其他服务」进选模板）；
 * - 新建的服务与已有的重名时自动加编号（「DeepSeek 2」）；
 * - 选模板：16 个模板分组展示；选中后 `provider_new` 生成草稿（尚未保存）；
 * - 编辑器：见 providers/ProviderEditor。
 * 深链锚点：`provider:<id>` 打开某个服务，`new-provider` 打开选模板。
 */
import { ChevronRight, KeyRound, Plus, ShieldAlert } from 'lucide-react'
import { m } from 'motion/react'
import { useEffect, useState, type MouseEvent, type ReactNode } from 'react'
import { useT } from '../../shared/i18n/useT'
import { api } from '../../shared/ipc'
import type { ModelRef, Provider, ProviderTemplate } from '../../shared/ipc/types'
import { adaptPreset, presets } from '../../shared/motion/presets'
import { useReducedMotionPref } from '../../shared/motion/useReducedMotionPref'
import { Badge, Banner, Button, Icon, Section } from '../../shared/ui'
import { useSettingsNav } from '../context'
import { Page } from '../components/Page'
import { ipcErrorText } from '../lib/errors'
import { at } from '../lib/patch'
import { useSubviewNav } from '../lib/subview'
import {
  currentSettings,
  notify,
  patchSettings,
  queuePatch,
  useRuntime,
  useSettings,
} from '../store'
import { DefaultModelFields } from '../providers/DefaultModels'
import { lastTestRun, probedModel, useTestRunsVersion } from '../providers/hooks'
import {
  fillDefaults,
  hostOf,
  localizedDraft,
  monogram,
  providerName,
  providerNeedsKey,
  replaceProvider,
  uniqueName,
} from '../providers/model'
import { ProviderEditor } from '../providers/ProviderEditor'
import { FeaturedCards } from '../providers/TemplateCard'
import { TemplatePicker } from '../providers/TemplatePicker'
import { loadTemplates } from '../providers/templates'

type View =
  | { kind: 'list' }
  | { kind: 'pick' }
  | { kind: 'edit'; id: string }
  | { kind: 'new'; draft: Provider; template: ProviderTemplate }

const DEPTH: Record<View['kind'], number> = { list: 0, pick: 1, edit: 1, new: 2 }

/** 子视图的 React key：新建与已保存的同一个服务共用一个（添加后原地变化，不重新挂载） */
function viewKey(view: View): string {
  if (view.kind === 'edit') return `editor:${view.id}`
  if (view.kind === 'new') return `editor:${view.draft.id}`
  return view.kind
}

/** 键盘触发的点击（Enter / Space）：detail 为 0 */
const viaKeyboard = (e: MouseEvent) => e.detail === 0

/** 发出时按当前本地快照取整个 providers 数组（期间删掉的服务不会被写回来） */
function providersPatch() {
  const s = currentSettings()
  return s ? { providers: s.providers } : null
}

export default function ProvidersPage() {
  const t = useT()
  const nav = useSettingsNav()
  const settings = useSettings()
  const reduced = useReducedMotionPref()
  const [view, setView] = useState<View>({ kind: 'list' })
  const [dir, setDir] = useState<1 | -1>(1)
  const [templates, setTemplates] = useState<ProviderTemplate[] | null>(null)
  const [creating, setCreating] = useState<string | null>(null)
  const key = viewKey(view)
  const sub = useSubviewNav(key)

  useEffect(() => {
    void loadTemplates()
      .then(setTemplates)
      .catch(() => setTemplates([]))
  }, [])

  const go = (next: View, returnTo: string | null = null) => {
    if (viewKey(next) !== key) sub.leave(returnTo)
    setDir(DEPTH[next.kind] >= DEPTH[view.kind] ? 1 : -1)
    setView(next)
  }

  // 深链锚点（随分组一起出现：不播子视图的进入动画）
  useEffect(() => {
    const f = nav.focus
    if (!f) return
    if (f === 'new-provider') setView({ kind: 'pick' })
    else if (f.startsWith('provider:')) setView({ kind: 'edit', id: f.slice('provider:'.length) })
    nav.consumeFocus()
    // eslint-disable-next-line react/exhaustive-deps -- 只响应锚点变化
  }, [nav.focus])

  // 正在编辑的服务被删除（例如外部改了 settings.json）：回到列表
  useEffect(() => {
    if (view.kind === 'edit' && settings && !settings.providers.some((p) => p.id === view.id)) {
      setDir(-1)
      setView({ kind: 'list' })
    }
  }, [settings, view])

  if (!settings) return null

  const pick = async (tpl: ProviderTemplate) => {
    setCreating(tpl.id)
    try {
      const fresh = localizedDraft(await api.providerNew(tpl.id), tpl)
      // 与已有的服务重名时加编号，列表里不会出现两行一模一样的「DeepSeek / api.deepseek.com」
      const draft = { ...fresh, name: uniqueName(fresh.name, settings.providers) }
      go({ kind: 'new', draft, template: tpl })
    } catch (error) {
      notify(ipcErrorText(error, 'provider_new'), 'danger')
    } finally {
      setCreating(null)
    }
  }

  const templateOf = (p: Provider) => templates?.find((tpl) => tpl.id === p.template_id) ?? null

  const motion = adaptPreset(presets.stepSwitch(dir), 'fade', reduced)

  let body
  if (view.kind === 'pick') {
    body = (
      <TemplatePicker
        templates={templates}
        busyId={creating}
        onPick={(tpl) => void pick(tpl)}
        onBack={() => go({ kind: 'list' })}
      />
    )
  } else if (view.kind === 'new') {
    const draft = view.draft
    body = (
      <ProviderEditor
        provider={draft}
        template={view.template}
        isNew
        onChange={(next) => setView({ ...view, draft: next })}
        onBack={() => go({ kind: 'pick' })}
        onAdd={async () => {
          const current = currentSettings()
          if (!current) return false
          const providers = replaceProvider(current, draft)
          // 默认翻译模型优先用测试连接探测成功的那个（列表第一项常是嵌入模型）
          const defaults = fillDefaults(current, draft, probedModel(draft))
          const ok = await patchSettings(defaults ? { providers, defaults } : { providers })
          if (!ok) return false
          notify(
            defaults
              ? t('settings.providers.added_with_defaults', { name: providerName(draft) })
              : t('settings.providers.added', { name: providerName(draft) }),
            'success',
          )
          return true
        }}
        // 同一个编辑器原地变成已保存的服务（同一个 React key）；返回时回到列表
        onAdded={() => setView({ kind: 'edit', id: draft.id })}
      />
    )
  } else if (view.kind === 'edit') {
    const provider = settings.providers.find((p) => p.id === view.id)
    body = provider ? (
      <ProviderEditor
        provider={provider}
        template={templateOf(provider)}
        isNew={false}
        onChange={(next) => {
          const current = currentSettings()
          if (current)
            queuePatch(
              `provider:${next.id}`,
              { providers: replaceProvider(current, next) },
              providersPatch,
            )
        }}
        onBack={() => go({ kind: 'list' })}
        onDeleted={() => go({ kind: 'list' })}
      />
    ) : null
  } else {
    body = (
      <ProviderList
        providers={settings.providers}
        defaults={settings.defaults}
        templates={templates}
        onOpen={(id, keyboard) => go({ kind: 'edit', id }, keyboard ? `provider:${id}` : null)}
        onAdd={(keyboard) => go({ kind: 'pick' }, keyboard ? 'provider-add' : null)}
        onQuickAdd={(tpl) => void pick(tpl)}
        creating={creating}
      />
    )
  }

  return (
    <div ref={sub.host} className="st-subview-host">
      <m.div
        key={key}
        className="st-subview"
        // 分组首次显示时由分组切换的淡入负责，子视图自己不再动
        initial={sub.entered ? motion.initial : false}
        animate={motion.animate}
      >
        {body}
      </m.div>
    </div>
  )
}

function ProviderList({
  providers,
  defaults,
  templates,
  onOpen,
  onAdd,
  onQuickAdd,
  creating,
}: {
  providers: readonly Provider[]
  defaults: { translate: ModelRef | null; ocr: ModelRef | null }
  templates: readonly ProviderTemplate[] | null
  onOpen: (id: string, keyboard: boolean) => void
  onAdd: (keyboard: boolean) => void
  onQuickAdd: (tpl: ProviderTemplate) => void
  creating: string | null
}) {
  const t = useT()
  const runtime = useRuntime()
  useTestRunsVersion()
  return (
    <Page title={t('settings.nav.providers')} description={t('settings.providers.description')}>
      {runtime && runtime.keychain !== 'available' ? (
        <div className="st-global-banner">
          <Banner tone="warning" icon={ShieldAlert} data-testid="keychain-banner">
            {t(`settings.providers.keychain_${runtime.keychain}`)}
          </Banner>
        </div>
      ) : null}
      {runtime && runtime.orphan_keys > 0 ? (
        <div className="st-global-banner">
          <Banner tone="info" icon={KeyRound} data-testid="orphan-keys-banner">
            {t('settings.providers.orphan_keys', {
              count: runtime.orphan_keys,
              n: runtime.orphan_keys,
            })}
          </Banner>
        </div>
      ) : null}

      <Section
        title={t('settings.providers.services')}
        action={
          providers.length > 0 ? (
            <Button
              size="sm"
              icon={Plus}
              onClick={(e) => onAdd(viaKeyboard(e))}
              data-nav-id="provider-add"
              data-testid="provider-add-open"
            >
              {t('settings.providers.add')}
            </Button>
          ) : null
        }
        data-testid="provider-list"
      >
        {providers.length === 0 ? (
          // 空状态：与引导一致的 2 × 2 模板卡片，第四张「其他服务」进入完整的模板列表
          <div className="st-providers-empty" data-testid="provider-empty">
            <p className="st-providers-empty-text">
              <b>{t('settings.providers.empty')}</b>
              <span>{t('settings.providers.empty_detail')}</span>
            </p>
            <FeaturedCards
              templates={templates}
              busyId={creating}
              onPick={onQuickAdd}
              onOthers={() => onAdd(false)}
            />
          </div>
        ) : (
          providers.map((p) => {
            const hasKey = runtime?.has_key[p.id] ?? false
            const tpl = templates?.find((x) => x.id === p.template_id)
            const keyRequired = providerNeedsKey(p, tpl)
            const { run, stale } = lastTestRun(p)
            const enabled = p.models.filter((x) => x.enabled).length
            let badge: ReactNode = null
            if (keyRequired && !hasKey)
              badge = (
                <Badge tone="warning" dot>
                  {t('settings.providers.no_key')}
                </Badge>
              )
            else if (stale && run.phase === 'done')
              badge = (
                <Badge tone="neutral" dot>
                  {t('settings.providers.retest')}
                </Badge>
              )
            else if (run.phase === 'done' && run.report.overall === 'ok')
              badge = (
                <Badge tone="success" dot>
                  {t('settings.providers.connected')}
                </Badge>
              )
            else if (run.phase === 'done' && run.report.overall === 'failed')
              badge = (
                <Badge tone="danger" dot>
                  {t('settings.providers.test_failed')}
                </Badge>
              )
            else if (run.phase === 'done')
              badge = (
                <Badge tone="warning" dot>
                  {t('settings.providers.test_partial')}
                </Badge>
              )
            // 从没测试过：不显示徽标（每次打开窗口都是一排灰色的「未测试」，只是噪声）
            return (
              <button
                key={p.id}
                type="button"
                className="st-provider-row"
                onClick={(e) => onOpen(p.id, viaKeyboard(e))}
                data-nav-id={`provider:${p.id}`}
                data-testid={`provider-row-${p.id}`}
              >
                {/* 头像只是标识（两字母、统一颜色），状态由右侧的徽标表达；读屏不念头像字母 */}
                <span className="st-logo" data-tone="accent" aria-hidden="true">
                  {monogram(providerName(p))}
                </span>
                <span className="st-provider-text">
                  <b>{providerName(p)}</b>
                  {/* host 单独截断，「· N 个模型」始终可见 */}
                  <span className="st-provider-sub">
                    <span className="st-provider-host">
                      {hostOf(p.base_url) || t('settings.providers.no_url')}
                    </span>
                    <span className="st-provider-count">
                      {' · '}
                      {t('settings.providers.model_count', { count: enabled, n: enabled })}
                    </span>
                  </span>
                </span>
                {badge}
                <Icon icon={ChevronRight} size={14} className="st-row-chevron" />
              </button>
            )
          })
        )}
      </Section>

      <Section
        title={t('settings.defaults.title')}
        description={t('settings.defaults.description')}
      >
        <DefaultModelFields
          providers={providers}
          translate={defaults.translate}
          ocr={defaults.ocr}
          onChange={(which, ref) => void patchSettings(at(`defaults.${which}`, ref))}
        />
      </Section>
    </Page>
  )
}
