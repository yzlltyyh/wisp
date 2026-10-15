/**
 * 5 个动作的默认快捷键（首选 / 备选）：直接取 Rust 生成的 `shared/ipc/fixtures/consts/HotkeyDefaults.json`
 * （wisp-types `tests/consts.rs` 由 `defaults.rs` 的 MACOS / WINDOWS / LINUX_HOTKEYS 写出并把关，
 * `node scripts/gen-fixtures.mjs` 重写）。Rust 增删动作时这里的类型检查会报缺键。
 *
 * 只用于**显示**：`HotkeyStatus` 在默认绑定注册失败时 `effective` 为空，界面要告诉用户「⌥D 没能注册」、
 * 「⌥D 被占用，已改用 ⌃⌥D」，需要知道 Rust 尝试的是哪个组合。注册、回退都由 Rust 决定。
 */
import TABLE from '../../shared/ipc/fixtures/consts/HotkeyDefaults.json'
import type { HotkeyAction, HotkeyStatus, OsKind } from '../../shared/ipc/types'

export interface HotkeyDefault {
  preferred: string
  fallback: string
}

export const HOTKEY_DEFAULTS: Record<OsKind, Record<HotkeyAction, HotkeyDefault>> = TABLE

/** 这个目标首选的组合：自定义绑定就是它；默认绑定取平台默认表；风格 / 未绑定为 null */
export function preferredCombo(s: HotkeyStatus | undefined, os: OsKind): string | null {
  if (!s || s.binding === 'none') return null
  if (s.binding !== 'default') return s.binding
  return s.target.kind === 'action' ? HOTKEY_DEFAULTS[os][s.target.action].preferred : null
}

/** 录制器显示的组合：生效的组合；没注册上时显示 Rust 尝试的那个（录制器标红） */
export function shownCombo(s: HotkeyStatus | undefined, os: OsKind): string | null {
  if (!s) return null
  if (s.effective) return s.effective
  return preferredCombo(s, os)
}
