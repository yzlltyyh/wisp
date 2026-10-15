/**
 * 面板外壳：会话 + 同步 prepare + 两种帧模式的卡片、进入 / 退出与高度跟随 +
 * 视图交叉淡入 + 全局键盘作用域 + 开发者模式的延迟埋点与帧监视。
 *
 * 全局键（global 层，视图作用域之后才轮到）：Esc 关闭、⌘W / Ctrl+W 关闭、⌘, / Ctrl+, 打开设置。
 * 隐藏后整棵视图树卸载（`phase = hidden` 时不渲染卡片），rAF / Observer / 计时器随之停止。
 * 窗口不能透明（Css 帧模式且 `shadow_margin == 0`，例如 Wayland）时 `<html data-opaque>`：页面底色取
 * `--surface-solid`（base.css），卡片铺满窗口、不做进入 / 退出的透明度与变换（PanelCard），高度变化立即上报。
 */
import { useCallback, useEffect, useLayoutEffect, useRef, type ComponentType } from 'react'
import { api, boot } from '../../shared/ipc'
import type { PanelPayload } from '../../shared/ipc/types'
import { cycleFocusWithin } from '../../shared/keys/focusLoop'
import { useKeyScope } from '../../shared/keys/useKeyScope'
import { FrameMonitorView } from '../../shared/perf/FrameMonitorView'
import { frameMonitorEnabled } from '../../shared/perf/frameMonitor'
import { warmUpWhenIdle } from '../../shared/perf/warmup'
import { setWindowHidden } from '../../shared/hooks/useHidden'
import { useReducedMotionPref } from '../../shared/motion/useReducedMotionPref'
import { DevLatency, startLatencyFeed } from './DevLatency'
import { viewOverride } from './devHost'
import type { FrameLabProps } from './FrameLab'
import { frameLab } from './frameLabConfig'
import { PanelCard } from './PanelCard'
import { usePanelStore } from './panelStore'
import { noteReserve, useHeightFollow, type ResizeHint } from './useHeightFollow'
import { isOpaqueWindow, usePanelLifecycle } from './usePanelLifecycle'
import { ViewSwitch } from './ViewSwitch'

/** 开发者模式的延迟 / 帧监视行的高度（逻辑 px）：视图拿到的 maxHeight 扣掉它，内容 + 这一行不超过卡片上限 */
export const DEV_HUD_HEIGHT = 18

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
    prepare?.max_height,
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

  if (!prepare || !payload || phase === 'hidden') return null
  const Lab = frameLab ? lab : null
  const render = __WISP_MOCK__ ? (viewOverride() ?? undefined) : undefined
  // 开发者模式的延迟 / 帧监视：内容之后单独一行（参与布局、从 prepare 起就占位，打开后不会长高一下），不再浮在卡片上
  // 盖住底栏的模型名、操作按钮与提示条；toast 不显示
  const devHud = (boot.dev || frameMonitorEnabled()) && payload.kind !== 'toast'
  const viewMaxHeight = prepare.max_height - (devHud ? DEV_HUD_HEIGHT : 0)
  return (
    <PanelCard
      key={epoch}
      frame={frame}
      phase={phase}
      width={width}
      maxHeight={prepare.max_height}
      placement={prepare.placement}
      originX={prepare.origin_x}
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
          requestHide={requestHide}
          switchView={switchView}
          activate={activate}
        />
      )}
    </PanelCard>
  )
}
