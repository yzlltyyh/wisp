/**
 * OCR 视图的组成部分：头部（缩略图 + 尺寸 / 模式 / 模型 + 关闭）、放大的截图（浮层）、分体按钮（主操作 + ▾）、
 * 流式区（骨架 → 流式 → 完成）、底栏、展开区与正文区外层。结构、间距与控件对齐视觉稿 design.ocr-recognizing /
 * design.ocr-done。
 *
 * 高度：视图按内容的自然高度排版，以卡片上限为 max-height（固定窗口铺满窗口），两区怎么分由 CSS
 * flex 在同一次布局里完成（ocr.css）。卡片 / 窗口的高度跟随（PanelCard 的 spring.gentle、Native 的 Rust 弹簧）负责
 * 「内容变高 / 变矮」；视图里只有两处自己的高度过渡，都与那个弹簧同值（HEIGHT_SPRING）：
 * - 卡片已到上限时展开区的展开 / 收起与提交引起的高度变化（正文区随 flex 逐帧让出 / 拿回，两者之和不变）；
 * - 内容变矮、卡片随后收缩时，下方的内容（底栏、区底状态行）用 FlipBox 贴着卡片底边一起移动，不会先跳上去、露出一截
 *   空白再等卡片追上来。
 */
import { ChevronDown, Maximize2, X } from 'lucide-react'
import { m, useAnimationControls, useMotionValue, type Transition } from 'motion/react'
import {
  useEffect,
  useImperativeHandle,
  useLayoutEffect,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
  type ReactNode,
  type Ref,
  type RefObject,
} from 'react'
import { useViewActive } from '../../shared/hooks/useViewActive'
import { usePlatform } from '../../shared/hooks/usePlatform'
import { useT } from '../../shared/i18n/useT'
import { resolveAssetUrl, type OcrPayload } from '../../shared/ipc'
import { presets, usePreset } from '../../shared/motion/presets'
import { useReducedMotionPref } from '../../shared/motion/useReducedMotionPref'
import type { StreamFormat, StreamHandle } from '../../shared/stream/StreamView'
import { StreamView } from '../../shared/stream/StreamView'
import { Icon, IconButton, Popover, Skeleton, Thumbnail, Tooltip, cx } from '../../shared/ui'
import { syncFadeEdges } from '../../shared/ui/ScrollArea'
import type { Run } from './useRun'

/** 展开区展开完的通知（区里的 SmoothBox 据此接手流式跟随） */
const SECTION_SIZED = 'ocr-section-sized'

/** 视图内的高度过渡：与卡片 / 窗口的高度跟随同一个弹簧（presets.sectionExpand 的高度过渡即 spring.gentle） */
export const HEIGHT_SPRING: Transition = presets.sectionExpand.animate.transition.height

// ---------------------------------------------------------------------------
// 头部

/** 缩略图框的长宽比范围：超出时按「细条」或「长条」显示 */
const THUMB_ASPECT_MIN = 0.6
const THUMB_ASPECT_MAX = 3.4
/** 极宽的截图（整行文字）：缩略图是一条这么高的横条，文字接近原始大小、右侧渐隐表示还有内容 */
const THUMB_STRIP = 32
/**
 * 截图的像素尺寸（面板载荷；固定窗口由 PinState.image_size 交接）。未知（0 × 0）时为 null：
 * 缩略图按方框占位，头部高度仍按高度上限，不跳
 */
/**
 * 物理像素 → 逻辑尺寸（pt）：按面板所在显示器的缩放换算。面板出现在截图的那块屏上（锚在选区附近），与 Overlay 换算
 * 尺寸标签用的是同一个缩放；载荷只带物理尺寸
 */
function logicalSize(size: { width: number; height: number }): { width: number; height: number } {
  const scale =
    typeof window !== 'undefined' && window.devicePixelRatio > 0 ? window.devicePixelRatio : 1
  return { width: Math.round(size.width / scale), height: Math.round(size.height / scale) }
}

function imageSizeOf(payload: OcrPayload): { width: number; height: number } | null {
  const { width, height } = payload.image_size
  return width > 0 && height > 0 ? payload.image_size : null
}

export interface MediaHeadProps {
  payload: OcrPayload
  /** 第二行：「纯文本 · qwen3-vl-plus」/「识别中…」 */
  status: ReactNode
  expanded: boolean
  onToggleImage: () => void
  thumbMaxWidth: number
  /** 缩略图的高度上限（卡片上限低时更细） */
  thumbHeight: number
  /** 面板宿主才有关闭按钮（固定窗口用 PinnedApp 的标题栏） */
  onClose?: (() => void) | null
  /** 关闭按钮之前的 ⋯ 菜单（面板宿主：重新截图 / 识别历史… / OCR 设置…） */
  more?: ReactNode
  /** 缩略图与说明那一块（放大的截图以它为锚点） */
  mediaRef?: Ref<HTMLDivElement>
}

