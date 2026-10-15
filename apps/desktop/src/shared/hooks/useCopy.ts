/**
 * 复制到剪贴板。
 *
 * 走 Rust 命令 `clipboard_copy_text`（会正常进入历史、受抑制规则约束；非 key 的 NSPanel、自定义 scheme 与
 * WebKitGTK 上 `navigator.clipboard` 不可靠）。只有**传输层不可用**（浏览器里直接打开页面：`not_implemented`）或
 * Rust 内部错误（`internal`）时才退回 `navigator.clipboard.writeText`；另外，平台层明确要求改由 WebView 写入时
 * （KDE Wayland 不接受后台 X11 写剪贴板、没有 XWayland：`platform` + `errors.platform.wayland_clipboard*`）
 * 也退回——此时面板仍有焦点，WebView 的写入有效。Rust 明确拒绝的其他情况（超过
 * 1 MB 的 `invalid_argument`、其他平台错误……）直接返回 false，不绕过 Rust 的上限与抑制规则，也不把失败伪装成成功。
 * 返回是否成功；界面反馈（对勾 / InlineToast / 失败提示）由调用方负责，见 CopyButton。
 */
import { clipboardCopyText } from '../ipc/commands'
import { WispIpcError } from '../ipc/transport'

export function useCopy(): (text: string) => Promise<boolean> {
  return copyText
}

/** 平台层要求改由 WebView 写剪贴板的原因（Rust 以 `IpcError { code: platform, message: <key> }` 下发） */
const WEBVIEW_COPY_REASONS: ReadonlySet<string> = new Set([
  'errors.platform.wayland_clipboard_bridge',
  'errors.platform.wayland_clipboard',
])

/** 可以退回 navigator.clipboard 的错误：传输层不可用、Rust 内部错误、平台层要求由 WebView 写入 */
function canFallBack(error: unknown): boolean {
  if (!(error instanceof WispIpcError)) return true
  if (error.code === 'not_implemented' || error.code === 'internal') return true
  return error.code === 'platform' && WEBVIEW_COPY_REASONS.has(error.message)
}

export async function copyText(text: string): Promise<boolean> {
  try {
    await clipboardCopyText(text)
    return true
  } catch (error) {
    if (!canFallBack(error)) return false
    try {
      await navigator.clipboard.writeText(text)
      return true
    } catch {
      return false
    }
  }
}
