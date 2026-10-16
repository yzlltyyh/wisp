/**
 * Menu / DropdownMenu / ContextMenu。StyleChip、LangBar、⋯、FilterMenu、Select 共用。
 *
 * - 数据驱动：`items` 为菜单项 / 分隔线 / 分组标题；菜单项可带图标、勾选（复选或单选）、快捷键列、右侧提示、第二行说明
 *   （`description`，例如置灰的原因）、危险色、禁用；
 * - 前导列：菜单里有勾选项或图标时，每一项都有同一个 16 px 的前导列——勾选项显示 ✓，其余显示自己的
 *   图标（勾选中的项以 ✓ 代替图标），文字左缘始终对齐；没有图标的项留空；
 * - 键盘：↑↓ 循环（跳过禁用项）、Home / End、Enter / Space 选择、首字母跳转（500 ms 内连续输入按前缀匹配）、
 *   Tab 关闭；Esc 经键盘路由的 overlay 层关闭菜单（不会关掉面板），焦点回到触发器；
 * - 焦点：定位完成的那次提交里移进菜单；关闭时（任何原因、包括调用方直接把 open 置 false）焦点还留在正在退场的菜单
 *   里或落到 body 上，就还给触发器——视图不需要自己补焦点；
 * - 指针：悬停高亮、点击选择、点外面关闭；
 * - 溢出：放不下时内部滚动，上 / 下还有内容的一侧显示一道渐隐阴影（纯 CSS，background-attachment）；
 * - 动效：presets.menuIn / menuOut，变换原点在触发器一侧；减少动效时只淡入淡出；
 * - 在面板里挂到卡片内并预留高度（卡片随之长高），不会被窗口裁掉（floating.tsx）；
 * - `ref` / `className` / `onExitComplete`：调用方不必再按 role + 无障碍名称去找菜单元素。
 */
import { AnimatePresence, m } from 'motion/react'
import type { LucideIcon } from 'lucide-react'
import { Check, ChevronRight } from 'lucide-react'
import {
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type PointerEvent as ReactPointerEvent,
  type ReactNode,
  type Ref,
  type RefObject,
} from 'react'
import { useKeyScope } from '../keys/useKeyScope'
import { adaptPreset, presets } from '../motion/presets'
import { ms, timing } from '../motion/tokens'
import { useReducedMotionPref } from '../motion/useReducedMotionPref'
import { cx } from './cx'
import { FloatingPortal, useFloating, type Align, type Side } from './floating'
import { Icon } from './Icon'
import { Kbd } from './Kbd'

export interface MenuItemEntry {
  type?: 'item'
  id: string
  label: string
  icon?: LucideIcon
  /** 快捷键组合串（右侧列） */
  shortcut?: string | null
  /** 勾选项：true / false；undefined 表示不是勾选项 */
  checked?: boolean
  /** 勾选项属于一组单选（role = menuitemradio，读屏读作「单选」）；默认是可以各自勾选的复选项 */
  radio?: boolean
  disabled?: boolean
  danger?: boolean
  /** 右侧的次要说明（例如风格描述、模型名） */
  hint?: string
  /** 标签下方的第二行说明（例如置灰的原因；完整显示、可以折行） */
  description?: string
  /** 子菜单提示箭头（只做视觉提示，选择时由 onSelect 打开新菜单） */
  submenu?: boolean
  onSelect?: () => void
}

export type MenuEntry =
  MenuItemEntry | { type: 'separator'; id: string } | { type: 'label'; id: string; label: string }

export type MenuCloseReason = 'select' | 'escape' | 'outside' | 'tab' | 'blur'

