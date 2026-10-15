/**
 * 自绘单选组（强调色色块、vision 三态）的键盘约定，与组件库 SegmentedControl 一致（WAI-ARIA radiogroup）：
 * roving tabindex——只有选中项可以 Tab 到（都没选中时是第一项），←→ / ↑↓ 移动并选中（首尾循环），Home / End 跳到首尾。
 */
import type { KeyboardEvent as ReactKeyboardEvent } from 'react'

/** 第 `i` 项的 tabIndex */
export function rovingTabIndex(i: number, checkedIndex: number): 0 | -1 {
  if (checkedIndex < 0) return i === 0 ? 0 : -1
  return i === checkedIndex ? 0 : -1
}

/**
 * 单选组容器的 onKeyDown：`select(i)` 选中第 i 项，`focus(i)` 把焦点移过去
 */
export function radioGroupKeys(
  count: number,
  checkedIndex: number,
  select: (i: number) => void,
  focus: (i: number) => void,
): (e: ReactKeyboardEvent) => void {
  return (e) => {
    if (count === 0) return
    const from = checkedIndex < 0 ? 0 : checkedIndex
    let next = -1
    if (e.key === 'ArrowRight' || e.key === 'ArrowDown') next = (from + 1) % count
    else if (e.key === 'ArrowLeft' || e.key === 'ArrowUp') next = (from - 1 + count) % count
    else if (e.key === 'Home') next = 0
    else if (e.key === 'End') next = count - 1
    if (next < 0) return
    e.preventDefault()
    e.stopPropagation()
    select(next)
    focus(next)
  }
}