export function MediaHead({
  payload,
  status,
  expanded,
  onToggleImage,
  thumbMaxWidth,
  thumbHeight,
  onClose,
  more,
  mediaRef,
}: MediaHeadProps) {
  const t = useT()
  const os = usePlatform()
  const size = imageSizeOf(payload)
  const hasThumb = payload.thumb_url !== ''
  // 极端长宽比：整行文字的细条显示成一条 32 px 高的横条（等比、左对齐、右侧渐隐），看得出截的是哪一行；
  // 长截图显示成 0.6 比例的竖条（顶端对齐、底部渐隐）。点击后弹出完整截图
  const aspect = size ? size.width / size.height : 1
  const wide = aspect > THUMB_ASPECT_MAX
  const tall = aspect < THUMB_ASPECT_MIN
  // 尺寸未知（不应出现：载荷与 PinState 都带尺寸）时按方框占满高度上限
  const natural = !size
    ? { width: thumbHeight, height: thumbHeight }
    : wide
      ? { width: Math.min(thumbMaxWidth, THUMB_STRIP * aspect), height: THUMB_STRIP }
      : tall
        ? { width: thumbHeight * THUMB_ASPECT_MIN, height: thumbHeight }
        : size
  return (
    <div className="ocr-head" data-testid="ocr-head" data-bare={hasThumb ? undefined : ''}>
      <div ref={mediaRef} className="ocr-media">
        {!hasThumb && !size && !status ? (
          <span className="ocr-title">{t('panel.view.ocr')}</span>
        ) : null}
        {hasThumb ? (
          // 自己的按钮（而不是 Thumbnail 的 onOpen）：要带 aria-expanded / aria-haspopup，并在悬停 / 聚焦时给出「放大」
          // 的提示（macOS 非激活面板在应用不在前台时不一定更新光标，只靠 zoom-in 光标很难被发现）
          <button
            type="button"
            className="ui-thumb-button ocr-thumb-button"
            aria-label={t(expanded ? 'ocr.a11y.collapse_thumbnail' : 'ocr.a11y.expand_thumbnail')}
            aria-haspopup="dialog"
            aria-expanded={expanded}
            data-open={expanded || undefined}
            onClick={onToggleImage}
            data-testid="ocr-thumbnail"
          >
            <Thumbnail
              src={resolveAssetUrl(payload.thumb_url)}
              alt={t('ocr.a11y.thumbnail')}
              naturalSize={natural}
              maxHeight={wide ? THUMB_STRIP : thumbHeight}
              maxWidth={thumbMaxWidth}
              className={wide ? 'ocr-thumb-strip' : tall ? 'ocr-thumb-tall' : undefined}
              data-testid="ocr-thumbnail-image"
            />
            <span className="ocr-thumb-zoom" aria-hidden="true">
              <Icon icon={Maximize2} size={12} />
            </span>
          </button>
        ) : null}
        <span className="ocr-meta">
          {size ? (
            // 逻辑尺寸，与框选时 Overlay 的尺寸标签同一单位——Retina 上物理像素是它的 2 倍，两处数字
            // 不一致会被当成截错了范围；物理像素放进 tooltip。单位按平台写：macOS 叫 pt，Windows / Linux 叫 px（数值同为
            // 逻辑单位）
            <Tooltip
              content={t('ocr.meta.size_physical', { width: size.width, height: size.height })}
            >
              <span data-testid="ocr-size">
                {t(os === 'macos' ? 'ocr.meta.size_pt' : 'ocr.meta.size_px', logicalSize(size))}
              </span>
            </Tooltip>
          ) : null}
          {status ? (
            <span className="ocr-meta-status" data-testid="ocr-meta-status">
              {status}
            </span>
          ) : null}
        </span>
      </div>
      <span className="ocr-spacer" />
      {more}
      {onClose ? (
        <IconButton
          icon={X}
          label={t('common.action.close')}
          shortcut="Escape"
          onClick={onClose}
          data-testid="ocr-close"
        />
      ) : null}
    </div>
  )
}

/**
 * 放大的截图：锚在缩略图下方的浮层（presets.previewIn，减少动效时纯淡入淡出），不参与排版——下面的文字与底栏不动，
 * 卡片按浮层需要的高度长高（FloatingBoundary 预留），再点或 Esc 收起
 */
