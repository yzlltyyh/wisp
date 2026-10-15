/**
 * mock 场景的挂载点（只在浏览器 mock 下使用；生产包里 `__WISP_MOCK__` 为 false，引用它的分支整段删除）。
 *
 * - `setPageOverride`：整页替换 PanelApp（组件展示页 `shell.gallery`、设置 / 引导的视觉稿），不走面板会话；
 * - `setViewOverride`：面板会话照常进行（真实的 PanelCard、进入动效、高度跟随、假桌面），只把视图注册表里的组件
 *   换成视觉稿组合（`design.*`）。
 * 场景的 `setup()` 在应用挂载之前执行，所以这里是普通的模块变量，不需要响应式。
 */
import type { ReactNode } from 'react'
import type { ViewSwitchRenderProps } from './ViewSwitch'

let page: ReactNode = null
let view: ((props: ViewSwitchRenderProps) => ReactNode) | null = null

export function setPageOverride(element: ReactNode): void {
  page = element
}

export function pageOverride(): ReactNode {
  return page
}

export function setViewOverride(
  render: ((props: ViewSwitchRenderProps) => ReactNode) | null,
): void {
  view = render
}

export function viewOverride(): ((props: ViewSwitchRenderProps) => ReactNode) | null {
  return view
}
