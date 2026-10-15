/**
 * 面板拖动：悬浮面板的各个视图（翻译、OCR 结果、输入、剪贴板）从头部与
 * 空白处按下左键即开始拖动——`panel_drag_start` 交给系统拖动窗口，Rust 跟随窗口更新几何（windows/panel.rs 的「拖动」一节：
 * 之后的长高 / 收缩以拖到的位置为准，卡片伸出工作区时整体推回）。固定窗口（Pinned）是普通窗口，仍用
 * `data-tauri-drag-region`。toast 点击即关闭，不接。
 *
 * 不从这些地方开始拖动（{@link canStartPanelDrag}）：
 * - 交互元素：链接、按钮、输入框 / 文本框 / 可编辑区、标签、菜单与菜单项、列表框与选项、滑块、开关、可聚焦的元素
 *   （`tabindex` ≥ 0）……按标签名与 ARIA 角色认；
 * - 可选中的正文：`user-select` 是 text / all / contain 的区域（base.css：面板默认不可选中，正文单独开启），以及
 *   `.ui-selectable` / `.ui-stream`（没有样式表的测试环境里同样认得出）；
 * - 可滚动元素的滚动条；
 * - 显式标记 `data-no-drag` 的子树；不在绑定元素的 DOM 子树里的目标（React 的事件会穿过 portal 冒泡上来）。
 *
 * 只认鼠标 / 笔的主键（触摸不接管）。pointerdown 里发起拖动、不阻止默认行为；紧接着的 mousedown 阻止默认行为——
 * 不开始选中文字、焦点留在原处（输入视图拖完可以接着打字）。内层已经处理过这次按下（`defaultPrevented`）时不拖动。
 */
import {
  useMemo,
  useRef,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
} from 'react'
import { api } from '../ipc'

const INTERACTIVE_ROLES = [
  'button',
  'link',
  'menu',
  'menubar',
  'menuitem',
  'menuitemcheckbox',
  'menuitemradio',
  'listbox',
  'option',
  'combobox',
  'textbox',
  'searchbox',
  'slider',
  'spinbutton',
  'scrollbar',
  'switch',
  'checkbox',
  'radio',
  'tab',
  'tablist',
  'tree',
  'treeitem',
  'grid',
  'gridcell',
  'dialog',
  'tooltip',
]

/** 不从这些元素（及其子树）开始拖动 */
export const NO_DRAG_SELECTOR = [
  'a',
  'button',
  'input',
  'textarea',
  'select',
  'option',
  'label',
  'summary',
  'video',
  'audio',
  'iframe',
  '[contenteditable]:not([contenteditable="false"])',
  '[tabindex]:not([tabindex="-1"])',
  ...INTERACTIVE_ROLES.map((role) => `[role="${role}"]`),
  '.ui-selectable',
  '.ui-stream',
  '[data-no-drag]',
].join(',')

const SELECTABLE = new Set(['text', 'all', 'contain'])

/** 元素自己声明的 `user-select`（WebKit 只认带前缀的写法）；没有声明时是 auto 或空串 */
function userSelectOf(el: Element): string {
  const style = getComputedStyle(el)
  return (
    style.getPropertyValue('user-select') ||
    style.getPropertyValue('-webkit-user-select') ||
    ''
  ).trim()
}

/** 点 (x, y) 落在元素自己的滚动条上（经典滚动条占布局：clientWidth / clientHeight 之外的那一条） */
function onScrollbar(el: Element, x: number, y: number): boolean {
  if (!(el instanceof HTMLElement)) return false
  const vertical = el.scrollHeight > el.clientHeight
  const horizontal = el.scrollWidth > el.clientWidth
  if (!vertical && !horizontal) return false
  const r = el.getBoundingClientRect()
  return (
    (vertical && x >= r.left + el.clientLeft + el.clientWidth && x <= r.right) ||
    (horizontal && y >= r.top + el.clientTop + el.clientHeight && y <= r.bottom)
  )
}

/**
 * 在 `root`（绑定拖动的元素）里按下 `target` 能否开始拖动（规则见文件头）。`point`：按下的视口坐标（判断滚动条用）
 */
export function canStartPanelDrag(
  target: EventTarget | null,
  root: Element,
  point?: { x: number; y: number },
): boolean {
  if (!(target instanceof Element) || !root.contains(target)) return false
  // 可选中与否由最近一个声明了 user-select 的祖先决定（auto 跟随父元素）
  let selectable: boolean | null = null
  for (let el: Element | null = target; el && el !== root; el = el.parentElement) {
    if (el.matches(NO_DRAG_SELECTOR)) return false
    if (selectable === null) {
      const value = userSelectOf(el)
      if (value !== '' && value !== 'auto') selectable = SELECTABLE.has(value)
    }
  }
  if (selectable === true) return false
  return !(point && onScrollbar(target, point.x, point.y))
}

export interface PanelDragHandlers {
  onPointerDown(e: ReactPointerEvent<HTMLElement>): void
  onMouseDown(e: ReactMouseEvent<HTMLElement>): void
}

/**
 * 面板拖动的事件处理（挂在视图根节点或头部上）；`enabled` 为 false（固定窗口里的视图）时返回 undefined。
 * 已有 onPointerDown 的元素自己先处理、再调用 `onPointerDown`
 */
export function usePanelDrag(sessionId: number, enabled = true): PanelDragHandlers | undefined {
  // 这一次按下发起了拖动：紧接着的 mousedown 阻止默认行为
  const armed = useRef(false)
  return useMemo(() => {
    if (!enabled) return undefined
    return {
      onPointerDown(e) {
        armed.current = false
        if (e.button !== 0 || e.pointerType === 'touch' || e.defaultPrevented) return
        if (!canStartPanelDrag(e.target, e.currentTarget, { x: e.clientX, y: e.clientY })) return
        armed.current = true
        void api.panelDragStart(sessionId).catch(() => undefined)
      },
      onMouseDown(e) {
        if (!armed.current) return
        armed.current = false
        e.preventDefault()
      },
    }
  }, [enabled, sessionId])
}
