/**
 * 菜单的「按下就打开」（翻译视图的风格 / ⋯ 菜单、OCR 的风格菜单共用）。
 *
 * - 指针按下（与系统菜单一致，不等抬起）、Enter / Space / ↓ 按下的那一刻就打开、播放 menuIn；卡片高度的预留、卡片
 *   追赶期间的下拉帘与关闭后延迟释放都由组件库的浮层定位负责（floating.tsx，Menu / PagedMenu / Popover 共用）；
 * - 键盘打开（Enter / Space / ↑↓）时高亮一项，指针打开时不高亮；
 * - 菜单第一次请求打开时才挂载（面板 prepare 的同步首帧里不渲染），之后保留以播放退出动画；
 * - `estimateMenuHeight`：菜单高度的估算（矮面板里决定收起哪些低频项：⋯ 菜单、风格菜单同一套规则）。
 */
import {
  useState,
  useRef,
  type KeyboardEvent as ReactKeyboardEvent,
  type MouseEvent,
  type PointerEvent as ReactPointerEvent,
  type RefObject,
} from 'react'
import type { MenuEntry } from './Menu'
import { useFloatingBoundary } from './floating'

/** 打开方式：键盘打开（Enter / Space / ↑↓）时高亮一项，指针打开时不高亮 */
export type MenuVia = 'pointer' | 'keyboard'

/** 菜单与触发器的间距、与卡片边界的最小距离（与 Menu / computePosition 的默认值一致） */
export const MENU_OFFSET = 4
export const MENU_PADDING = 8

/**
 * 菜单高度的估算（px）：与组件库的菜单样式一致（components.css：菜单上下内边距各 4，菜单项 --h-control = 28，分组标题
 * 6 + 2 + 14，分隔线 1 + 上下 4；带第二行说明的项：上下内边距 5 + 标签 18 + 说明每行 14）
 */
export function estimateMenuHeight(items: readonly MenuEntry[]): number {
  let h = 8
  for (const e of items) {
    if (e.type === 'separator') h += 9
    else if (e.type === 'label') h += 22
    else h += e.description ? 28 + descriptionLines(e.description) * 14 : 28
  }
  return h
}

/** 说明的行数：约 26 个全角字宽以上按两行估（宁大勿小：卡片只会多留几 px） */
function descriptionLines(text: string): number {
  let units = 0
  for (const ch of text) units += ch.charCodeAt(0) > 0x2e80 ? 1 : 0.55
  return units > 26 ? 2 : 1
}

/**
 * 卡片上限以内、触发器上方或下方能放下的最大菜单高度（px；下方含卡片还能长高的空间——浮层会预留）。没有浮层边界
 * （设置 / 引导窗口）时为 Infinity
 */
export function useMenuRoom(trigger: RefObject<HTMLElement | null>): () => number {
  const boundary = useFloatingBoundary()
  return () => {
    const el = trigger.current
    if (!el || !boundary) return Infinity
    const rect = boundary.rect()
    const a = el.getBoundingClientRect()
    const below = rect.height - (a.bottom - rect.top) - MENU_OFFSET - MENU_PADDING
    const above = a.top - rect.top - MENU_OFFSET - MENU_PADDING
    return Math.max(below, above)
  }
}

export interface MenuOpener {
  /** 交给 Menu 的 open */
  open: boolean
  /** 第一次请求打开之后才挂载 Menu */
  mounted: boolean
  via: MenuVia
  /** 请求打开（已经开着时不动） */
  request(via: MenuVia): void
  /** 关闭；`returnFocus`：先把焦点还给触发器（数字直选不经过 Menu 的关闭路径） */
  close(returnFocus?: boolean): void
  /** 触发器的指针按下：按下就打开 / 关闭（不等 click） */
  onPointerDown(e: ReactPointerEvent): void
  /** 触发器的 click：指针按下已经处理过的忽略；键盘的 click（Enter，detail = 0）打开 / 关闭 */
  onClick(e: MouseEvent): void
  /** 触发器的按键：Space 按下就打开（不等抬起时的 click）；↓ / ↑ 打开并高亮 */
  onKeyDown(e: ReactKeyboardEvent): void
}

/** `beforeOpen`：打开之前调整菜单内容（矮面板里按卡片上限决定收起程度） */
export function useMenuOpener(
  trigger: RefObject<HTMLElement | null>,
  beforeOpen?: () => void,
): MenuOpener {
  const [open, setOpen] = useState(false)
  const [mounted, setMounted] = useState(false)
  const [via, setVia] = useState<MenuVia>('pointer')
  /** 这次 click 之前的指针按下已经打开 / 关闭过 */
  const pressed = useRef(false)

  const request = (v: MenuVia) => {
    if (open) return
    beforeOpen?.()
    setVia(v)
    setMounted(true)
    setOpen(true)
  }
  const close = (returnFocus = false) => {
    // 先还焦点再关闭：焦点不会随正在退场的菜单一起消失
    if (returnFocus) trigger.current?.focus({ preventScroll: true })
    setOpen(false)
  }
  return {
    open,
    mounted,
    via,
    request,
    close,
    onPointerDown: (e) => {
      if (e.button !== 0) return
      // 不让按下把焦点交给触发器（菜单打开时焦点由 Menu 移进菜单）
      e.preventDefault()
      pressed.current = true
      if (open) close()
      else request('pointer')
    },
    onClick: (e) => {
      if (e.detail > 0 && pressed.current) {
        pressed.current = false
        return
      }
      pressed.current = false
      if (open) close()
      else request(e.detail === 0 ? 'keyboard' : 'pointer')
    },
    onKeyDown: (e) => {
      if (e.key === ' ' && !e.repeat) {
        e.preventDefault()
        if (open) close()
        else request('keyboard')
      }
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        e.preventDefault()
        request('keyboard')
      }
    },
  }
}