export function ZoomImage({
  payload,
  open,
  anchor,
  onClose,
}: {
  payload: OcrPayload
  open: boolean
  anchor: RefObject<HTMLElement | null>
  onClose: () => void
}) {
  const t = useT()
  const { width, height } = payload.image_size
  return (
    <Popover
      open={open}
      onClose={onClose}
      anchor={anchor}
      preview
      label={t('ocr.a11y.thumbnail')}
      className="ocr-zoom-pop"
      data-testid="ocr-zoom"
    >
      <button
        type="button"
        className="ocr-zoom"
        onClick={onClose}
        aria-label={t('ocr.a11y.collapse_thumbnail')}
      >
        <img
          src={resolveAssetUrl(payload.thumb_url)}
          alt={t('ocr.a11y.thumbnail')}
          draggable={false}
          data-releasable=""
          style={{ aspectRatio: width > 0 && height > 0 ? `${width} / ${height}` : undefined }}
        />
      </button>
    </Popover>
  )
}

// ---------------------------------------------------------------------------
// 分体按钮：[主操作 | ▾]

/**
 * 外框：一个圆角胶囊，两半之间 1 px 分隔，两半的悬停 / 按下底色在框内裁切。静止时不画描边（与视觉稿的轻量底栏一致），
 * 悬停、聚焦、菜单打开、选中时才显示整体外框；键盘焦点环画在外框上表示「焦点在这一组」，**被聚焦的那一半另有自己的
 * 内环与强调色**，Tab 在两半之间移动时看得出焦点在主操作还是 ▾ 上
 */
export function Split({
  children,
  ref,
  selected = false,
  open = false,
  'data-testid': testId,
}: {
  children: ReactNode
  ref?: Ref<HTMLSpanElement>
  selected?: boolean
  open?: boolean
  'data-testid'?: string
}) {
  return (
    <span
      ref={ref}
      className="ocr-split"
      data-selected={selected || undefined}
      data-open={open || undefined}
      data-testid={testId}
    >
      {children}
    </span>
  )
}

export interface SplitCaretProps {
  label: string
  disabled?: boolean
  ref?: Ref<HTMLButtonElement>
  onClick: (e: ReactMouseEvent) => void
  /** 按下就打开的菜单（组件库 StyleMenu / menuOpener） */
  onPointerDown?: (e: ReactPointerEvent) => void
  onKeyDown?: (e: ReactKeyboardEvent<HTMLElement>) => void
  'aria-haspopup'?: 'menu'
  'aria-expanded'?: boolean
  'data-open'?: true | undefined
  'data-testid'?: string
}

/** 分体按钮的 ▾（24 px 宽，命中区不小于最小目标尺寸） */
export function SplitCaret({ label, disabled, 'data-testid': testId, ...rest }: SplitCaretProps) {
  return (
    <IconButton
      {...rest}
      icon={ChevronDown}
      label={label}
      disabled={disabled}
      className="ocr-caret"
      glyph={<Icon icon={ChevronDown} size={12} />}
      data-testid={testId}
    />
  )
}

// ---------------------------------------------------------------------------
// 流式区

export interface RunBodyProps {
  run: Run
  format: StreamFormat
  /** 给出后成为区内滚动容器（实际可见高度由 flex 决定，见 ocr.css） */
  maxHeight?: number
  /** 等第一个 Delta 时的骨架行数 */
  skeleton?: number
  /** 切换风格 / 重新识别时：新结果到达前显示的旧文字（变淡 + 模糊 + 骨架同节奏的脉冲） */
  ghost?: string | null
  /** 等待中、没有旧文字时的提示（例如「正在识别原文…」） */
  waitingNote?: ReactNode
  label: string
  className?: string
  'data-testid'?: string
}

/**
 * 骨架 → 流式（逐段淡入）→ 完成；切换时旧文字淡出、新文字淡入（Linux / 减少动效时不模糊）。
 * 等待中（骨架或旧文字）的高度在流式期间作为最小高度保留，直到流结束：第一段文字比骨架 / 旧译文短时不会先缩再长；
 * 流结束时若新结果更短，变矮由外层处理（卡片已到上限时 SmoothBox 过渡，否则 FlipBox 让下方内容贴着卡片底边）
 */
