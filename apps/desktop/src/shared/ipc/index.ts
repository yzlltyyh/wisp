/**
 * 前端访问 Tauri 的唯一入口：`api`（命令）、`on`（事件）、`boot`（窗口启动信息）。
 * 除 `src/shared/ipc/**` 外禁止导入 `@tauri-apps/*`（oxlint no-restricted-imports）。
 * Overlay（`entries/overlay.ts`、`src/overlay/**`）不得导入本桶，改用窄门面 `./overlay`（体积）。
 * 窗口能力只有拖动 / 关闭（capability 比 Tauri 的 core:default 窄）：其他窗口操作一律走 Rust 命令。
 */
import { applyAppearance, applyDeveloperMode } from './boot'
import { onMany } from './events'

export * as api from './commands'
export type {
  CommandArgs,
  CommandMap,
  CommandName,
  CommandResult,
  StreamCommandName,
  StreamListener,
} from './commands'
export { on, onMany, type EventListener } from './events'
export {
  applyAppearance,
  applyBootAttributes,
  applyDeveloperMode,
  boot,
  devFlags,
  overlayIndexOf,
  pinnedIdOf,
  windowKindOf,
  type DevFlags,
  type WindowKind,
} from './boot'
export { getTransport, setTransport, whenReady } from './runtime'
export { WispIpcError, type Transport, type Unlisten, type WindowFacade } from './transport'
export { isSettingsSection, SETTINGS_SECTIONS, type SettingsSection } from './sections'
export { fetchRgbaBitmap, resolveAssetUrl, wispUrl } from './url'
export type * from './types'

/**
 * 订阅 `appearance:changed`（把主题 / 强调色 / 减少动效等同步到 `<html>`）与 `settings:changed`（开发者模式 → `boot.dev`）。
 * 各入口在挂载后调用一次；Overlay 生命周期很短，不需要。返回取消函数（可在订阅完成前调用）
 */
export function startAppearanceSync(): Promise<() => void> {
  return Promise.resolve(
    onMany({
      'appearance:changed': applyAppearance,
      'settings:changed': (s) => applyDeveloperMode(s.settings.general.developer_mode),
    }),
  )
}
