/**
 * 输入视图。
 *
 * 入口：划词时没有取到选区（含所有取词失败，原因在 `InputPayload.hint`，用 NotReadyBanner 渲染），或专用快捷键
 * （⌥⇧D / Ctrl+Alt+Shift+D）。结构与视觉稿 `design.input` 一致：标题 · 语言条（自动检测 → 目标，可手动改目标）· ✕；
 * 无边框多行输入框（自动聚焦、IME 安全、↩ 换行、⌘↩ / Ctrl+Enter 提交）；底栏「↩ 换行」+ [翻译 ⌘↩]。
 *
 * 提交：`translate_start` 的事件先收进 hub 的缓冲，拿到 StartAck 后 `switchView(翻译载荷, { width: ack.width })`——
 * 120 ms 交叉淡入，宽度在内容完全透明的那一帧由外壳改（视图不调 panel_set_width）；翻译视图挂载时
 * 从缓冲接着显示。超过长文本阈值时先切到翻译视图的长文本确认；命令失败时切到翻译视图显示错误卡片。
 *
 * 首次渲染的 JIT 冷启动：面板隐藏时的空闲时间里预热一次（文件末尾的 prewarmInputOnce）。
 */
import { X } from 'lucide-react'
import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
  type CSSProperties,
} from 'react'
import type { ViewProps } from '../../panel/types'
import { subscribeAppearance, getAppearance } from '../../shared/hooks/useAppearance'
import { usePanelDrag } from '../../shared/hooks/usePanelDrag'
import { useViewActive } from '../../shared/hooks/useViewActive'
import { useT } from '../../shared/i18n/useT'
import { api, boot } from '../../shared/ipc'
import {
  PREWARM_ORDER,
  prewarmRender,
  schedulePrewarm,
  usePrewarm,
} from '../../shared/perf/prewarm'
import type { Lang, PanelPayload, PermissionKind, TranslateReq } from '../../shared/ipc/types'
import { useKeyScope } from '../../shared/keys/useKeyScope'
import { Button, IconButton, Kbd, NotReadyBanner, TextArea } from '../../shared/ui'
import { styleList, styleOf, translateSettings, useCatalog } from '../translate/catalog'
import { pendingKey, putHandoff, type Handoff } from '../translate/handoff'
import { discardBuffered, failLocally, startBuffered } from '../translate/hub'
import { BROWSER_ACCELERATORS } from '../translate/keys'
import { LangSwitch, type SwapControl } from '../translate/LangSwitch'
import { toLlmError } from '../translate/run'
import { scheduleMarkdownWarmup } from '../translate/warmup'
import { autoTarget, detectInput, isDictionaryInput, planSwap } from '../translate/text'
import { loadTranslateMessages } from '../translate/i18n'
import '../translate/translate.css'
import './input.css'

loadTranslateMessages()

/** 输入框的最少 / 最多行数（视觉稿 design.input）；卡片上限放不下时按上限收紧 */
const MIN_ROWS = 3
const MAX_ROWS = 10
/** 输入框以外的固定高度：上下内边距 24 + 头部 34 + 底栏 38（与 translate.css 的 tv-header / tv-footer 一致） */
const CHROME = 24 + 34 + 38
/**
 * 提示条（NotReadyBanner）占用的高度：按一行估算（36 + 间距 8）。文案折成两行时多出的一行由输入框在 --tv-max 内
 * 收缩吸收（input.css：输入框可以收缩、内部滚动），不会把卡片撑出上限
 */
const BANNER = 44
/** 正文行高（--lh-body） */
const LINE_HEIGHT = 1.6

