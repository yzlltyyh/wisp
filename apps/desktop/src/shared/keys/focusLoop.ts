/**
 * 窗口内的 Tab 焦点循环（由面板外壳 / 固定窗口统一做，视图不各自实现）。面板与固定窗口只有一张卡片，
 * Tab 在卡片里的控件之间按文档顺序移动，最后一个之后回到第一个（Shift+Tab 反之），焦点不会落到看不见的 body 或窗口外
 * 的空停顿上。
 *
 * **每一次 Tab 都由这里接管**：如果只在首尾接管、中间交给浏览器，macOS 默认（系统设置的
 * 「键盘导航」关闭）时 WebKit 的 Tab 不经过按钮，两套语义会混在一起：中间跳过按钮、首尾又被强制聚焦到按钮上。全部
 * 接管后，三端、两种引擎、系统设置开或关都是同一个顺序：卡片里的全部控件（按钮也在内——面板是键盘优先的工具，按钮必须够得着）。
 *
 * 可聚焦的控件：按钮、链接、输入框、`tabindex ≥ 0` 的元素；跳过禁用、看不见（没有布局盒）、`inert` 子树（暂存层、
 * 已被替换的旧视图）与 `aria-hidden` 子树（正在淡出的旧页）。浮层（菜单 / popover）自己处理 Tab，不在这里。
 */
const TABBABLE =
  'button:not(:disabled), [href], input:not(:disabled), textarea:not(:disabled), select:not(:disabled), [tabindex]'

function tabbables(root: HTMLElement): HTMLElement[] {
  return [...root.querySelectorAll<HTMLElement>(TABBABLE)].filter(
    (el) =>
      el.tabIndex >= 0 &&
      (el.getClientRects().length > 0 || el === document.activeElement) &&
      !el.closest('[inert], [aria-hidden="true"]'),
  )
}

/**
 * 在 root 里把焦点移到下一个（`back`：上一个）控件，首尾回绕；焦点不在 root 里时进入第一个 / 最后一个。
 * 移动了返回 true；root 里没有可聚焦的控件时返回 false（交给浏览器）
 */
export function cycleFocusWithin(root: HTMLElement | null, back: boolean): boolean {
  if (!root) return false
  const items = tabbables(root)
  if (items.length === 0) return false
  const active = document.activeElement
  const inside = active instanceof HTMLElement && root.contains(active)
  let next: HTMLElement | undefined
  if (!inside) next = back ? items.at(-1) : items[0]
  else {
    const at = items.indexOf(active)
    if (at >= 0) next = items[(at + (back ? -1 : 1) + items.length) % items.length]
    // 焦点在 root 里、但不在序列里（例如 tabindex = -1 的分组标题）：按文档顺序找它前 / 后的第一个
    else if (back)
      next =
        items.findLast(
          (el) => !!(el.compareDocumentPosition(active) & Node.DOCUMENT_POSITION_FOLLOWING),
        ) ?? items.at(-1)
    else
      next =
        items.find(
          (el) => !!(active.compareDocumentPosition(el) & Node.DOCUMENT_POSITION_FOLLOWING),
        ) ?? items[0]
  }
  next?.focus()
  return next !== undefined
}
