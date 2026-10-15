/**
 * 设置窗口内的跳转：分组之间（「去模型与服务」）与分组内的子页（服务编辑器、风格编辑器）。
 *
 * - 在子页里再点一次侧栏当前分组：回到这个分组的根视图（macOS / Raycast 的习惯）。子页用 `useReselect` 接住，
 *   走自己「返回」的逻辑（有改动的新建草稿照样先确认放弃）；
 * - 子页用 `useSubviewTitle` 上报自己的标题：内容滚动后顶部条带的紧凑标题显示「DeepSeek」「添加服务」，而不是分组名。
 */
import { createContext, useContext, useEffect, useLayoutEffect, useRef } from 'react'
import type { SettingsSection } from '../shared/ipc'

export interface SettingsNav {
  section: SettingsSection
  /** 切到某个分组；`focus` 是分组内的锚点（例如 `style:<id>`、`provider:<id>`、`new-provider`），由该分组页解释 */
  go: (section: SettingsSection, focus?: string | null) => void
  /** 本次进入分组时带来的锚点（读过即可，不会重复触发） */
  focus: string | null
  /** 分组页处理完锚点后调用 */
  consumeFocus: () => void
  /** 在当前分组上再次点击侧栏的次数（子页据此回到根视图） */
  reselect: number
  /** 子页的标题（紧凑标题用）；null = 用分组名 */
  setTitle: (title: string | null) => void
}

export const SettingsNavContext = createContext<SettingsNav>({
  section: 'general',
  go: () => {},
  focus: null,
  consumeFocus: () => {},
  reselect: 0,
  setTitle: () => {},
})

export function useSettingsNav(): SettingsNav {
  return useContext(SettingsNavContext)
}

/** 子页挂载期间：再次点击当前分组时调用 `onReselect`（通常就是这个子页的「返回」） */
export function useReselect(onReselect: () => void): void {
  const { reselect } = useSettingsNav()
  const latest = useRef(onReselect)
  useLayoutEffect(() => {
    latest.current = onReselect
  })
  const seen = useRef(reselect)
  useEffect(() => {
    if (reselect === seen.current) return
    seen.current = reselect
    latest.current()
  }, [reselect])
}

/** 子页挂载期间上报自己的标题（紧凑标题显示它），卸载时还原为分组名 */
export function useSubviewTitle(title: string): void {
  const { setTitle } = useSettingsNav()
  useEffect(() => {
    setTitle(title)
  }, [title, setTitle])
  useEffect(() => () => setTitle(null), [setTitle])
}
