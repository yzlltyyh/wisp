/**
 * Overlay 入口专用的窄门面。Overlay（`entries/overlay.ts`、`src/overlay/**`）**只能**从这里导入
 * IPC，不得导入 `shared/ipc`（index 桶）或 `shared/ipc/commands`：后两者会把全部 87 条命令封装（与 panel 共享的 chunk）
 * 拖进 Overlay 的预加载（oxlint `no-restricted-imports` 对这两处路径报错）。
 *
 * 这里只引用 runtime / tauri（core + event）/ boot / events / url 与 `commands.overlay.ts`。
 */
export { getTransport, whenReady } from './runtime'
export { on, onMany, type EventListener } from './events'
export { boot, overlayIndexOf, type WindowKind } from './boot'
export {
  overlayCancel,
  overlayCommit,
  overlayDone,
  overlayFocus,
  overlayInit,
  overlayReady,
} from './commands.overlay'
export { fetchRgbaBitmap, resolveAssetUrl, wispUrl } from './url'
export { WispIpcError, type Unlisten } from './transport'
export type * from './types'
