/**
 * 面板外壳：会话 + 同步 prepare + 两种帧模式的卡片、进入 / 退出与高度跟随 +
 * 视图交叉淡入 + 全局键盘作用域 + 开发者模式的延迟埋点与帧监视。
 *
 * 全局键（global 层，视图作用域之后才轮到）：Esc 关闭、⌘W / Ctrl+W 关闭、⌘, / Ctrl+, 打开设置。
 * 隐藏后整棵视图树卸载（`phase = hidden` 时不渲染卡片），rAF / Observer / 计时器随之停止。
 * 窗口不能透明（Css 帧模式且 `shadow_margin == 0`，例如 Wayland）时 `<html data-opaque>`：页面底色取
 * `--surface-solid`（base.css），卡片铺满窗口、不做进入 / 退出的透明度与变换（PanelCard），高度变化立即上报。
 *
 * **展开阅读**（翻译 / OCR；PanelPrepare.reading 不为 null 时视图拿到 `toggleReading`。输入会话的 prepare 也带着它——
 * 提交后在同一会话里换成翻译视图，换过去之后才提供）：点下（或按快捷键）的那一刻
 * 调用 `panel_set_reading` 并开始把视图淡出（timing.crossfade、ease.exit；减少动效时 reducedFade）——Rust 立即返回
 * 新的宽度、上限与版式，并把窗口的改变排在同一时刻（Native 帧模式的宽度切换在 timing.crossfade 之后）；内容完全透明、
 * 结果也到了之后，在同一次提交里让新的宽度、上限与版式生效（panelStore.commitReading），立即上报新高度（高度走
 * spring.gentle），再淡入（presets.crossfade）。视图不重新挂载：进行中的流、文本框里的编辑都保留。展开时面板取得键盘
 * 焦点（键盘滚动、快捷键、Esc 都要能用）；Esc 先收起、再按一次才关闭（usePanelLifecycle 的 `setEscapeHook`）；关闭
 * 面板即复原（下一次唤出是新会话、普通布局）。
 */
import { animate } from 'motion/react'
import { useCallback, useEffect, useLayoutEffect, useRef, type ComponentType } from 'react'
import { flushSync } from 'react-dom'
import { api, boot } from '../../shared/ipc'
import type { PanelPayload, PanelPrepare, PanelReading } from '../../shared/ipc/types'
import { cycleFocusWithin } from '../../shared/keys/focusLoop'
import { useKeyScope } from '../../shared/keys/useKeyScope'
import { FrameMonitorView } from '../../shared/perf/FrameMonitorView'
import { frameMonitorEnabled } from '../../shared/perf/frameMonitor'
import { warmUpWhenIdle } from '../../shared/perf/warmup'
import { setWindowHidden } from '../../shared/hooks/useHidden'
import { presets } from '../../shared/motion/presets'
import { useReducedMotionPref } from '../../shared/motion/useReducedMotionPref'
import { DevLatency, startLatencyFeed } from './DevLatency'
import { viewOverride } from './devHost'
import type { FrameLabProps } from './FrameLab'
import { frameLab } from './frameLabConfig'
import { PanelCard } from './PanelCard'
import { usePanelStore } from './panelStore'
import { noteReserve, useHeightFollow, type ResizeHint } from './useHeightFollow'
import { isOpaqueWindow, setEscapeHook, usePanelLifecycle } from './usePanelLifecycle'
import { ViewSwitch } from './ViewSwitch'

/** 开发者模式的延迟 / 帧监视行的高度（逻辑 px）：视图拿到的 maxHeight 扣掉它，内容 + 这一行不超过卡片上限 */
export const DEV_HUD_HEIGHT = 18

/**
 * 当前视图能否展开阅读：这次显示能展开（PanelPrepare.reading 不为 null），而且此刻是翻译 / OCR 视图。输入会话的 prepare
 * 按翻译预估（Rust 不知道前端什么时候换视图）：还在输入视图时不提供，提交后换成翻译视图就有
 */
function canRead(prepare: PanelPrepare | null, payload: PanelPayload | null): boolean {
  return prepare?.reading != null && (payload?.kind === 'translate' || payload?.kind === 'ocr')
}

/** 进行中的展开 / 收起：淡出完成、`panel_set_reading` 也返回之后提交（结果为 null 时什么都不改，直接淡入） */
interface ReadingFlight {
  session: number
  /** undefined = 还在等 Rust 的返回 */
  result: PanelReading | null | undefined
  faded: boolean
}

/**
 * 把元素的不透明度动画到 `opacity`，返回完成的 Promise。没有 WAAPI 的环境（jsdom）直接落到终值；元素不在（视图还没
 * 渲染）时立即完成
 */
