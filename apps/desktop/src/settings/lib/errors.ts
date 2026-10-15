/**
 * IpcError → 界面文案。Rust 的 `message` 可能是 i18n key（`errors.platform.*`、`hotkeys.*`），也可能是英文开发者文本
 * （「style.name must not be empty」）：前者翻译后显示，后者不直接给用户看，只显示错误码对应的通用说明。
 */
import { tKey } from '../../shared/i18n/i18n'
import { WispIpcError } from '../../shared/ipc'

const KEY_LIKE = /^(errors|hotkeys|settings|onboarding)\.[a-z0-9_.-]+$/

/** 是不是可以翻译的文案 key（而不是英文开发者文本） */
export function isKeyLike(message: string): boolean {
  return KEY_LIKE.test(message)
}

/**
 * `params`：文案 key 的参数（例如 `hotkey_set` 拒绝时下发的 `hotkeys.conflict.*` 只带 key、不带 `{combo}` / `{action}`，
 * 调用方知道这些值）；仍然缺的占位符去掉，不把「{combo}」显示给用户
 */
export function ipcErrorText(
  error: unknown,
  command = 'unknown',
  params?: Record<string, string>,
): string {
  const e = WispIpcError.from(command, error)
  if (KEY_LIKE.test(e.message)) {
    const text = tKey(e.message, params)
    if (text) return text.replace(/\{\w+\}\s*/g, '').trim()
  }
  return tKey(`errors.ipc.${e.code}`, undefined, tKey('errors.ipc.internal', undefined, e.code))
}
