/**
 * 剪贴板快捷键的组合串工具。快捷键本身在操作注册表（`actions/registry.ts`）里按操作声明，键位表、菜单与提示条都从
 * 那里派生；这里只放与平台有关、也被设置页引用的组合串写法（设置页不引入注册表）。
 */
import type { OsKind } from '../../shared/ipc/types'
import { parseCombo } from '../../shared/keys/combo'

/**
 * 纯文本粘贴第 n 条的组合串：macOS 是 ⌥⌘n（⌘⇧3 / ⌘⇧4 / ⌘⇧5 是系统截屏键、在系统层被拦下，到不了 WebView），
 * Windows / Linux 是 Ctrl+Shift+n。键位表、`aria-keyshortcuts` 与设置页的说明都从这里取
 */
export function plainNthCombo(os: OsKind, n: number): string {
  return os === 'macos' ? `Alt+Mod+Digit${n}` : `Mod+Shift+Digit${n}`
}

/** 组合串 → `aria-keyshortcuts` 的写法（`Meta+1`、`Control+Shift+1`：修饰键按 ARIA 的名称，主键用字符） */
export function ariaKeyshortcut(combo: string, os: OsKind): string {
  const p = parseCombo(combo, os)
  const parts: string[] = []
  if (p.ctrl) parts.push('Control')
  if (p.alt) parts.push('Alt')
  if (p.shift) parts.push('Shift')
  if (p.meta) parts.push('Meta')
  parts.push(p.code.replace(/^(Digit|Key)/, ''))
  return parts.join('+')
}
