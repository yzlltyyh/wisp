/**
 * 分页菜单：同一个浮层里换页（OCR 识别菜单的「模式页 ⇄ 视觉
 * 模型页」），像 Raycast 的 push / pop：
 * 浮层容器不卸载、不重播进入动效；只有内容交叉淡入 + ±8 px 水平位移（presets.stepSwitch，前进从右、后退从左），
 * 容器高度用 HEIGHT_SPRING 过渡，锚定的一边（在触发器上方时是底边，定位层用 CSS bottom 锚住）全程不动。减少动效时
 * 换页只淡入淡出、高度直接到位。
 *
 * 外观与键盘行为与 Menu 一致（菜单项由同一个 MenuEntryView 渲染；↑↓ 循环跳过禁用项、Home / End、Enter / Space、
 * 首字母跳转、Tab 关闭、Esc 经键盘路由的 overlay 层关闭并把焦点还给触发器、点外面 / 窗口失焦关闭），另外：
 * - → 在带「›」的项上进入下一页，← 返回上一页；
 * - 子页顶部的返回行「‹ 视觉模型」：‹ 在前导列里，标题与条目文字左对齐，无障碍名称由调用方给（「返回识别菜单」）；
 * - 打开时在定位完成（浮层可见）之后才把焦点移进菜单；关闭后焦点还在退场的菜单里 / 落到 body 时还给触发器。
 */
import { ChevronLeft } from 'lucide-react'
import { AnimatePresence, m, useAnimationControls, useIsPresent } from 'motion/react'
import {
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactNode,
  type Ref,
  type RefObject,
} from 'react'
import { useKeyScope } from '../keys/useKeyScope'
import { adaptPreset, presets, type PresetProps } from '../motion/presets'
import { ms, timing } from '../motion/tokens'
import { useReducedMotionPref } from '../motion/useReducedMotionPref'
import { FloatingPortal, useFloating, useFloatingBoundary, type Align, type Side } from './floating'
import { cx } from './cx'
import {
  MenuEntryView,
  MenuLead,
  usePointerIntent,
  type MenuEntry,
  type MenuItemEntry,
} from './Menu'

/** 换页时菜单高度的过渡：与卡片高度跟随同一个弹簧（spring.gentle） */
const HEIGHT_SPRING = presets.sectionExpand.animate.transition.height

export interface MenuPage {
  /** 页的标识（换页的依据） */
  key: string
  /** 菜单的无障碍名称（每页各自的） */
  label: string
  items: readonly MenuEntry[]
  /** 子页顶部的返回行 */
  back?: { label: string; ariaLabel: string; onSelect: () => void }
  'data-testid'?: string
}

export interface PagedMenuProps {
  open: boolean
  onClose: () => void
  anchor: RefObject<HTMLElement | null>
  /** 关闭后把焦点还给它（默认还给锚点元素） */
  returnFocus?: RefObject<HTMLElement | null>
  page: MenuPage
  /** 换页方向：1 前进（进入子页），-1 后退 */
  direction: 1 | -1
  side?: Side
  align?: Align
  minWidth?: number
  /** 菜单本体的附加样式类 */
  className?: string
}

const isItem = (e: MenuEntry): e is MenuItemEntry => e.type === undefined || e.type === 'item'

/** 返回行占用的下标（-1）；条目从 0 开始 */
const BACK = -1
/** 与触发器的间距（与 Menu 一致） */
const MENU_OFFSET = 4

/** 从 from 起按 step 找下一个可选项（返回行算在最前面） */
function nextEnabled(page: MenuPage, from: number, step: 1 | -1): number {
  const first = page.back ? BACK : 0
  const n = page.items.length - first
  for (let k = 0; k < n; k++) {
    const i = ((((from - first + step * k) % n) + n) % n) + first
    if (i === BACK) return BACK
    const e = page.items[i]
    if (e && isItem(e) && !e.disabled) return i
  }
  return first - 1
}

function initialActive(page: MenuPage): number {
  const checked = page.items.findIndex((e) => isItem(e) && e.checked === true && !e.disabled)
  if (checked >= 0) return checked
  return nextEnabled(page, page.back ? BACK : 0, 1)
}