export function RunBody({
  run,
  format,
  maxHeight,
  skeleton = 2,
  ghost = null,
  waitingNote,
  label,
  className,
  'data-testid': testId,
}: RunBodyProps) {
  const reduced = useReducedMotionPref()
  const os = usePlatform()
  const active = useViewActive()
  const plain = reduced || os === 'linux'
  const { phase, key } = run.state
  const waiting = phase === 'waiting'
  const showGhost = waiting && ghost !== null && ghost.trim() !== ''
  const out = plain ? presets.styleSwitchOutPlain : presets.styleSwitchOut
  const into = plain ? presets.styleSwitchInPlain : presets.styleSwitchIn
  const box = useRef<HTMLDivElement>(null)
  const [hold, setHold] = useState(0)
  useLayoutEffect(() => {
    // 暂存层里（视图切换还没显示）不读布局：等显示后的那次提交（active 变化）再量
    if (!active) return
    if (waiting) {
      const h = box.current?.offsetHeight ?? 0
      setHold((cur) => (cur === h ? cur : h))
    } else if (phase !== 'streaming') setHold(0)
    // oxlint-disable-next-line react/exhaustive-effect-dependencies -- 换了一次请求或出现旧文字时重新量
  }, [waiting, phase, key, showGhost, active])
  const minHeight = phase === 'streaming' && hold > 0 ? hold : undefined
  let body: ReactNode
  if (showGhost) {
    body = (
      <StreamView
        key={`ghost-${key}`}
        format={format}
        initialText={ghost}
        ended
        maxHeight={maxHeight}
      />
    )
  } else if (waiting || phase === 'idle') {
    body = (
      <div className="ocr-wait">
        <Skeleton lines={skeleton} />
        {waitingNote ? <p className="ocr-wait-note">{waitingNote}</p> : null}
      </div>
    )
  } else {
    body = (
      <StreamView
        key={key}
        ref={run.sink as Ref<StreamHandle>}
        format={format}
        // oxlint-disable-next-line react/refs -- 流式文本不进 state：挂载时取一次已收到的全文作初始内容
        initialText={run.text()}
        ended={phase !== 'streaming'}
        maxHeight={maxHeight}
      />
    )
  }
  return (
    <m.div
      ref={box}
      className={cx('ocr-run', className)}
      data-phase={phase}
      data-ghost={showGhost || undefined}
      aria-label={label}
      aria-busy={waiting || phase === 'streaming'}
      animate={showGhost ? out : into}
      initial={false}
      style={minHeight !== undefined ? { minHeight } : undefined}
      data-testid={testId}
    >
      {body}
    </m.div>
  )
}

/**
 * 状态行放不下时按段整体隐藏：先去掉 tokens，再去掉耗时，再去掉模型名，绝不截断数字；提示（思考耗尽、已截断……）
 * 最后才以省略号截断，全文在 title 里。各段都去掉了仍放不下（「已复制」不截断，窄面板上排版、翻译、识别三组按钮
 * 挤在一起时会这样）：分体按钮里的文字按钮（「翻译」/「原文」）只留图标（底栏的 data-compact）。只留图标之后不随
 * 「已复制」的出现与消失来回切换，底栏变宽了才重新判断。只在状态行的尺寸或内容变化时量（ResizeObserver +
 * MutationObserver，合并到一帧）
 */
export function useStatusFit(container: RefObject<HTMLElement | null>): void {
  useEffect(() => {
    const root = container.current
    if (!root || typeof ResizeObserver === 'undefined') return
    let frame = 0
    let watched: Element | null = null
    /** 只留图标时底栏的宽度（0 = 没有只留图标） */
    let compactAt = 0
    const over = (el: HTMLElement) => el.scrollWidth > el.clientWidth + 0.5
    const drop = (el: HTMLElement) => {
      delete el.dataset.drop
      for (let level = 1; level <= 3 && over(el); level++) el.dataset.drop = String(level)
    }
    const fit = () => {
      frame = 0
      const el = root.querySelector<HTMLElement>('.ocr-status')
      if (!el) return
      if (el !== watched) {
        if (watched) ro.unobserve(watched)
        watched = el
        ro.observe(el)
      }
      if (compactAt && root.clientWidth > compactAt) {
        delete root.dataset.compact
        compactAt = 0
      }
      drop(el)
      if (compactAt || !over(el) || !root.querySelector('.ocr-split > .ui-chip')) return
      root.dataset.compact = ''
      compactAt = root.clientWidth
      drop(el)
    }
    const schedule = () => {
      frame ||= requestAnimationFrame(fit)
    }
    const ro = new ResizeObserver(schedule)
    const mo = new MutationObserver(schedule)
    mo.observe(root, { childList: true, subtree: true, characterData: true })
    schedule()
    return () => {
      ro.disconnect()
      mo.disconnect()
      if (frame) cancelAnimationFrame(frame)
    }
  }, [container])
}

/** 完成后的底栏淡入（presets.footerIn） */
export function Footer({
  children,
  'data-testid': testId,
}: {
  children: ReactNode
  'data-testid'?: string
}) {
  const motion = usePreset('footerIn')
  const ref = useRef<HTMLDivElement>(null)
  useStatusFit(ref)
  return (
    <m.div ref={ref} className="ocr-footer" data-testid={testId} {...motion}>
      {children}
    </m.div>
  )
}

/** FlipBox 的句柄：上方内容变矮了 dy px（这一块已被布局提上去）——先压回原位，再随卡片的弹簧一起移上去 */
export interface FlipHandle {
  nudge(dy: number): void
  readonly el: HTMLDivElement | null
}