function fadeTo(el: HTMLElement | null, opacity: 0 | 1, reduced: boolean): Promise<void> {
  if (!el) return Promise.resolve()
  if (typeof el.animate !== 'function') {
    if (opacity === 1) el.style.removeProperty('opacity')
    else el.style.opacity = '0'
    return Promise.resolve()
  }
  const transition = reduced
    ? presets.reduced.fade
    : opacity === 0
      ? presets.pageSwitch.exit.transition
      : presets.crossfade
  return animate(el, { opacity }, transition).then(() => {
    // 回到 1 之后去掉内联的 opacity：不留合成层，文字照常按子像素抗锯齿绘制
    if (opacity === 1) el.style.removeProperty('opacity')
  })
}

interface LabModule {
  FrameLab: ComponentType<FrameLabProps>
  FrameLabMarker: ComponentType
}

/** 实验台组件（开发者工具）：页面加载时就开始动态加载，保证第一次 prepare 能同步渲染 */
let lab: LabModule | null = null
if (frameLab) {
  void import('./FrameLab').then((mod) => {
    lab = mod
  })
}

export function PanelShell() {
  const contentRef = useRef<HTMLDivElement>(null)
  // 布局高度：prepare 时 Css 卡片处在进入动画的初始缩放（0.96），getBoundingClientRect 会少算 4%
  const measure = useCallback(() => contentRef.current?.offsetHeight ?? NaN, [])
  const { requestHide, completeExit, completeEnter } = usePanelLifecycle(measure)
  useEffect(() => {
    // 无条件订阅 panel:latency（只是一个事件监听，空闲零开销）：开发者模式可以在运行时打开，
    // 打开后的第一次唤出就能显示埋点
    startLatencyFeed()
    // 面板窗口在启动时预热创建、处于隐藏状态：一开始就标记隐藏（CSS 动画暂停、长时间隐藏后释放资源）
    if (usePanelStore.getState().phase === 'hidden') setWindowHidden(true)
    // 空闲时付掉字体与 Intl 的一次性开销（shared/perf/warmup），不让第一次唤出 / 切换视图把它带进交互中的那一帧
    return warmUpWhenIdle()
  }, [])
  const prepare = usePanelStore((s) => s.prepare)
  const payload = usePanelStore((s) => s.payload)
  const phase = usePanelStore((s) => s.phase)
  const focused = usePanelStore((s) => s.focused)
  const width = usePanelStore((s) => s.width)
  const nextWidth = usePanelStore((s) => s.nextWidth)
  const maxHeight = usePanelStore((s) => s.maxHeight)
  const reading = usePanelStore((s) => s.reading)
  const epoch = usePanelStore((s) => s.epoch)
  const viewKey = usePanelStore((s) => s.viewKey)
  const sessionId = prepare?.session_id ?? null
  const frame = boot.frame_mode
  const opaque = isOpaqueWindow()
  const live = phase === 'entering' || phase === 'shown'
  const reduced = useReducedMotionPref()
  // 浮层预留的状态：卡片经回调写、高度跟随读（Native 帧据此给 panel_resize 指定 snappy）
  const resizeHint = useRef<ResizeHint>({ reserved: 0, releaseUntil: 0 })
  const onReserveChange = useCallback(
    (reserved: number) => noteReserve(resizeHint.current, reserved),
    [],
  )
  const {
    cardHeight,
    onHeightSettled,
    flush,
    edge: windowEdge,
  } = useHeightFollow(
    contentRef,
    sessionId,
    live,
    frame === 'native' ? 'native' : opaque ? 'opaque' : 'css',
    prepare ? maxHeight : undefined,
    reduced,
    resizeHint,
  )
  useLayoutEffect(() => {
    const root = document.documentElement
    if (opaque) root.dataset.opaque = ''
    else delete root.dataset.opaque
  }, [opaque])

  useKeyScope(
    'panel',
    [
      { combo: 'Escape', allowInInput: true, run: () => requestHide('escape') },
      { combo: 'Mod+KeyW', allowInInput: true, run: () => requestHide('command_w') },
      {
        combo: 'Mod+Comma',
        allowInInput: true,
        run: () => void api.windowOpen({ kind: 'settings', section: null }),
      },
      // Tab 在卡片内循环：每一次 Tab / Shift+Tab 都由外壳接管（shared/keys/focusLoop：按文档顺序经过卡片里
      // 的全部控件、首尾回绕），不交给浏览器——WebKit 默认的 Tab 会跳过按钮
      { combo: 'Tab', allowInInput: true, run: () => cycleFocusWithin(contentRef.current, false) },
      {
        combo: 'Shift+Tab',
        allowInInput: true,
        run: () => cycleFocusWithin(contentRef.current, true),
      },
    ],
    { layer: 'global', enabled: live },
  )

  const switchView = useCallback((next: PanelPayload, options?: { width?: number | null }) => {
    usePanelStore.getState().switchPayload(next, options?.width)
  }, [])
  const activate = useCallback(() => {
    if (sessionId !== null) void api.panelActivate(sessionId)
  }, [sessionId])
  // 视图交换（ViewSwitch）：交换的那次提交里让等待中的新宽度生效，并告诉 Rust 改窗口宽度（此刻旧内容已完全透明、
  // 新视图还是 opacity 0；Rust 发起的交叉淡入在 ready 时已按新宽度安排，这里的调用是空操作）；提交之后立即
  // 上报新内容的高度，不等 ResizeObserver 的下一帧（prepare 时报的是旧视图的高度，见 usePanelLifecycle）
  const commitWidth = useCallback(() => {
    const store = usePanelStore.getState()
    const next = store.nextWidth
    store.commitWidth()
    const session = store.prepare?.session_id
    if (next !== null && session !== undefined) void api.panelSetWidth(session, next)
  }, [])

  // 展开 / 收起阅读（见文件头）。淡出的是视图切换层的根（ViewSwitch），卡片与开发者模式的那一行不动
  const switchRef = useRef<HTMLDivElement>(null)
  const flight = useRef<ReadingFlight | null>(null)
  const reducedRef = useRef(reduced)
  useLayoutEffect(() => {
    reducedRef.current = reduced
  }, [reduced])
  const toggleReading = useCallback(() => {
    const store = usePanelStore.getState()
    const session = store.prepare?.session_id
    if (session === undefined || !canRead(store.prepare, store.payload) || flight.current) return
    if (store.phase !== 'entering' && store.phase !== 'shown') return
    const on = store.reading === null
    const f: ReadingFlight = { session, result: undefined, faded: false }
    flight.current = f
    // 阅读与核对是展开的本意：面板取得键盘焦点（键盘滚动、快捷键、Esc 都落到面板里）
    if (on) void api.panelActivate(session)
    const settle = () => {
      if (flight.current !== f || !f.faded || f.result === undefined) return
      flight.current = null
      const now = usePanelStore.getState()
      const result = f.result
      const showing = now.phase === 'entering' || now.phase === 'shown'
      if (result && showing && now.prepare?.session_id === session) {
        flushSync(() => now.commitReading(result))
        flush()
      }
      void fadeTo(switchRef.current, 1, reducedRef.current)
    }
    api.panelSetReading(session, on).then(
      (result) => {
        f.result = result
        settle()
      },
      () => {
        f.result = null
        settle()
      },
    )
    void fadeTo(switchRef.current, 0, reducedRef.current).then(() => {
      f.faded = true
      settle()
    })
  }, [flush])
  // Esc 先收起展开阅读：正在切换的那一下也吞掉（再按一次 Esc 才关闭）；不在展开阅读时照常关闭
  useEffect(
    () =>
      setEscapeHook(() => {
        if (flight.current) return true
        if (usePanelStore.getState().reading === null) return false
        toggleReading()
        return true
      }),
    [toggleReading],
  )

  if (!prepare || !payload || phase === 'hidden') return null
  const Lab = frameLab ? lab : null
  const render = __WISP_MOCK__ ? (viewOverride() ?? undefined) : undefined
  // 开发者模式的延迟 / 帧监视：内容之后单独一行（参与布局、从 prepare 起就占位，打开后不会长高一下），不再浮在卡片上
  // 盖住底栏的模型名、操作按钮与提示条；toast 不显示
  const devHud = (boot.dev || frameMonitorEnabled()) && payload.kind !== 'toast'
  const viewMaxHeight = maxHeight - (devHud ? DEV_HUD_HEIGHT : 0)
  return (
    <PanelCard
      key={epoch}
      frame={frame}
      phase={phase}
      width={width}
      maxHeight={maxHeight}
      placement={prepare.placement}
      originX={prepare.origin_x}
      side={prepare.side}
      shadowMargin={boot.shadow_margin}
      height={cardHeight}
      windowEdge={windowEdge}
      contentRef={contentRef}
      onHeightSettled={onHeightSettled}
      onEntered={completeEnter}
      onExited={completeExit}
      cssFill={frameLab?.fill === 'css'}
      onReserveChange={onReserveChange}
      overlay={Lab && frameLab?.marker ? <Lab.FrameLabMarker /> : null}
      footer={
        devHud ? (
          <div className="dev-hud" aria-hidden="true" style={{ height: DEV_HUD_HEIGHT }}>
            {boot.dev ? <DevLatency /> : null}
            {/* 采样只在可见时进行（隐藏 / prepare 时不跑 rAF） */}
            {live ? <FrameMonitorView className="dev-frames" /> : null}
          </div>
        ) : null
      }
    >
      {Lab ? (
        <Lab.FrameLab phase={phase} requestHide={requestHide} />
      ) : (
        <ViewSwitch
          rootRef={switchRef}
          viewKey={viewKey}
          widthPending={nextWidth !== null}
          stageWidthDelta={frame === 'css' && !opaque && nextWidth !== null ? nextWidth - width : 0}
          onSwap={commitWidth}
          onSwapped={flush}
          render={render}
          sessionId={prepare.session_id}
          payload={payload}
          phase={phase}
          focused={focused}
          host="panel"
          frame={frame}
          placement={prepare.placement}
          maxHeight={viewMaxHeight}
          reading={reading}
          toggleReading={canRead(prepare, payload) ? toggleReading : undefined}
          requestHide={requestHide}
          switchView={switchView}
          activate={activate}
        />
      )}
    </PanelCard>
  )
}
