/**
 * 固定窗口。
 *
 * 复用翻译视图 / OCR 视图（`host = 'pinned'`、`sessionId = 0`），卡片视觉与面板一致（圆角、hairline、
 * 排版；背景纯色，不用材质）。Linux 的固定窗口是透明窗口（`Boot.shadow_margin` > 0）：卡片四周留阴影边距，自己画圆角、
 * 描边与 `--shadow-pinned`；macOS / Windows 的窗口边框、圆角与阴影由系统提供（shadow_margin = 0），卡片铺满窗口。
 *
 * 首帧交接（无空帧）：`pinned_init` → 视图同步渲染首帧（已结束的结果直接写入；仍在进行的请求先 `stream_attach`
 * 回放完）并调用 `ready()` → 同步测量卡片的自然高度 → `pinned_ready(id, content_height)`（Rust 在命令内同一帧显示固定
 * 窗口、隐藏面板；展开 ⤢ 的窗口按 min(内容, 屏幕 70%) 定高）→ 返回后卡片「浮起」：presets.pinLift（y −2→0，
 * spring.gentle）+ 带 `--shadow-pinned` 的阴影层淡入。首帧不依赖 rAF / ResizeObserver / setTimeout。
 *
 * 窗口控件：整条头部可拖动（`data-tauri-drag-region`）、置顶开关（默认置顶）、✕；获焦时 Esc / ⌘W 关闭（即销毁，
 * 编排器取消归属该窗口的流）。翻译视图把这些控件并进自己的头部（usePinnedChrome），只占一条头部。
 */
import { Pin, PinOff, X } from 'lucide-react'
import { m } from 'motion/react'
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { useT } from '../shared/i18n/useT'
import { api, boot, getTransport, pinnedIdOf } from '../shared/ipc'
import type { PanelPayload, PinState } from '../shared/ipc/types'
import { cycleFocusWithin } from '../shared/keys/focusLoop'
import { useKeyScope } from '../shared/keys/useKeyScope'
import { MotionRoot } from '../shared/motion/MotionRoot'
import { presets } from '../shared/motion/presets'
import { useReducedMotionPref } from '../shared/motion/useReducedMotionPref'
import { IconButton } from '../shared/ui'
import type { PanelPhase, ViewHost } from '../panel/types'
import { OcrView } from '../views/ocr'
import { TranslateView } from '../views/translate'
import { loadTranslateMessages } from '../views/translate/i18n'
import { PinnedChromeContext, PinnedContext, type PinnedHost } from './context'
import './pinned.css'

loadTranslateMessages()

/** 独立标题栏的高度（视图没有把窗口控件并进自己的头部时：36 px 整条拖拽区） */
const HEADER_HEIGHT = 36
/**
 * 展开 ⤢ 的窗口过渡：交接之后这么久还没长到展开宽度就不再等，卡片放开宽度、铺满窗口（ms，不是动效时长：Rust 在
 * pinned_ready 里立即开始展开，弹簧约 0.4–0.6 s；只是窗口没能展开时的兜底）
 */
const EXPAND_SETTLE_MS = 1500

/**
 * 固定窗口里视图的载荷：由 PinState 还原。OCR 的缩略图与原图尺寸来自面板交接的 `thumb_url` / `image_size`；
 * 没有尺寸时为 0 × 0（MediaHead 在图片加载后取自然尺寸）
 */
export function payloadOf(state: PinState): PanelPayload {
  if (state.kind === 'ocr') {
    return {
      kind: 'ocr',
      capture_id: state.capture_id ?? '',
      request_id: state.request_id,
      mode: state.ocr_mode ?? 'lines',
      thumb_url: state.thumb_url ?? '',
      image_size: state.image_size ?? { width: 0, height: 0 },
      target_lang: state.target_lang,
      not_ready: null,
      // 固定窗口里的排版沿用面板交接时的那个；旧面板没有交接时按「原样」
      layout: state.ocr_layers?.layout ?? 'keep',
    }
  }
  return {
    kind: 'translate',
    source_text: state.source_text,
    request_id: state.request_id,
    task: state.task,
    style_id: state.style_id,
    detected_lang: state.detected_lang,
    target_lang: state.target_lang,
    dict_mode: state.dict_mode ?? false,
    long_text: null,
    not_ready: null,
    selection_method: null,
  }
}

/** 根元素（窗口内容区）的尺寸：Tauri 里等于窗口尺寸；浏览器 mock 里是模拟窗口的尺寸 */
function rootSize(): { width: number; height: number } {
  const root = document.getElementById('root')
  return {
    width: root?.clientWidth || window.innerWidth,
    height: root?.clientHeight || window.innerHeight,
  }
}