/**
 * 一段跟在可变高度内容后面的块（底栏 + 展开区、区底状态行）。上方内容变矮时，卡片按高度跟随用弹簧收缩，而这一块会被
 * 布局一帧提上去、下面露出一截空白；`nudge(dy)` 把它先压回原位（同步写 transform，不等 Motion 的下一帧），再以同一个
 * 弹簧回到 0——它就贴着卡片底边一起移动。减少动效时不做（卡片也是直接到位）
 */
export function FlipBox({
  handle,
  children,
  className,
  onPointerDown,
  'data-testid': testId,
}: {
  handle: Ref<FlipHandle>
  children: ReactNode
  className?: string
  onPointerDown?: () => void
  'data-testid'?: string
}) {
  const ref = useRef<HTMLDivElement>(null)
  const controls = useAnimationControls()
  const y = useMotionValue(0)
  const reduced = useReducedMotionPref()
  useImperativeHandle(
    handle,
    () => ({
      get el() {
        return ref.current
      },
      nudge(dy: number) {
        const el = ref.current
        if (!el || reduced || Math.abs(dy) < 1) return
        // 从当前位移（上一次还没回到 0 时）再加上这次的 dy 起跳。用 jump（速度归零）而不是 set：set 会把这一跳算成
        // 速度，弹簧先冲过头；不写进 motion value 的话，紧接着的重渲染又会把 transform 写回 0
        const from = y.get() + dy
        el.style.transform = `translateY(${from}px)`
        y.jump(from)
        void controls.start({ y: [from, 0], transition: HEIGHT_SPRING })
      },
    }),
    [controls, reduced, y],
  )
  return (
    // oxlint-disable-next-line jsx-a11y/no-static-element-interactions -- 只记录最近一次交互的区域（⌘R 的作用对象）
    <m.div
      ref={ref}
      className={className}
      style={{ y }}
      animate={controls}
      initial={false}
      onPointerDown={onPointerDown}
      data-testid={testId}
    >
      {children}
    </m.div>
  )
}

/** 展开区底部的状态行一栏（从展开起就占位；结果的状态行按段整体隐藏，见 useStatusFit；上方译文变矮时贴着卡片底边） */
export function SectionFoot({
  children,
  handle,
  'data-testid': testId,
}: {
  children: ReactNode
  handle: Ref<FlipHandle>
  'data-testid'?: string
}) {
  const own = useRef<FlipHandle>(null)
  const el = useRef<HTMLElement | null>(null)
  useImperativeHandle(handle, () => ({
    get el() {
      return own.current?.el ?? null
    },
    nudge: (dy: number) => own.current?.nudge(dy),
  }))
  useLayoutEffect(() => {
    el.current = own.current?.el ?? null
  })
  useStatusFit(el)
  return (
    <FlipBox handle={own} className="ocr-section-foot" data-testid={testId}>
      {children}
    </FlipBox>
  )
}

/**
 * 展开的区域（译文 / 原文）。
 * - 进入：卡片还没到上限时直接占满自己的高度、内容 120 ms 淡入（presets.pageSwitch）——卡片 / 窗口的高度跟随负责
 *   「长高」，不再叠一层区自身的高度弹簧；卡片已到上限（`expand`）时高度从 0 以 HEIGHT_SPRING 展开（与
 *   presets.sectionExpand 同值），正文区随 flex 逐帧让出同样的高度，卡片不动。展开途中内容又长高（骨架换成译文、
 *   流式）时，目标随之更新（弹簧保留速度重定目标），而不是按开始时量到的高度展开、结束时再一下跳到新高度；
 * - 退出：`collapse`（卡片已到上限）时高度折叠（presets.sectionExpand.exit），正文区逐帧拿回空间；否则立即脱离排版
 *   （absolute，留在原位）并淡出，卡片按高度跟随一次缩到位。
 * 区自身的高度在过渡时，内容按自然高度排版、被区裁住（data-sizing），不被 flex 压扁。
 * 减少动效时进出都只做 ≤ 100 ms 的淡入淡出、高度直接到位。
 */
/** 区内容的自然高度：内层的边框盒 + 上下外边距（内层顶部有 12 px 的外边距，回到 auto 时它也在区的高度里） */
function naturalHeight(inner: HTMLElement): number {
  const cs = getComputedStyle(inner)
  return inner.offsetHeight + (parseFloat(cs.marginTop) || 0) + (parseFloat(cs.marginBottom) || 0)
}