export function InputView(props: ViewProps<'input'>) {
  const { payload, sessionId, maxHeight, phase, focused } = props
  // 从头部、提示与底栏的空白处拖动面板（shared/hooks/usePanelDrag）；输入框、按钮、语言条不触发
  const panelDrag = usePanelDrag(sessionId, props.host === 'panel')
  const t = useT()
  const prewarm = usePrewarm()
  // 预热渲染（见文件末尾）不拉取设置快照：风格 / 语言用内置默认值
  const catalog = useCatalog(!prewarm)
  const settings = translateSettings(catalog)
  const styles = useMemo(() => styleList(catalog), [catalog])
  const fontSize = useSyncExternalStore(
    subscribeAppearance,
    () => getAppearance().fontSize,
    () => getAppearance().fontSize,
  )

  const [text, setText] = useState(payload.initial_text)
  /** 手动指定的目标语言（只对本会话生效）；null = 自动方向 */
  const [override, setOverride] = useState<Lang | null>(payload.target_lang)
  /** 交换方向时强制显示的源语言（检测不确定时「把原文当作另一种语言」，text.ts planSwap） */
  const [forcedSource, setForcedSource] = useState<Lang | null>(null)
  const [hintDismissed, setHintDismissed] = useState(false)
  const [submitting, setSubmitting] = useState(false)
  const submitted = useRef(false)
  const rootRef = useRef<HTMLElement>(null)
  const viewActive = useViewActive()
  // 提交之后的命令返回时，视图可能已经卸载或进入退出（Esc、点击外部、又唤出了新会话）
  const live = useRef(true)
  const phaseRef = useRef(phase)
  useLayoutEffect(() => {
    phaseRef.current = phase
  }, [phase])
  useEffect(() => {
    live.current = true
    if (!prewarm) inputMounted = true
    return () => {
      live.current = false
    }
    // 预热标记在一个实例里不变
    // oxlint-disable-next-line react/exhaustive-deps
  }, [])
  const stale = () =>
    !live.current || phaseRef.current === 'exiting' || phaseRef.current === 'hidden'

  // 方向判定与 Rust 同一套规则（text.ts detectInput ↔ wisp_prompts::lang）：输入过程中语言条就是真实的方向
  const detection = useMemo(() => detectInput(text), [text])
  const detected = detection?.lang ?? null
  const target = override ?? autoTarget(detected, settings.primary_lang, settings.secondary_lang)
  const swapPlan = planSwap({
    detected,
    forced: forcedSource,
    target,
    lowConfidence: detection?.lowConfidence ?? false,
    primary: settings.primary_lang,
    secondary: settings.secondary_lang,
  })
  const hint = hintDismissed ? null : payload.hint
  const empty = text.trim() === ''

  // 输入框最多行数：卡片上限扣掉头部、底栏、提示条后能放下的行数（超出时输入框内部滚动）
  const available = maxHeight - CHROME - (hint ? BANNER : 0)
  const maxRows = Math.max(
    MIN_ROWS,
    Math.min(MAX_ROWS, Math.floor(available / (fontSize * LINE_HEIGHT))),
  )

  const toTranslate = (patch: Partial<Extract<PanelPayload, { kind: 'translate' }>>) => {
    const styleId = settings.default_style
    const style = styleOf(styles, styleId)
    const source = text
    const next: PanelPayload = {
      kind: 'translate',
      source_text: source,
      request_id: null,
      task: 'translate',
      style_id: styleId,
      detected_lang: forcedSource ?? detected,
      target_lang: target,
      dict_mode: style.dictionary && isDictionaryInput(source),
      long_text: null,
      not_ready: null,
      selection_method: null,
      ...patch,
    }
    return next
  }

  /**
   * 交给翻译视图的会话内选择，以及输入框所在区域的高度（头部与底栏之间）：翻译视图在第一次流式结束前不低于它，
   * 交叉淡入之后卡片不会先缩再长（骨架画满这段高度）
   */
  const handoff = (): Handoff => {
    const root = rootRef.current
    const header = root?.querySelector<HTMLElement>(':scope > .tv-header')
    const footer = root?.querySelector<HTMLElement>(':scope > .tv-footer')
    let body: Handoff['body']
    if (header && footer) {
      const gap =
        Number.parseFloat(getComputedStyle(header).marginBottom) +
        Number.parseFloat(getComputedStyle(footer).marginTop)
      const px = Math.round(footer.offsetTop - (header.offsetTop + header.offsetHeight) - gap)
      if (px > 0) body = { px, lines: Math.floor(px / (fontSize * LINE_HEIGHT)) }
    }
    return { target: override, forcedSource, body }
  }

  const submit = async () => {
    if (empty || submitted.current) return false
    submitted.current = true
    setSubmitting(true)
    const req: TranslateReq = {
      session_id: sessionId,
      text,
      task: 'translate',
      style_id: settings.default_style,
      target_lang: override,
      model_ref: null,
      dict_mode: 'auto',
      bypass_cache: false,
      draft: null,
      // 长文本由 Rust 判定（与划词同一阈值与耗时估算）
      confirm_long: false,
    }
    try {
      const ack = await startBuffered(req)
      if (ack.long_text) {
        if (stale()) return
        // 长文本：请求没有发出。先在翻译视图里确认（估算来自 Rust，含预计耗时），「继续」时由它以 confirm_long
        // 发起
        putHandoff(pendingKey(sessionId), { target: override, forcedSource })
        props.switchView(toTranslate({ long_text: ack.long_text }), { width: ack.width })
        return
      }
      if (stale()) {
        // 没有人会接着显示它了：取消请求、丢掉缓冲，不把载荷切进正在退出 / 已结束的会话
        discardBuffered(ack.request_id)
        void api.cancel(ack.request_id).catch(() => undefined)
        return
      }
      putHandoff(ack.request_id, handoff())
      props.switchView(toTranslate({ request_id: ack.request_id }), { width: ack.width })
    } catch (error) {
      if (stale()) return
      // 没能开始（例如还没有接入模型）：照样切到翻译视图，由它显示错误卡片（可重试 / 去设置）
      const id = failLocally(toLlmError(error))
      putHandoff(id, { target: override, forcedSource })
      props.switchView(toTranslate({ request_id: id }), { width: null })
    }
  }

  // 提交之后的翻译视图可能用 Markdown-lite 排版：显示之后的空闲时间里先预热它的字体组合（整个窗口只做一次）
  const shown = phase === 'shown' && viewActive
  useEffect(() => (shown ? scheduleMarkdownWarmup() : undefined), [shown])

  // 自动聚焦：TextArea 的 autoFocus 只在视图显示的那次提交里尝试一次；那一刻窗口还没真正可见（浏览器 mock、
  // 慢机器上的 WebView）时聚焦会落空。显示之后、窗口获得键盘焦点时，焦点不在任何控件上就再聚焦一次
  useEffect(() => {
    if (!shown || !focused || submitted.current) return
    const active = document.activeElement
    if (active && active !== document.body) return
    rootRef.current?.querySelector<HTMLTextAreaElement>('textarea')?.focus({ preventScroll: true })
  }, [shown, focused])

  // ⌘↩ / Ctrl+Enter：焦点不在输入框（例如在提示条按钮上）时也能提交；IME 组合中由键盘路由放行。
  // ⌘R / ⌘⇧R / ⌘P 在这里没有动作，同样吞掉（translate/keys.ts：否则落到 WebView2 的重载 / 打印预览）
  useKeyScope('input', [
    { combo: 'Mod+Enter', allowInInput: true, run: () => void submit() },
    ...BROWSER_ACCELERATORS,
  ])

  const swap: SwapControl = empty
    ? null
    : swapPlan.ok
      ? {
          run: () => {
            setForcedSource(swapPlan.forceSource)
            setOverride(swapPlan.target)
          },
        }
      : {
          reason: t('translate.a11y.swap_unavailable', {
            lang: t(`common.lang.name.${detected ?? 'other'}`),
          }),
        }

  const openPermission = (kind: PermissionKind) => void api.permissionsOpen(kind)

  return (
    // 按下只用来开始拖动面板（usePanelDrag），不是交互控件
    // oxlint-disable-next-line jsx-a11y/no-noninteractive-element-interactions
    <section
      ref={rootRef}
      className="tv iv"
      data-testid="view-input"
      data-view="input"
      data-session={sessionId}
      aria-label={t('panel.view.input')}
      style={
        {
          '--tv-max': `calc(${Math.max(0, Math.round(maxHeight))}px - 2 * var(--pad-panel-y))`,
        } as CSSProperties
      }
      onPointerDown={panelDrag?.onPointerDown}
      onMouseDown={panelDrag?.onMouseDown}
    >
      <div className="tv-header">
        <span className="tv-title">{t('panel.view.input')}</span>
        <span className="tv-spacer" />
        <LangSwitch
          source={empty ? null : (forcedSource ?? detected)}
          target={target}
          lowConfidence={!empty && forcedSource === null && (detection?.lowConfidence ?? false)}
          targets={[settings.primary_lang, settings.secondary_lang]}
          onPickTarget={(lang) => {
            setForcedSource(null)
            setOverride(lang)
          }}
          swap={swap}
          data-testid="iv-langbar"
        />
        <IconButton
          icon={X}
          label={t('common.action.close')}
          shortcut="Escape"
          onClick={() => props.requestHide('close_button')}
          data-testid="iv-close"
        />
      </div>
      {hint ? (
        <NotReadyBanner
          notReady={hint}
          onOpenSystemSettings={openPermission}
          onDismiss={() => setHintDismissed(true)}
          className="iv-banner"
          data-testid="iv-hint"
        />
      ) : null}
      <TextArea
        variant="bare"
        value={text}
        onChange={setText}
        minRows={MIN_ROWS}
        maxRows={maxRows}
        // oxlint-disable-next-line jsx-a11y/no-autofocus -- 输入视图打开即可打字（暂存中推迟到显示时，useAutoFocus）
        autoFocus
        spellCheck={false}
        placeholder={t('translate.input.placeholder')}
        aria-label={t('panel.view.input')}
        onSubmit={() => void submit()}
        readOnly={submitting}
        className="iv-text"
        data-testid="iv-text"
      />
      <div className="tv-footer iv-footer">
        <span className="iv-hint" aria-hidden="true">
          <Kbd combo="Enter" variant="cap" />
          {t('translate.input.newline')}
        </span>
        <Button
          variant="primary"
          size="sm"
          disabled={empty}
          loading={submitting}
          onClick={() => void submit()}
          data-testid="iv-submit"
        >
          {/* 按钮里的快捷键与其他按钮一致：单段文字、与标签同色不透明，靠字重区分 */}
          {t('translate.input.submit')}
          <Kbd combo="Mod+Enter" variant="inline" />
        </Button>
      </div>
    </section>
  )
}