export interface MenuProps {
  open: boolean
  onClose: (reason: MenuCloseReason) => void
  anchor:
    RefObject<HTMLElement | null> | { left: number; top: number; width: number; height: number }
  items: readonly MenuEntry[]
  side?: Side
  align?: Align
  /** 空间不够时是否翻到另一侧，默认 true（组件库展示里的静态菜单传 false，见 Gallery） */
  flip?: boolean
  /** 两侧都放不下、边界放得下时整份上移到边界内（以指针为锚的右键菜单；floating.tsx） */
  shift?: boolean
  /** 最小宽度（Select 传触发器宽度） */
  minWidth?: number
  /** 打开时高亮哪一项：first（键盘打开）/ selected（勾选项）/ none（指针打开） */
  initialActive?: 'first' | 'selected' | 'none'
  /** 无障碍名称 */
  label?: string
  /** 关闭后把焦点还给它（默认还给锚点元素） */
  returnFocus?: RefObject<HTMLElement | null>
  /** 菜单元素（退出动画结束后卸载） */
  ref?: Ref<HTMLDivElement>
  className?: string
  /** 退出动画播完、菜单卸载之后 */
  onExitComplete?: () => void
  'data-testid'?: string
}

const isItem = (e: MenuEntry): e is MenuItemEntry => e.type === undefined || e.type === 'item'

function firstEnabled(items: readonly MenuEntry[], from: number, step: 1 | -1): number {
  const n = items.length
  for (let k = 0; k < n; k++) {
    const i = (((from + step * k) % n) + n) % n
    const e = items[i]
    if (e && isItem(e) && !e.disabled) return i
  }
  return -1
}