function closeWindow(): void {
  void getTransport().window.close()
}

export function PinnedApp() {
  const pinnedId = pinnedIdOf(boot.label)
  const [state, setState] = useState<PinState | null>(null)
  const [failed, setFailed] = useState(false)
  useEffect(() => {
    if (!pinnedId) return
    void api.pinnedInit(pinnedId).then(setState, () => setFailed(true))
  }, [pinnedId])
  return (
    <MotionRoot>
      {state && pinnedId ? (
        <PinnedWindow pinnedId={pinnedId} state={state} />
      ) : failed ? (
        <PinnedFallback />
      ) : null}
    </MotionRoot>
  )
}

function useWindowFocus(): boolean {
  const [focused, setFocused] = useState(() => document.hasFocus())
  useEffect(() => {
    const on = () => setFocused(true)
    const off = () => setFocused(false)
    window.addEventListener('focus', on)
    window.addEventListener('blur', off)
    return () => {
      window.removeEventListener('focus', on)
      window.removeEventListener('blur', off)
    }
  }, [])
  return focused
}

function PinnedWindow({ pinnedId, state }: { pinnedId: string; state: PinState }) {
  const t = useT()
  const reduced = useReducedMotionPref()
  const focused = useWindowFocus()
  const cardRef = useRef<HTMLDivElement>(null)
  const [phase, setPhase] = useState<PanelPhase>('preparing')
  // 置顶的初值由 Rust 给出（新窗口取上一次的设置，页面重载后取窗口的当前状态）
  const [onTop, setOnTop] = useState(state.on_top ?? true)
  // 翻译视图把窗口控件并进自己的头部；其他视图（OCR）声明之前由这里画一条独立标题栏
  const [claimed, setClaimed] = useState(state.kind === 'translate')
  const [size, setSize] = useState(rootSize)
  const margin = boot.shadow_margin
  // 展开 ⤢：展开后卡片的宽度（Rust 在 pinned_init 里给出，已按工作区收窄；普通固定为 null）。首帧按它测量（窗口还是
  // 面板的宽度），窗口追上这个宽度之前卡片保持它
  const [expandWidth, setExpandWidth] = useState(() => state.expand_width ?? null)
  const cardWidth = size.width - 2 * margin
  if (expandWidth !== null && phase === 'shown' && cardWidth >= expandWidth - 1)
    setExpandWidth(null)
  // 兜底：窗口到时还没长够（展开没有成功），放开卡片宽度、铺满窗口，不让卡片右侧被裁掉
  const expanding = expandWidth !== null && phase === 'shown'
  useEffect(() => {
    if (!expanding) return
    const giveUp = setTimeout(() => setExpandWidth(null), EXPAND_SETTLE_MS)
    return () => clearTimeout(giveUp)
  }, [expanding])

  // 窗口尺寸变化（用户缩放、展开过渡）：只在真的变化时回调，空闲时没有任何计时器
  useEffect(() => {
    const root = document.getElementById('root')
    if (!root) return
    const observer = new ResizeObserver(() => setSize(rootSize()))
    observer.observe(root)
    return () => observer.disconnect()
  }, [])

  const readyDone = useRef(false)
  const readyHeld = useRef(false)
  const ready = useCallback(() => {
    if (readyDone.current) return
    readyDone.current = true
    // 首帧（opacity 1，窗口仍隐藏）同步测量卡片的自然高度；测量非法时退回一个合理的最小高度
    const measured = cardRef.current?.offsetHeight ?? NaN
    const contentHeight =
      Number.isFinite(measured) && measured > 0 ? Math.ceil(measured) : HEADER_HEIGHT * 4
    const lift = () => {
      setPhase('shown')
    }
    void api.pinnedReady(pinnedId, contentHeight).then(lift, lift)
  }, [pinnedId])

  // 置顶：开关立即响应；失败时回到原状态（与窗口实际状态一致）。新值由 Rust 记为下一个固定窗口的初值。
  // 副作用不放进 setState 的 updater（StrictMode 下 updater 会执行两次）
  const toggleOnTop = () => {
    const wanted = !onTop
    setOnTop(wanted)
    api.pinnedSetOnTop(pinnedId, wanted).catch(() => setOnTop(!wanted))
  }

  const chrome: ReactNode = (
    <>
      {/* 置顶：Pin（强调色、很淡的底色）/ PinOff（描边），与底栏的复制图标外形区分得开 */}
      <IconButton
        icon={onTop ? Pin : PinOff}
        label={t('translate.pinned.on_top')}
        active={onTop}
        onClick={toggleOnTop}
        className="pinned-ontop"
        data-testid="pinned-on-top"
      />
      <IconButton
        icon={X}
        label={t('translate.pinned.close')}
        shortcut="Escape"
        onClick={closeWindow}
        data-testid="pinned-close"
      />
    </>
  )

  const host = useMemo<PinnedHost>(
    () => ({
      pinnedId,
      state,
      ready,
      holdReady: () => {
        readyHeld.current = true
      },
      claimChrome: () => setClaimed(true),
    }),
    [pinnedId, state, ready],
  )

  // 视图没有声明自己负责首帧就绪（usePinnedReadyHold）：挂载后（视图的布局 effect 已经写完首帧）由这里 ready
  useEffect(() => {
    if (!readyHeld.current) ready()
  }, [ready])

  useKeyScope(
    'pinned',
    [
      { combo: 'Escape', allowInInput: true, run: closeWindow },
      { combo: 'Mod+KeyW', allowInInput: true, run: closeWindow },
      {
        combo: 'Mod+Comma',
        allowInInput: true,
        run: () => void api.windowOpen({ kind: 'settings', section: null }),
      },
      // Tab 在卡片内循环（与面板一致）
      { combo: 'Tab', allowInInput: true, run: () => cycleFocusWithin(cardRef.current, false) },
      {
        combo: 'Shift+Tab',
        allowInInput: true,
        run: () => cycleFocusWithin(cardRef.current, true),
      },
    ],
    { layer: 'global' },
  )

  const viewHost: ViewHost = 'pinned'
  const shown = phase === 'shown'
  // 视图可用的高度：窗口内容区 − 阴影边距 − 独立标题栏
  const maxHeight = Math.max(0, size.height - 2 * margin - (claimed ? 0 : HEADER_HEIGHT))
  // 浮起：首帧停在起点（与窗口出现的那一帧一致），交接之后动画到 0；减少动效时不位移（阴影层照常淡入）
  const liftFrom = reduced ? 0 : presets.pinLift.y[0]
  const payload = useMemo(() => payloadOf(state), [state])
  const viewProps = {
    sessionId: 0,
    phase,
    focused,
    host: viewHost,
    frame: boot.frame_mode,
    placement: 'center' as const,
    maxHeight,
    requestHide: closeWindow,
    switchView: () => undefined,
    activate: () => undefined,
  }

  return (
    <main
      className="pinned"
      data-margin={margin > 0 || undefined}
      data-testid="pinned-app"
      data-kind={state.kind}
    >
      <m.div
        ref={cardRef}
        className="pinned-card"
        data-phase={phase}
        data-lifted={shown || undefined}
        data-expanding={expandWidth !== null || undefined}
        data-testid="pinned-card"
        initial={false}
        animate={{ y: shown ? presets.pinLift.y[1] : liftFrom }}
        transition={presets.pinLift.transition}
        style={expandWidth !== null ? { width: expandWidth } : undefined}
      >
        {claimed ? null : (
          <header className="pinned-header" data-tauri-drag-region>
            <span className="pinned-title" data-tauri-drag-region>
              {t(`translate.pinned.title.${state.kind}`)}
            </span>
            <span className="pinned-spacer" data-tauri-drag-region />
            {chrome}
          </header>
        )}
        <div className="pinned-content">
          <PinnedContext value={host}>
            <PinnedChromeContext value={chrome}>
              {payload.kind === 'translate' ? (
                <TranslateView {...viewProps} payload={payload} />
              ) : payload.kind === 'ocr' ? (
                <OcrView {...viewProps} payload={payload} />
              ) : null}
            </PinnedChromeContext>
          </PinnedContext>
        </div>
      </m.div>
    </main>
  )
}

/** pinned_init 失败（窗口状态已经不在）：只留一个可以关闭的空卡片，不让窗口一片空白 */
function PinnedFallback() {
  const t = useT()
  useKeyScope('pinned', [{ combo: 'Escape', allowInInput: true, run: closeWindow }], {
    layer: 'global',
  })
  return (
    <main
      className="pinned"
      data-margin={boot.shadow_margin > 0 || undefined}
      data-testid="pinned-app"
    >
      <div className="pinned-card" data-lifted>
        <header className="pinned-header" data-tauri-drag-region>
          <span className="pinned-spacer" data-tauri-drag-region />
          <IconButton icon={X} label={t('translate.pinned.close')} onClick={closeWindow} />
        </header>
      </div>
    </main>
  )
}
