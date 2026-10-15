/**
 * 快捷键显示串。全应用唯一的格式化函数；与 Rust `hotkeys::display`
 * 共用测试用例 `__fixtures__/format-cases.json`（每条 `{ combo, macos, windows, linux }`）。
 *
 * - macOS：符号、无分隔，顺序 ⌃⌥⇧⌘ + 主键；特殊键 ⎋ ⇥ ⌫ ⌦ ↩ ↑ ↓ ← → ⇞ ⇟ ↖ ↘ ⇪；Space 显示「Space」；字母大写；
 * - Windows：文字、`+` 分隔，顺序 Win、Ctrl、Alt、Shift；Linux 同 Windows，Win 改为 Super；
 * - 方向键三端都用箭头符号（Windows 写作 `Ctrl+↑`）。
 *
 * `hotkeyParts` 返回逐个键帽的文本，供 Kbd 组件渲染。
 */
import type { OsKind } from '../ipc/types'
import { parseCombo, type ParsedCombo } from './combo'

const MAC_KEYS: Record<string, string> = {
  Escape: '⎋',
  Tab: '⇥',
  Backspace: '⌫',
  Delete: '⌦',
  Enter: '↩',
  ArrowUp: '↑',
  ArrowDown: '↓',
  ArrowLeft: '←',
  ArrowRight: '→',
  PageUp: '⇞',
  PageDown: '⇟',
  Home: '↖',
  End: '↘',
  CapsLock: '⇪',
  Space: 'Space',
}

const TEXT_KEYS: Record<string, string> = {
  Escape: 'Esc',
  Tab: 'Tab',
  Backspace: 'Backspace',
  Delete: 'Delete',
  Enter: 'Enter',
  ArrowUp: '↑',
  ArrowDown: '↓',
  ArrowLeft: '←',
  ArrowRight: '→',
  PageUp: 'PgUp',
  PageDown: 'PgDn',
  Home: 'Home',
  End: 'End',
  Insert: 'Ins',
  PrintScreen: 'PrtSc',
  CapsLock: 'CapsLock',
  Space: 'Space',
}

const PUNCT: Record<string, string> = {
  Comma: ',',
  Period: '.',
  Slash: '/',
  Backslash: '\\',
  Semicolon: ';',
  Quote: "'",
  BracketLeft: '[',
  BracketRight: ']',
  Minus: '-',
  Equal: '=',
  Backquote: '`',
  NumpadAdd: '+',
  NumpadSubtract: '-',
  NumpadMultiply: '*',
  NumpadDivide: '/',
  NumpadDecimal: '.',
  NumpadEqual: '=',
}

/** 主键的显示文本 */
export function keyLabel(code: string, os: OsKind): string {
  if (code.startsWith('Key') && code.length === 4) return code.slice(3)
  if (code.startsWith('Digit') && code.length === 6) return code.slice(5)
  if (/^Numpad[0-9]$/.test(code)) return code.slice(6)
  const special = (os === 'macos' ? MAC_KEYS : TEXT_KEYS)[code]
  if (special) return special
  const punct = PUNCT[code]
  if (punct) return punct
  return code
}

function modifierLabels(p: ParsedCombo, os: OsKind): string[] {
  if (os === 'macos') {
    const out: string[] = []
    if (p.ctrl) out.push('⌃')
    if (p.alt) out.push('⌥')
    if (p.shift) out.push('⇧')
    if (p.meta) out.push('⌘')
    return out
  }
  const out: string[] = []
  if (p.meta) out.push(os === 'windows' ? 'Win' : 'Super')
  if (p.ctrl) out.push('Ctrl')
  if (p.alt) out.push('Alt')
  if (p.shift) out.push('Shift')
  return out
}

/** 逐个键帽的文本（Kbd 组件用） */
export function hotkeyParts(combo: string, os: OsKind): string[] {
  const p = parseCombo(combo, os)
  const parts = modifierLabels(p, os)
  if (p.code) parts.push(keyLabel(p.code, os))
  return parts
}

/** 显示串：macOS `⌥⇧D`，Windows `Ctrl+Alt+Shift+D`，Linux `Super+Alt+D` */
export function formatHotkey(combo: string, os: OsKind): string {
  return hotkeyParts(combo, os).join(os === 'macos' ? '' : '+')
}

/**
 * 一串连续快捷键的区间（文案里的「⌘1–⌘9」「Ctrl+1–9」）：macOS 两端都写完整组合，Windows / Linux 只把末尾换成最后一个
 * 主键（与系统文档的习惯一致）。全应用的区间写法只有这一处
 */
export function formatHotkeyRange(first: string, last: string, os: OsKind): string {
  const from = formatHotkey(first, os)
  if (os === 'macos') return `${from}–${formatHotkey(last, os)}`
  const parts = hotkeyParts(last, os)
  return `${from}–${parts[parts.length - 1] ?? ''}`
}

/** macOS 键名的朗读文本（符号 ⌥ ⇧ ↩ 读屏不一定读得出来；与 VoiceOver / 系统菜单的叫法一致） */
const MAC_SPOKEN: Record<string, string> = {
  Escape: 'Escape',
  Tab: 'Tab',
  Backspace: 'Delete',
  Delete: 'Forward Delete',
  Enter: 'Return',
  ArrowUp: 'Up Arrow',
  ArrowDown: 'Down Arrow',
  ArrowLeft: 'Left Arrow',
  ArrowRight: 'Right Arrow',
  PageUp: 'Page Up',
  PageDown: 'Page Down',
  Home: 'Home',
  End: 'End',
  CapsLock: 'Caps Lock',
  Space: 'Space',
}

/**
 * 读屏用的朗读文本（录制器等用键帽显示组合的控件的无障碍名称）：macOS `Option Shift D`、Windows `Ctrl Alt D`。
 * 键名是键盘上的英文名（与系统读屏一致），不随界面语言翻译
 */
export function hotkeySpeech(combo: string, os: OsKind): string {
  const p = parseCombo(combo, os)
  if (os !== 'macos') return hotkeyParts(combo, os).join(' ')
  const out: string[] = []
  if (p.ctrl) out.push('Control')
  if (p.alt) out.push('Option')
  if (p.shift) out.push('Shift')
  if (p.meta) out.push('Command')
  if (p.code) out.push(MAC_SPOKEN[p.code] ?? keyLabel(p.code, os))
  return out.join(' ')
}

/** 单独的修饰键状态的显示（录制器实时回显） */
export function formatModifiers(p: Omit<ParsedCombo, 'code'>, os: OsKind): string[] {
  return modifierLabels({ ...p, code: '' }, os)
}