export function Menu({
  open,
  onClose,
  anchor,
  items,
  side = 'bottom',
  align = 'start',
  flip = true,
  shift = false,
  minWidth,
  initialActive = 'none',
  label,
  returnFocus,
  ref,
  className,
  onExitComplete,
  'data-testid': testId = 'menu',
}: MenuProps) {
  const reduced = useReducedMotionPref()
  const {
    ref: floatingRef,
    style: floatingStyle,
    side: floatingSide,
    placed,
  } = useFloating({ open, anchor, side, align, flip, shift, offset: 4, reserve: true })
  const [active, setActive] = useState(-1)
  const pointerMoved = usePointerIntent()
  const typeahead = useRef({ text: '', at: 0 })
  const baseId = useId()
  const checkable = items.some((e) => isItem(e) && e.checked !== undefined)
  const leading = checkable || items.some((e) => isItem(e) && e.icon !== undefined)
  const setRef = useCallback(
    (el: HTMLDivElement | null) => {
      floatingRef.current = el
      if (typeof ref === 'function') ref(el)
      else if (ref) ref.current = el
    },
    [floatingRef, ref],
  )

  // 打开时决定初始高亮
  useLayoutEffect(() => {
    if (!open) return
    let start = -1
    if (initialActive === 'selected') {
      start = items.findIndex((e) => isItem(e) && e.checked === true && !e.disabled)
      if (start < 0) start = firstEnabled(items, 0, 1)
    } else if (initialActive === 'first') start = firstEnabled(items, 0, 1)
    setActive(start)
    // 只在打开时执行
    // oxlint-disable-next-line react/exhaustive-deps
  }, [open])

  // 定位完成后把焦点移进菜单：第一次打开的那次提交里浮层还没有位置
  // （`visibility: hidden`），这时 focus() 会静默落空，键盘就进不了菜单。定位在 useFloating 的布局阶段完成并同步重渲染，
  // 这里在 placed 变为 true 的那次提交里（绘制之前）聚焦；之后再打开时沿用上次的位置，打开的那次提交里就能聚焦。
  // 焦点已经在菜单里时不动
  useLayoutEffect(() => {
    if (!open || !placed) return
    const el = floatingRef.current
    if (el && !el.contains(document.activeElement)) el.focus({ preventScroll: true })
  }, [open, placed, floatingRef])

  const returnTarget = useCallback(
    () => returnFocus?.current ?? ('current' in anchor ? anchor.current : null),
    [returnFocus, anchor],
  )
  const close = useCallback(
    (reason: MenuCloseReason) => {
      onClose(reason)
      if (reason !== 'outside' && reason !== 'blur') returnTarget()?.focus({ preventScroll: true })
    },
    [onClose, returnTarget],
  )

  // 关闭（含调用方直接把 open 置 false：数字直选、换到另一个菜单）之后，焦点还在正在退场的菜单里、或已经落到 body：
  // 还给触发器——否则退出动画结束、菜单卸载时焦点随之丢失，键盘用户要从头 Tab
  const wasOpen = useRef(open)
  useLayoutEffect(() => {
    const closing = wasOpen.current && !open
    wasOpen.current = open
    if (!closing) return
    const focused = document.activeElement
    const inMenu = focused instanceof Node && floatingRef.current?.contains(focused) === true
    if (inMenu || !focused || focused === document.body)
      returnTarget()?.focus({ preventScroll: true })
  }, [open, floatingRef, returnTarget])

  const select = useCallback(
    (index: number) => {
      const e = items[index]
      if (!e || !isItem(e) || e.disabled) return
      close('select')
      e.onSelect?.()
    },
    [items, close],
  )

  // Esc 走键盘路由的 overlay 层（模态：菜单打开时视图的快捷键暂停）
  useKeyScope('menu', [{ combo: 'Escape', allowInInput: true, run: () => close('escape') }], {
    layer: 'overlay',
    modal: true,
    enabled: open,
  })

  // 点外面关闭；窗口失焦关闭
  useEffect(() => {
    if (!open) return
    const onPointer = (e: PointerEvent) => {
      const t = e.target as Node
      if (floatingRef.current?.contains(t)) return
      if ('current' in anchor && anchor.current?.contains(t)) return
      close('outside')
    }
    const onBlur = () => close('blur')
    document.addEventListener('pointerdown', onPointer, true)
    window.addEventListener('blur', onBlur)
    return () => {
      document.removeEventListener('pointerdown', onPointer, true)
      window.removeEventListener('blur', onBlur)
    }
  }, [open, anchor, close, floatingRef])

  // 高亮项滚入视野（键盘导航）
  useEffect(() => {
    if (!open || active < 0) return
    document.getElementById(`${baseId}-${active}`)?.scrollIntoView({ block: 'nearest' })
  }, [open, active, baseId])

  const onKeyDown = (e: ReactKeyboardEvent<HTMLDivElement>) => {
    if (e.nativeEvent.isComposing) return
    switch (e.key) {
      case 'ArrowDown':
        e.preventDefault()
        setActive((a) => firstEnabled(items, a + 1, 1))
        return
      case 'ArrowUp':
        e.preventDefault()
        setActive((a) => firstEnabled(items, a < 0 ? items.length - 1 : a - 1, -1))
        return
      case 'Home':
        e.preventDefault()
        setActive(firstEnabled(items, 0, 1))
        return
      case 'End':
        e.preventDefault()
        setActive(firstEnabled(items, items.length - 1, -1))
        return
      case 'Enter':
      case ' ':
        e.preventDefault()
        if (active >= 0) select(active)
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

  const motion = adaptPreset({ ...presets.menuIn, exit: presets.menuOut }, 'fade', reduced)

  return (
    <FloatingPortal>
      <AnimatePresence onExitComplete={onExitComplete}>
        {open ? (
          <m.div
            key="menu"
            ref={setRef}
            role="menu"
            tabIndex={-1}
            aria-label={label}
            aria-activedescendant={active >= 0 ? `${baseId}-${active}` : undefined}
            className={cx('ui-menu', className)}
            data-side={floatingSide}
            data-checkable={checkable || undefined}
            data-leading={leading || undefined}
            data-testid={testId}
            style={{ ...floatingStyle, minWidth }}
            onKeyDown={onKeyDown}
            onPointerLeave={() => setActive(-1)}
            {...motion}
          >
            {items.map((entry, i) => (
              <MenuEntryView
                key={entry.id}
                entry={entry}
                id={`${baseId}-${i}`}
                active={i === active}
                leading={leading}
                testId={testId}
                onActivate={(e) => {
                  if (pointerMoved(e) && i !== active) setActive(i)
                }}
                onSelect={() => select(i)}
              />
            ))}
          </m.div>
        ) : null}
      </AnimatePresence>
    </FloatingPortal>
  )
}

/** 前导列与文字之间、各列的渲染：Menu 与 PagedMenu 共用（同一套 .ui-menu* 结构，视图不依赖内部类名） */
export interface MenuEntryViewProps {
  entry: MenuEntry
  /** 菜单项元素的 id（aria-activedescendant 指向它） */
  id: string
  active: boolean
  /** 菜单有前导列（有勾选项或图标） */
  leading: boolean
  testId: string
  /** 指针在这一项上移动（禁用项不会调用）；是不是真实的移动由调用方用 {@link usePointerIntent} 判断 */
  onActivate: (e: ReactPointerEvent) => void
  onSelect: () => void
}

/** 指针坐标（只用到视口坐标；单测里 fireEvent 不带坐标时为 0） */
interface PointerPoint {
  clientX: number
  clientY: number
}

/**
 * 菜单项的悬停只响应**真实的**指针移动：WebKit 在布局变化时（分页菜单换页的高度动画、浮层出现在静止的
 * 指针下面）会合成坐标不变的 mousemove，把它当成悬停会抢走键盘导航的高亮（e2e「视觉模型 ›」偶发失败的根因）。
 * 每个菜单实例一份：第一次只记基线（浮层出现在指针下时的合成事件不高亮——真实移动紧跟着就到），之后坐标变了才算移动。
 * 与剪贴板列表行的悬停（ClipList 的 pointer 基线）同一个规则。
 */
export function usePointerIntent(): (e: PointerPoint) => boolean {
  const last = useRef<{ x: number; y: number } | null>(null)
  return useCallback((e: PointerPoint) => {
    const p = last.current
    last.current = { x: e.clientX, y: e.clientY }
    return p !== null && (p.x !== e.clientX || p.y !== e.clientY)
  }, [])
}

export function MenuEntryView({
  entry,
  id,
  active,
  leading,
  testId,
  onActivate,
  onSelect,
}: MenuEntryViewProps) {
  if (entry.type === 'separator') return <hr className="ui-menu-separator" />
  if (entry.type === 'label') {
    return (
      <div role="presentation" className="ui-menu-label">
        {entry.label}
      </div>
    )
  }
  const descId = entry.description ? `${id}-desc` : undefined
  const common = {
    id,
    'aria-disabled': entry.disabled || undefined,
    'aria-describedby': descId,
    'aria-haspopup': entry.submenu ? ('menu' as const) : undefined,
    className: 'ui-menu-item',
    'data-described': entry.description ? true : undefined,
    'data-active': active || undefined,
    'data-danger': entry.danger || undefined,
    'data-testid': `${testId}-item-${entry.id}`,
    onPointerMove: (e: ReactPointerEvent) => {
      if (!entry.disabled) onActivate(e)
    },
    onClick: onSelect,
  }
  const body = (
    <>
      {leading ? <MenuLead checked={entry.checked === true} icon={entry.icon} /> : null}
      <span className="ui-menu-text">
        <span className="ui-menu-label-text">{entry.label}</span>
        {entry.description ? (
          <span id={descId} className="ui-menu-desc">
            {entry.description}
          </span>
        ) : null}
      </span>
      {entry.hint ? <span className="ui-menu-hint">{entry.hint}</span> : null}
      {entry.shortcut ? (
        <Kbd combo={entry.shortcut} variant="inline" className="ui-menu-shortcut" />
      ) : null}
      {entry.submenu ? <Icon icon={ChevronRight} size={14} className="ui-menu-submenu" /> : null}
    </>
  )
  // 键盘由菜单容器统一处理（aria-activedescendant），菜单项本身不获得焦点
  return entry.checked === undefined ? (
    // oxlint-disable-next-line jsx-a11y/click-events-have-key-events -- 见上
    <div role="menuitem" tabIndex={-1} {...common}>
      {body}
    </div>
  ) : (
    // oxlint-disable-next-line jsx-a11y/click-events-have-key-events -- 见上
    <div
      role={entry.radio ? 'menuitemradio' : 'menuitemcheckbox'}
      aria-checked={entry.checked}
      tabIndex={-1}
      {...common}
    >
      {body}
    </div>
  )
}

/** 前导列：✓（勾选中）或图标；都没有时留空 */
export function MenuLead({ checked, icon }: { checked?: boolean; icon?: LucideIcon | null }) {
  return (
    <span className="ui-menu-lead" data-check={checked || undefined} aria-hidden="true">
      {checked ? (
        <Icon icon={Check} size={14} />
      ) : icon ? (
        <Icon icon={icon} size={16} className="ui-menu-icon" />
      ) : null}
    </span>
  )
}

/** DropdownMenu 触发器需要的属性（展开到任意按钮上） */
export interface MenuTriggerProps {
  ref: Ref<HTMLButtonElement>
  onClick: () => void
  onKeyDown: (e: ReactKeyboardEvent<HTMLElement>) => void
  'aria-haspopup': 'menu'
  'aria-expanded': boolean
  'data-open': true | undefined
}

export interface DropdownMenuProps extends Omit<
  MenuProps,
  'open' | 'onClose' | 'anchor' | 'initialActive'
> {
  renderTrigger: (props: MenuTriggerProps, open: boolean) => ReactNode
  onOpenChange?: (open: boolean) => void
  /** 指针打开时也高亮勾选项（Select、StyleChip） */
  highlightSelected?: boolean
}

export function DropdownMenu({
  renderTrigger,
  onOpenChange,
  highlightSelected = false,
  ...menu
}: DropdownMenuProps) {
  const trigger = useRef<HTMLButtonElement | null>(null)
  const [open, setOpen] = useState(false)
  const [via, setVia] = useState<'pointer' | 'keyboard'>('pointer')
  const set = (next: boolean) => {
    setOpen(next)
    onOpenChange?.(next)
  }
  const props: MenuTriggerProps = {
    ref: trigger,
    onClick: () => {
      setVia('pointer')
      set(!open)
    },
    onKeyDown: (e) => {
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        e.preventDefault()
        setVia('keyboard')
        set(true)
      }
    },
    'aria-haspopup': 'menu',
    'aria-expanded': open,
    'data-open': open || undefined,
  }
  return (
    <>
      {renderTrigger(props, open)}
      <Menu
        {...menu}
        open={open}
        anchor={trigger}
        initialActive={
          via === 'keyboard'
            ? highlightSelected
              ? 'selected'
              : 'first'
            : highlightSelected
              ? 'selected'
              : 'none'
        }
        onClose={() => set(false)}
      />
    </>
  )
}

export interface ContextMenuProps {
  items: readonly MenuEntry[]
  children: ReactNode
  label?: string
  className?: string
  onOpenChange?: (open: boolean) => void
  'data-testid'?: string
}

/** 右键菜单：在指针位置打开 */
export function ContextMenu({
  items,
  children,
  label,
  className,
  onOpenChange,
  'data-testid': testId,
}: ContextMenuProps) {
  const [at, setAt] = useState<{ left: number; top: number; width: number; height: number } | null>(
    null,
  )
  const area = useRef<HTMLDivElement>(null)
  return (
    <div
      ref={area}
      className={cx('ui-contextmenu-area', className)}
      onContextMenu={(e) => {
        e.preventDefault()
        setAt({ left: e.clientX, top: e.clientY, width: 0, height: 0 })
        onOpenChange?.(true)
      }}
    >
      {children}
      <Menu
        open={at !== null}
        anchor={at ?? { left: 0, top: 0, width: 0, height: 0 }}
        items={items}
        label={label}
        shift
        returnFocus={area}
        data-testid={testId}
        onClose={() => {
          setAt(null)
          onOpenChange?.(false)
        }}
      />
    </div>
  )
}
