/**
 * 组合键的解析、规范化与事件转换。
 *
 * 写法与 Rust 侧相同，都是 global-hotkey 可解析的串：`修饰键+…+主键`，主键用 `KeyboardEvent.code`（物理键位，
 * 不受输入法与 ⌥ 产生的特殊字符影响），例如 `Alt+KeyD`、`Control+Alt+Shift+KeyS`、`Super+Shift+KeyV`。
 * 全局快捷键与录制器按物理键位；应用内快捷键（keyRouter）的字母 / 标点按当前布局产生的字符匹配（matchesEvent）。
 * 规范顺序 `Control+Alt+Shift+Super`（与 types.ts 的 Binding 注释一致）。
 *
 * 另外接受前端专用的 `Mod`（macOS = ⌘，其他平台 = Ctrl）及常见别名（Ctrl / Option / Cmd / Command / Meta / Win /
 * CmdOrCtrl …），以及单字符主键（`a`、`1`、`,`）。
 */
import type { OsKind } from '../ipc/types'

export interface ParsedCombo {
  ctrl: boolean
  alt: boolean
  shift: boolean
  /** macOS ⌘ / Windows Win / Linux Super */
  meta: boolean
  /** 规范化后的主键（KeyboardEvent.code 形式）；只有修饰键时为空串 */
  code: string
}

const MOD_ALIASES: Record<string, 'ctrl' | 'alt' | 'shift' | 'meta' | 'mod'> = {
  control: 'ctrl',
  ctrl: 'ctrl',
  alt: 'alt',
  option: 'alt',
  opt: 'alt',
  shift: 'shift',
  super: 'meta',
  meta: 'meta',
  cmd: 'meta',
  command: 'meta',
  win: 'meta',
  windows: 'meta',
  mod: 'mod',
  cmdorctrl: 'mod',
  cmdorcontrol: 'mod',
  commandorctrl: 'mod',
  commandorcontrol: 'mod',
}

/** 主键别名 → KeyboardEvent.code */
const KEY_ALIASES: Record<string, string> = {
  esc: 'Escape',
  escape: 'Escape',
  return: 'Enter',
  enter: 'Enter',
  space: 'Space',
  spacebar: 'Space',
  ' ': 'Space',
  tab: 'Tab',
  backspace: 'Backspace',
  delete: 'Delete',
  del: 'Delete',
  up: 'ArrowUp',
  down: 'ArrowDown',
  left: 'ArrowLeft',
  right: 'ArrowRight',
  arrowup: 'ArrowUp',
  arrowdown: 'ArrowDown',
  arrowleft: 'ArrowLeft',
  arrowright: 'ArrowRight',
  home: 'Home',
  end: 'End',
  pageup: 'PageUp',
  pagedown: 'PageDown',
  insert: 'Insert',
  printscreen: 'PrintScreen',
  capslock: 'CapsLock',
  ',': 'Comma',
  comma: 'Comma',
  '.': 'Period',
  period: 'Period',
  '/': 'Slash',
  slash: 'Slash',
  '\\': 'Backslash',
  backslash: 'Backslash',
  ';': 'Semicolon',
  semicolon: 'Semicolon',
  "'": 'Quote',
  quote: 'Quote',
  '[': 'BracketLeft',
  bracketleft: 'BracketLeft',
  ']': 'BracketRight',
  bracketright: 'BracketRight',
  '-': 'Minus',
  minus: 'Minus',
  '=': 'Equal',
  equal: 'Equal',
  '`': 'Backquote',
  backquote: 'Backquote',
}

const MODIFIER_CODES = new Set([
  'ShiftLeft',
  'ShiftRight',
  'ControlLeft',
  'ControlRight',
  'AltLeft',
  'AltRight',
  'MetaLeft',
  'MetaRight',
  'OSLeft',
  'OSRight',
  'Fn',
  'FnLock',
])

/** 主键规范化为 KeyboardEvent.code；无法识别时原样返回 */
export function normalizeKey(token: string): string {
  if (token === '') return ''
  const lower = token.toLowerCase()
  const alias = KEY_ALIASES[lower]
  if (alias) return alias
  if (/^[a-z]$/i.test(token)) return `Key${token.toUpperCase()}`
  if (/^[0-9]$/.test(token)) return `Digit${token}`
  if (/^key[a-z]$/i.test(token)) return `Key${token.slice(3).toUpperCase()}`
  if (/^digit[0-9]$/i.test(token)) return `Digit${token.slice(5)}`
  if (/^f([1-9]|1[0-9]|2[0-4])$/i.test(token)) return token.toUpperCase()
  if (/^numpad/i.test(token)) return `Numpad${token.slice(6)}`
  return token
}