// -----------------------------------------------------------------------------
// JIT 预热（shared/perf/prewarm.tsx）：输入视图冷启动时 ready 只剩几毫秒余量（回收出来的新页面同样冷），空闲时在不可见的
// 暂存节点里渲染一次空输入框的状态（头部、语言条、输入框、底栏）再卸载；真实的视图挂载过就不再需要

/** 真实的输入视图已经挂载过 */
let inputMounted = false
let inputPrewarmed = false

const PREWARM_PROPS: ViewProps<'input'> = {
  sessionId: 0,
  payload: { kind: 'input', initial_text: '', target_lang: null, hint: null },
  phase: 'preparing',
  focused: false,
  host: 'panel',
  frame: boot.frame_mode,
  placement: 'below',
  maxHeight: 480,
  requestHide: () => undefined,
  switchView: () => undefined,
  activate: () => undefined,
}

/** 预热渲染本身（同步渲染、排版、卸载，不留节点） */
export function prewarmInputRender(): void {
  prewarmRender(<InputView {...PREWARM_PROPS} />, { width: 480, height: 320 })
}

/** 预热一次（见上）；真实视图已挂载过、或已经预热过时什么都不做 */
export function prewarmInputOnce(): void {
  if (inputPrewarmed || inputMounted) return
  inputPrewarmed = true
  prewarmInputRender()
}

schedulePrewarm(prewarmInputOnce, PREWARM_ORDER.input)