export function PagedMenu({
  open,
  onClose,
  anchor,
  returnFocus,
  page,
  direction,
  side = 'bottom',
  align = 'start',
  minWidth,
  className,
}: PagedMenuProps) {
  const reduced = useReducedMotionPref()
  const floating = useFloating({ open, anchor, side, align, offset: MENU_OFFSET, reserve: true })
  const box = useRef<HTMLDivElement>(null)
  const sizer = useAnimationControls()
  const boundary = useFloatingBoundary()
  /**
   * 在触发器上方时，定位层改用 bottom 锚在触发器上沿（容器坐标）：换页时菜单高度变化，底边不动、顶边自然移动，
   * 不必重新定位。随定位层的每次重新定位（窗口尺寸、滚动）一起更新
   */
  const [anchorBottom, setAnchorBottom] = useState<number | null>(null)
  const baseId = useId()
  const [active, setActive] = useState(-2)
  /** 换页的高度动画里 WebKit 合成的 mousemove 不算悬停（usePointerIntent） */
  const pointerMoved = usePointerIntent()
  const typeahead = useRef({ text: '', at: 0 })
  /** 上一次提交时的页与容器高度（换页时的过渡起点） */
  const shown = useRef<{ key: string; height: number } | null>(null)
  /** 换页的高度过渡进行中 */
  const sizing = useRef(false)
  const items = page.items
  const checkable = items.some((e) => isItem(e) && e.checked !== undefined)
  const leading = checkable || !!page.back || items.some((e) => isItem(e) && e.icon !== undefined)

  const close = useCallback(
    (reason: 'select' | 'escape' | 'outside' | 'tab' | 'blur') => {
      onClose()
      if (reason === 'outside' || reason === 'blur') return
      ;(returnFocus?.current ?? anchor.current)?.focus({ preventScroll: true })
    },
    [onClose, returnFocus, anchor],
  )

  const { placed } = floating
  const top = floating.style.top
  useLayoutEffect(() => {
    if (!open || !placed || floating.side !== 'top') return setAnchorBottom(null)
    const a = anchor.current?.getBoundingClientRect()
    if (!a) return
    const edge = boundary?.container?.getBoundingClientRect().bottom ?? window.innerHeight
    setAnchorBottom(Math.round(edge - (a.top - MENU_OFFSET)))
    // oxlint-disable-next-line react/exhaustive-deps -- 随定位结果（top）更新
  }, [open, placed, floating.side, top])

  // 打开：定位完成（浮层可见）之后再把焦点移进菜单、高亮勾选项
  useLayoutEffect(() => {
    if (!open) {
      shown.current = null
      return
    }
    if (!placed) return
    box.current?.focus({ preventScroll: true })
    // oxlint-disable-next-line react/exhaustive-deps -- 只在打开（定位完成）时执行
  }, [open, placed])

  // 关闭之后焦点还在正在退场的菜单里、或落到 body：还给触发器（与 Menu 一致）
  const wasOpen = useRef(open)
  useLayoutEffect(() => {
    const closing = wasOpen.current && !open
    wasOpen.current = open
    if (!closing) return
    const focused = document.activeElement
    const inMenu = focused instanceof Node && box.current?.contains(focused) === true
    if (inMenu || !focused || focused === document.body)
      (returnFocus?.current ?? anchor.current)?.focus({ preventScroll: true })
  }, [open, returnFocus, anchor])

  // 换页：高亮新页的初始项；容器从旧高度过渡到新高度，锚定的一边不动
  useLayoutEffect(() => {
    const el = box.current
    if (!open || !el) return
    const prev = shown.current
    if (prev?.key === page.key) return
    setActive(initialActive(page))
    // 过渡起点：正在过渡时取当前可见的高度，否则取上一页稳定时的高度
    const from = sizing.current ? el.offsetHeight : (prev?.height ?? 0)
    sizer.stop()
    sizing.current = false
    // 新页的自然高度：旧页已脱离排版（popLayout），先去掉过渡中的显式高度
    el.style.height = ''
    const to = el.offsetHeight
    shown.current = { key: page.key, height: to }
    if (!prev) return
    // 锚定的一边由定位层的 CSS 固定（在触发器上方时用 bottom，见 anchorBottom），高度变化不需要重新定位
    if (reduced || Math.abs(to - from) < 1) {
      sizer.set({ height: 'auto' })
      return
    }
    // 起点同步写上（Motion 的第一帧在下一帧才应用，否则会先画出一帧新高度）
    el.style.height = `${from}px`
    sizing.current = true
    void sizer.start({ height: [from, to], transition: HEIGHT_SPRING }).then(() => {
      if (shown.current?.key !== page.key || !sizing.current) return
      sizing.current = false
      // 回到自然高度（Motion 记着动画的终值，重渲染时会写回去，所以经它来设）
      sizer.set({ height: 'auto' })
    })
    // oxlint-disable-next-line react/exhaustive-deps -- 只在换页（或打开）时执行
  }, [open, page.key])

  // Esc 走键盘路由的 overlay 层（模态：菜单打开时视图的快捷键暂停）
  useKeyScope('paged-menu', [{ combo: 'Escape', allowInInput: true, run: () => close('escape') }], {
    layer: 'overlay',
    modal: true,
    enabled: open,
  })

  // 点外面关闭；窗口失焦关闭
  useEffect(() => {
    if (!open) return
    const onPointer = (e: PointerEvent) => {
      const target = e.target as Node
      if (floating.ref.current?.contains(target) || anchor.current?.contains(target)) return
      close('outside')
    }
    const onBlur = () => close('blur')
    document.addEventListener('pointerdown', onPointer, true)
    window.addEventListener('blur', onBlur)
    return () => {
      document.removeEventListener('pointerdown', onPointer, true)
      window.removeEventListener('blur', onBlur)
    }
  }, [open, anchor, close, floating.ref])

  // 高亮项滚入视野（键盘导航）
  useEffect(() => {
    if (!open || active < BACK) return
    document.getElementById(`${baseId}-${page.key}-${active}`)?.scrollIntoView({ block: 'nearest' })
  }, [open, active, baseId, page.key])

  const select = (index: number) => {
    if (index === BACK) return page.back?.onSelect()
    const e = items[index]
    if (!e || !isItem(e) || e.disabled) return
    // 进入子页不关闭菜单（同一浮层里换页）
    if (!e.submenu) close('select')
    e.onSelect?.()
  }

  const onKeyDown = (e: ReactKeyboardEvent<HTMLDivElement>) => {
    if (e.nativeEvent.isComposing) return
    switch (e.key) {
      case 'ArrowDown':
        e.preventDefault()
        setActive((a) => nextEnabled(page, a + 1, 1))
        return
      case 'ArrowUp':
        e.preventDefault()
        setActive((a) => nextEnabled(page, a < BACK ? items.length - 1 : a - 1, -1))
        return
      case 'Home':
        e.preventDefault()
        setActive(nextEnabled(page, page.back ? BACK : 0, 1))
        return
      case 'End':
        e.preventDefault()
        setActive(nextEnabled(page, items.length - 1, -1))
        return
      case 'Enter':
      case ' ':
        e.preventDefault()
        if (active >= BACK) select(active)
        return
      case 'ArrowRight': {
        const it = items[active]
        if (it && isItem(it) && it.submenu && !it.disabled) {
          e.preventDefault()
          it.onSelect?.()
        }
        return
      }
      case 'ArrowLeft':
        if (page.back) {
          e.preventDefault()
          page.back.onSelect()
        }
        return
      case 'Tab':
        e.preventDefault()
        close('tab')
        return
    }
    if (e.key.length === 1 && !e.metaKey && !e.ctrlKey && !e.altKey) {
      const now = performance.now()
      const ta = typeahead.current
      ta.text = now - ta.at < ms(timing.typeahead) ? ta.text + e.key : e.key
      ta.at = now
      const needle = ta.text.toLocaleLowerCase()
      const n = items.length
      for (let k = 1; k <= n; k++) {
        const i = (Math.max(active, 0) + (ta.text.length > 1 ? k - 1 : k)) % n
        const it = items[i]
        if (it && isItem(it) && !it.disabled && it.label.toLocaleLowerCase().startsWith(needle)) {
          setActive(i)
          break
        }
      }
    }
  }

  const menuMotion = adaptPreset({ ...presets.menuIn, exit: presets.menuOut }, 'fade', reduced)
  const pageMotion = adaptPreset(presets.stepSwitch(direction), 'fade', reduced)
  const id = (i: number) => `${baseId}-${page.key}-${i}`

  return (
    <FloatingPortal>
      <AnimatePresence>
        {open ? (
          // 定位层：floating 的位置、进入 / 退出（menuIn / menuOut，变换原点在触发器一侧）
          <m.div
            key="menu"
            ref={floating.ref}
            className="ui-pmenu-pos"
            style={
              anchorBottom !== null
                ? { ...floating.style, top: 'auto', bottom: anchorBottom, minWidth }
                : { ...floating.style, minWidth }
            }
            {...menuMotion}
          >
            {/* 菜单本体：换页时高度与位移在这里过渡（锚定的一边不动） */}
            <m.div
              ref={box}
              role="menu"
              tabIndex={-1}
              aria-label={page.label}
              aria-activedescendant={active >= BACK ? id(active) : undefined}
              className={cx('ui-menu ui-pmenu', className)}
              data-side={floating.side}
              data-checkable={checkable || undefined}
              data-leading={leading || undefined}
              data-testid={page['data-testid']}
              style={{ minWidth }}
              animate={sizer}
              initial={false}
              onKeyDown={onKeyDown}
              onPointerLeave={() => setActive(-2)}
            >
              <AnimatePresence initial={false} mode="popLayout">
                <PageView key={page.key} pageKey={page.key} motion={pageMotion}>
                  {page.back ? (
                    <>
                      {/* oxlint-disable-next-line jsx-a11y/click-events-have-key-events -- 键盘由菜单容器统一处理（aria-activedescendant） */}
                      <div
                        id={id(BACK)}
                        role="menuitem"
                        tabIndex={-1}
                        aria-label={page.back.ariaLabel}
                        className="ui-menu-item ui-pmenu-back"
                        data-active={active === BACK || undefined}
                        data-testid={`${page['data-testid']}-item-back`}
                        onPointerMove={(e) => {
                          if (pointerMoved(e)) setActive(BACK)
                        }}
                        onClick={() => select(BACK)}
                      >
                        <MenuLead icon={ChevronLeft} />
                        <span className="ui-menu-text">
                          <span className="ui-menu-label-text">{page.back.label}</span>
                        </span>
                      </div>
                      <hr className="ui-menu-separator" />
                    </>
                  ) : null}
                  {items.map((entry, i) => (
                    <MenuEntryView
                      key={entry.id}
                      entry={entry}
                      id={id(i)}
                      active={i === active}
                      leading={leading}
                      testId={page['data-testid'] ?? 'paged-menu'}
                      onActivate={(e) => {
                        if (pointerMoved(e) && i !== active) setActive(i)
                      }}
                      onSelect={() => select(i)}
                    />
                  ))}
                </PageView>
              </AnimatePresence>
            </m.div>
          </m.div>
        ) : null}
      </AnimatePresence>
    </FloatingPortal>
  )
}

/** 一页的内容；换页时正在淡出的旧页不可交互、对读屏隐藏（菜单的高亮与 aria-activedescendant 已经指向新页） */
function PageView({
  pageKey,
  motion,
  children,
  ref,
}: {
  pageKey: string
  motion: PresetProps
  children: ReactNode
  /** AnimatePresence 的 popLayout 要量旧页的位置、把它移出排版 */
  ref?: Ref<HTMLDivElement>
}) {
  const present = useIsPresent()
  return (
    <m.div
      ref={ref}
      className="ui-pmenu-page"
      data-page={pageKey}
      aria-hidden={present ? undefined : true}
      inert={!present}
      {...motion}
    >
      {children}
    </m.div>
  )
}