/** 解析组合串（大小写不敏感，`+` 分隔；`Mod` 按平台展开） */
export function parseCombo(combo: string, os: OsKind): ParsedCombo {
  const out: ParsedCombo = { ctrl: false, alt: false, shift: false, meta: false, code: '' }
  // 允许主键本身是「+」：`Ctrl++` → ['Ctrl', '+']
  const tokens = combo.endsWith('++') ? [...combo.slice(0, -2).split('+'), '+'] : combo.split('+')
  for (const raw of tokens) {
    const token = raw.trim()
    if (token === '') continue
    const mod = MOD_ALIASES[token.toLowerCase()]
    if (mod === 'mod') {
      if (os === 'macos') out.meta = true
      else out.ctrl = true
    } else if (mod) {
      out[mod] = true
    } else {
      out.code = token === '+' ? 'Equal' : normalizeKey(token)
    }
  }
  return out
}

/** 规范串：`Control+Alt+Shift+Super+<code>`（存储、比较、上报 Rust 都用它） */
export function toCanonical(p: ParsedCombo): string {
  const parts: string[] = []
  if (p.ctrl) parts.push('Control')
  if (p.alt) parts.push('Alt')
  if (p.shift) parts.push('Shift')
  if (p.meta) parts.push('Super')
  if (p.code) parts.push(p.code)
  return parts.join('+')
}

/** 组合串规范化（别名、大小写、顺序都统一） */
export function canonicalCombo(combo: string, os: OsKind): string {
  return toCanonical(parseCombo(combo, os))
}

/** 事件是否为单独的修饰键 */
export function isModifierEvent(event: KeyboardEvent): boolean {
  return MODIFIER_CODES.has(event.code) || event.key === 'Meta' || event.key === 'OS'
}

/** 事件当前按下的修饰键 */
export function modifiersOf(event: KeyboardEvent): Omit<ParsedCombo, 'code'> {
  return {
    ctrl: event.ctrlKey,
    alt: event.altKey,
    shift: event.shiftKey,
    meta: event.metaKey,
  }
}

/** 事件的主键（NumpadEnter 视同 Enter；单独的修饰键返回空串） */
export function codeOf(event: KeyboardEvent): string {
  if (isModifierEvent(event)) return ''
  if (event.code === 'NumpadEnter') return 'Enter'
  if (event.code) return event.code
  // 个别合成事件只有 key（测试、辅助技术）
  return normalizeKey(event.key)
}

/**
 * KeyboardEvent → 规范组合串（快捷键录制器用）。单独按下修饰键时返回 null（录制器只回显修饰键）。
 * `_os` 保留给将来区分平台的规则（例如 macOS 上把 Fn 合成位剥离）。
 */
export function comboFromEvent(event: KeyboardEvent, _os: OsKind): string | null {
  const code = codeOf(event)
  if (!code) return null
  return toCanonical({ ...modifiersOf(event), code })
}

/** 标点主键 → 它在 US 布局上产生的字符（应用内快捷键按字符匹配，见 matchesEvent） */
const PUNCTUATION: Record<string, string> = {
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
}
const PUNCTUATION_CODE: Record<string, string> = Object.fromEntries(
  Object.entries(PUNCTUATION).map(([code, ch]) => [ch, code]),
)
const LATIN = /^[a-z]$/i

/**
 * 应用内快捷键的主键匹配（布局感知）：字母键与标点键按**当前布局产生的字符**（`event.key`）匹配——AZERTY 上 ⌘W
 * 按的是物理 KeyZ、Dvorak 上 ⌘C 按的是物理 KeyI，与系统和其他应用的应用内快捷键一致；`event.key` 不是拉丁字母 /
 * 这组标点时（西里尔等非拉丁布局、⌥ 产生的特殊字符、死键、Shift 改变了标点）退回物理键位（`event.code`）。
 * 其他键（数字、方向键、功能键……）一律按物理键位。全局快捷键与录制器仍然只用物理键位（comboFromEvent）。
 */
function keyMatches(code: string, event: KeyboardEvent): boolean {
  const key = event.key ?? ''
  const letter = /^Key([A-Z])$/.exec(code)
  if (letter) {
    if (LATIN.test(key)) return key.toUpperCase() === letter[1]
    return codeOf(event) === code
  }
  if (code in PUNCTUATION) {
    const produced = PUNCTUATION_CODE[key]
    if (produced) return produced === code
    // 这个物理键在当前布局上产生的是字母（例如 Dvorak 的物理 Comma 键是 w）：它不是这个标点，不再按物理键位匹配
    if (LATIN.test(key)) return false
    return codeOf(event) === code
  }
  return codeOf(event) === code
}

/** 事件是否与解析后的组合完全匹配（修饰键必须一致，不允许多按；主键按布局感知规则，见 keyMatches） */
export function matchesEvent(p: ParsedCombo, event: KeyboardEvent): boolean {
  return (
    p.code !== '' &&
    p.ctrl === event.ctrlKey &&
    p.alt === event.altKey &&
    p.shift === event.shiftKey &&
    p.meta === event.metaKey &&
    keyMatches(p.code, event)
  )
}