export function Section({
  open,
  expand,
  collapse,
  children,
  label,
  'data-testid': testId,
}: {
  open: boolean
  /** 展开时从 0 长高（卡片已到上限） */
  expand: boolean
  /** 收起时折叠（卡片已到上限） */
  collapse: boolean
  children: ReactNode
  label: string
  'data-testid'?: string
}) {
  const reduced = useReducedMotionPref()
  const ref = useRef<HTMLElement>(null)
  const inner = useRef<HTMLDivElement>(null)
  const [present, setPresent] = useState(open)
  const [leaving, setLeaving] = useState<'collapse' | 'pop' | null>(null)
  /** 这次进入的方式（进入时定下来）：grow = 高度从 0（或折叠到一半的当前高度）展开 */
  const [entry, setEntry] = useState<{ n: number; grow: boolean }>({ n: 0, grow: false })
  /** 区自身的高度在过渡（展开 / 折叠） */
  const [sizing, setSizing] = useState(false)
  /** grow 进入的目标高度：null = 还没量到（停在起点）；'auto' = 展开完成、回到自然高度 */
  const [height, setHeight] = useState<number | 'auto' | null>(null)
  // 由 open 推导出场 / 离场（渲染期间更新自己的 state：React 会立即以新 state 重新渲染，不会先提交旧的一帧）
  if (open && (!present || leaving)) {
    // 新进入；或退场到一半又打开：折叠中 / 原本按高度展开的，从当前高度接着展开回去（不跳），只淡出的接着淡入
    const g = !reduced && (present ? leaving === 'collapse' || entry.grow : expand)
    setPresent(true)
    setLeaving(null)
    setEntry((e) => ({ n: e.n + 1, grow: g }))
    if (g) setHeight(null)
    setSizing(g)
  } else if (!open && present && !leaving) {
    const c = collapse && !reduced
    setLeaving(c ? 'collapse' : 'pop')
    setSizing(c)
  }
  const fade = presets.reduced.fade
  const enter = presets.sectionExpand

  // grow 进入：在绘制之前量出内容的自然高度作为目标；展开途中内容又长高（骨架换成译文、流式）时目标随之更新
  // （弹簧保留速度重定目标）。只设 state，由 Motion 在下一帧应用——不在 ResizeObserver 回调里同步改尺寸
  useLayoutEffect(() => {
    if (!present || leaving || !entry.grow) return
    const el = inner.current
    if (!el) return
    setHeight(naturalHeight(el))
    if (typeof ResizeObserver === 'undefined') return
    const ro = new ResizeObserver(() => setHeight((h) => (h === 'auto' ? h : naturalHeight(el))))
    ro.observe(el)
    return () => ro.disconnect()
    // oxlint-disable-next-line react/exhaustive-deps -- entry.n：每次进入（含退场到一半又打开）重新量一次
  }, [entry.n, present, leaving, entry.grow])

  // 声明式的目标（在 effect 里命令式 controls.start 的话，StrictMode 的 effect 双调用 / 将来的
  // <Activity> 重挂载下会停在 opacity 0）；任何一次重新挂载，Motion 都按 animate 重新走到同一个终点
  const animate =
    leaving === 'collapse'
      ? { height: 0, opacity: 0, transition: enter.exit.transition }
      : leaving === 'pop'
        ? { opacity: 0, transition: reduced ? fade : presets.pageSwitch.exit.transition }
        : !entry.grow
          ? { opacity: 1, transition: reduced ? fade : presets.pageSwitch.animate.transition }
          : height === 'auto'
            ? { height: 'auto', opacity: 1, transition: { duration: 0 } }
            : { height: height ?? 0, opacity: 1, transition: enter.animate.transition }
  const state = useRef({ leaving, grow: entry.grow, height })
  useLayoutEffect(() => {
    state.current = { leaving, grow: entry.grow, height }
  })

  if (!present && !open) return null
  return (
    <m.section
      ref={ref}
      className="ocr-section"
      aria-label={label}
      aria-hidden={leaving ? true : undefined}
      data-leaving={leaving ?? undefined}
      data-sizing={sizing || undefined}
      data-testid={leaving ? undefined : testId}
      initial={entry.grow ? { height: 0, opacity: 0 } : { opacity: 0 }}
      animate={animate}
      onAnimationComplete={() => {
        const cur = state.current
        if (cur.leaving) {
          setPresent(false)
          setLeaving(null)
          setSizing(false)
          return
        }
        if (!cur.grow || typeof cur.height !== 'number') return
        // 区自己的高度真的到了目标（这次完成的可能只是同一批里的淡入）、内容也没再长：先让区里的流式框接手（钉住现在的
        // 高度），再回到 auto。比较 Motion 写上的高度而不是 offsetHeight：内容比卡片剩余空间高时区会被 flex 压矮，
        // offsetHeight 永远到不了目标，区就一直停在 data-sizing（内层不收缩、被区裁住），长译文的下半截既看不到也滚不动
        const animated = parseFloat(ref.current?.style.height ?? '')
        if (!(Math.abs(animated - cur.height) < 1)) return
        if (!inner.current || Math.abs(naturalHeight(inner.current) - cur.height) >= 1) return
        ref.current?.dispatchEvent(new Event(SECTION_SIZED))
        setSizing(false)
        setHeight('auto')
      }}
    >
      <div ref={inner} className="ocr-section-inner">
        {children}
      </div>
    </m.section>
  )
}
/**
 * 展开区正文的外层，也是它的滚动容器（流式区不另设滚动，见 RunBody 的 maxHeight）。`animate`（卡片已到上限 /
 * 固定窗口）时高度变化都用 HEIGHT_SPRING 过渡，正文区随 flex 逐帧让出 / 拿回同样的高度、两区之和不变：
 * - 由**提交**引起的变化（流式区保留的最小高度释放、骨架换成结果、错误卡片……）：在布局 effect 里同步写上起点；
 * - **流式中**内容自己的增长（不经过 React 提交）：外框在流式期间一直保持显式高度，新的一行先在框内（被裁住），
 *   内容的 ResizeObserver 再让外框以弹簧长上去——新行像拉开帘子一样露出来，底栏只朝一个方向平滑移动，不会一帧
 *   跳上去一两行；放不下时（正文区已收缩到下限）框内滚动并跟随到底部，用户上滚后停止跟随。
 * 回调里不同步改被观察元素的尺寸（只交给 Motion 在下一帧应用，外框此时已是显式高度、不会先跳），免得触发
 * 「ResizeObserver loop」。减少动效时直接到位。
 */
export function SmoothBox({
  animate,
  stream = false,
  streamKey = 0,
  children,
  className,
  onPointerDown,
  ref,
}: {
  animate: boolean
  /** 区里的流式请求正在进行 */
  stream?: boolean
  /** 当前流式请求的标识（换了一次请求时重新跟随到底部） */
  streamKey?: number
  children: ReactNode
  className?: string
  onPointerDown?: () => void
  ref?: RefObject<HTMLDivElement | null>
}) {
  const own = useRef<HTMLDivElement>(null)
  const box = ref ?? own
  const content = useRef<HTMLDivElement>(null)
  // 用 m.div 的动画控制（LazyMotion 的 domAnimation 已加载），不另外引入 Motion 的命令式 animate（约 10 KB）
  const controls = useAnimationControls()
  /** 最近一次稳定（不在过渡中）时的高度；-1 = 还没量过 */
  const stable = useRef(-1)
  /** 进行中的过渡序号：0 = 没有在过渡 */
  const running = useRef(0)
  const seq = useRef(0)
  /** 流式跟随：外框保持显式高度、内容长高时外框跟上 */
  const pinned = useRef(false)
  /** 框内滚动时跟随到底部（用户上滚后停止，回到底部恢复） */
  const follow = useRef(true)
  const programmatic = useRef(-1)
  const reduced = useReducedMotionPref()
  const active = useViewActive()
  const streaming = animate && stream && !reduced
  const streamingRef = useRef(streaming)
  streamingRef.current = streaming

  /** 回到自然高度。`settle`：之后不再接着过渡——Motion 记着动画的终值、重渲染时会写回去，经它设成 auto
   * （它在下一帧才写；紧接着要从显式起点过渡时不能调用，否则那一帧会先画出自然高度） */
  const release = (el: HTMLElement, settle: boolean) => {
    controls.stop()
    running.current = 0
    pinned.current = false
    el.style.height = ''
    el.style.overflowY = ''
    if (settle) controls.set({ height: 'auto' })
  }
  /** 在展开 / 折叠中的展开区里：区自己的高度过渡是唯一的驱动者，这里按自然高度排版（不再叠一层弹簧） */
  const inSizing = (): boolean => !!box.current?.closest('[data-sizing]')
  /** 钉住当前高度（与现在的布局相同，不改变尺寸），之后内容长高时外框以弹簧跟上 */
  const pin = (el: HTMLElement) => {
    if (pinned.current) return
    pinned.current = true
    el.style.overflowY = 'hidden'
    el.style.height = `${el.offsetHeight}px`
  }
  const toBottom = (el: HTMLElement) => {
    if (!follow.current || el.scrollHeight <= el.clientHeight + 1) return
    el.scrollTop = el.scrollHeight
    programmatic.current = el.scrollTop
  }

  useLayoutEffect(() => {
    const el = box.current
    const inner = content.current
    if (!el || !inner || typeof ResizeObserver === 'undefined') return
    const ro = new ResizeObserver((entries) => {
      if (!running.current && !pinned.current) stable.current = el.offsetHeight
      syncFadeEdges(el)
      // 流式跟随还没钉住（例如卡片在流式途中才到上限）：钉在现在的高度——尺寸不变，不会触发 ResizeObserver loop
      if (!pinned.current && streamingRef.current && !inSizing()) pin(el)
      if (!pinned.current || !entries.some((e) => e.target === inner)) return
      toBottom(el)
      const from = el.offsetHeight
      const to = inner.offsetHeight
      if (Math.abs(to - from) < 1) return
      const id = ++seq.current
      running.current = id
      void controls.start({ height: [from, to], transition: HEIGHT_SPRING }).then(() => {
        if (running.current === id) running.current = 0
      })
    })
    ro.observe(el)
    ro.observe(inner)
    // 所在的展开区展开完：从这里起由本框跟随内容（区随即回到 auto 高度，等于现在的内容高度）
    const section = el.closest('.ocr-section')
    const onSized = () => {
      if (streamingRef.current) pin(el)
    }
    section?.addEventListener(SECTION_SIZED, onSized)
    return () => {
      ro.disconnect()
      section?.removeEventListener(SECTION_SIZED, onSized)
    }
    // oxlint-disable-next-line react/exhaustive-deps -- 只在挂载时建立观察
  }, [box])

  // 换了一次流式请求：重新跟随到底部
  useLayoutEffect(() => {
    follow.current = true
  }, [streamKey])

  // 流式结束：一路跟随到底部、放不下的长结果回到开头——与翻译视图 / 固定窗口同一种行为（OCR 窄面板的
  // 译文区停在末尾，第一行上半截被裁掉、叠上顶部渐隐像缺了字）；用户自己往上滚过（不在跟随）时不动
  const wasStream = useRef(stream)
  useEffect(() => {
    const ended = wasStream.current && !stream
    wasStream.current = stream
    const el = box.current
    if (!ended || !el || !follow.current || el.scrollHeight <= el.clientHeight + 1) return
    el.scrollTo({ top: 0, behavior: reduced ? 'auto' : 'smooth' })
    // oxlint-disable-next-line react/exhaustive-deps -- 只在流式结束时执行
  }, [stream])

  // 每次提交之后：内容已按新状态排好（子组件的布局 effect 先执行）
  useLayoutEffect(() => {
    const el = box.current
    if (!el || !active) return
    if (inSizing()) {
      if (running.current || pinned.current) release(el, true)
      stable.current = el.offsetHeight
      return
    }
    if (streamingRef.current) {
      // 进入流式跟随：外框钉在提交前的高度（还没画出新布局），内容的 ResizeObserver 接着把它带到新高度
      if (!pinned.current) {
        // 起点：过渡中取当前可见高度，否则取提交前稳定时的高度（刚挂载、还没量过时就是现在的高度）
        const now = el.offsetHeight
        const from = running.current || stable.current < 0 ? now : stable.current
        controls.stop()
        running.current = 0
        pinned.current = true
        el.style.overflowY = 'hidden'
        el.style.height = `${from}px`
        const to = content.current?.offsetHeight ?? from
        if (Math.abs(to - from) >= 1) {
          const id = ++seq.current
          running.current = id
          void controls.start({ height: [from, to], transition: HEIGHT_SPRING }).then(() => {
            if (running.current === id) running.current = 0
          })
        }
      }
      return
    }
    const wasPinned = pinned.current
    const from = running.current || wasPinned ? el.offsetHeight : stable.current
    if (running.current || wasPinned) release(el, false)
    const to = el.offsetHeight
    stable.current = to
    syncFadeEdges(el)
    if (from < 0 || !animate || reduced || Math.abs(to - from) < 1) {
      if (wasPinned) controls.set({ height: 'auto' })
      return
    }
    const id = ++seq.current
    running.current = id
    el.style.overflowY = 'hidden'
    // 起点同步写上：Motion 的第一帧在下一个动画帧才应用，否则会先画出一帧新高度（跳变）
    el.style.height = `${from}px`
    void controls.start({ height: [from, to], transition: HEIGHT_SPRING }).then(() => {
      if (running.current !== id) return
      running.current = 0
      el.style.height = ''
      el.style.overflowY = ''
      controls.set({ height: 'auto' })
    })
  })
  return (
    // oxlint-disable-next-line jsx-a11y/no-static-element-interactions -- 只记录最近一次交互的区域（⌘R 的作用对象）
    <m.div
      ref={box}
      className={cx('ocr-box', className)}
      animate={controls}
      initial={false}
      onPointerDown={onPointerDown}
      onScroll={(e) => {
        const el = e.currentTarget
        syncFadeEdges(el)
        // 程序自己跟随到底部引起的 scroll 不算；用户上滚停止跟随，回到底部恢复
        if (Math.abs(el.scrollTop - programmatic.current) < 1) return
        follow.current = el.scrollTop + el.clientHeight >= el.scrollHeight - 2
      }}
    >
      <div ref={content} className="ocr-box-content">
        {children}
      </div>
    </m.div>
  )
}
