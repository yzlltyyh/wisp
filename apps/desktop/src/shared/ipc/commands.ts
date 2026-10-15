/**
 * 命令封装。
 *
 * - 每条命令一个**具名导出**函数（命令名的 camelCase），不组装成对象字面量，保证 tree-shake（Overlay 只打进用到的几个）；
 * - 参数对象的键保持 snake_case；
 * - 失败时抛 `WispIpcError`；
 * - `CommandMap` 同时约束 mock 路由的处理函数（mock/router.ts）；
 * - Overlay 的 6 条命令定义在 `commands.overlay.ts`（体积原因，见该文件），这里继承其类型并原样 re-export；
 * - 命令集合与每条命令的参数键由 Rust 的 `ipc::contract_tests` 与 `ipc/mod.rs`、`cmd_*.rs` 逐项比对。
 */
import type { OverlayCommandMap } from './commands.overlay'
import { getTransport } from './runtime'
import type * as T from './types'

export {
  overlayCancel,
  overlayCommit,
  overlayDone,
  overlayFocus,
  overlayInit,
  overlayReady,
  type OverlayCommandMap,
} from './commands.overlay'

type NoArgs = Record<string, never>

/** 命令名 → 参数（不含流式命令的 `on_event` 通道）与返回值。Overlay 的 6 条见 `OverlayCommandMap` */
export interface CommandMap extends OverlayCommandMap {
  // ----- 面板 / 窗口
  panel_ready: { args: { session_id: number; content_height: number }; ret: T.PanelShown | null }
  panel_painted: { args: { session_id: number }; ret: void }
  panel_resize: {
    args: { session_id: number; content_height: number; spring?: T.ResizeSpring | null }
    ret: void
  }
  panel_set_width: { args: { session_id: number; width: number }; ret: void }
  panel_hide: { args: { session_id: number; reason: T.HideReason }; ret: void }
  panel_activate: { args: { session_id: number }; ret: void }
  panel_drag_start: { args: { session_id: number }; ret: void }
  panel_pin: { args: { session_id: number; state: T.PinState; expand: boolean }; ret: string }
  pinned_init: { args: { pinned_id: string }; ret: T.PinState }
  pinned_ready: { args: { pinned_id: string; content_height: number }; ret: void }
  pinned_set_on_top: { args: { pinned_id: string; on_top: boolean }; ret: void }
  window_open: { args: { target: T.WindowTarget }; ret: void }
  // ----- 流 / 编排
  stream_attach: { args: { request_id: string }; ret: void }
  cancel: { args: { request_id: string }; ret: void }
  translate_start: { args: { req: T.TranslateReq }; ret: T.StartAck }
  ocr_start: { args: { req: T.OcrReq }; ret: T.StartAck }
  capture_release: { args: { capture_id: string }; ret: void }
  trigger_action: {
    args: { action: T.HotkeyAction; text: string | null; origin: T.ActionOrigin }
    ret: void
  }
  // overlay_init / overlay_ready / overlay_commit / overlay_done / overlay_cancel / overlay_focus：继承自 OverlayCommandMap
  // ----- 剪贴板
  clipboard_recent: {
    args: { offset: number; limit: number; filter: T.ClipFilter }
    ret: T.ClipPage
  }
  clipboard_search: {
    args: { query: string; filter: T.ClipFilter; limit: number }
    ret: T.ClipPage
  }
  clipboard_get: { args: { id: number }; ret: T.ClipItemDetail | null }
  clipboard_paste: { args: { id: number; mode: T.PasteMode }; ret: T.PasteOutcome }
  clipboard_pin: { args: { id: number; pinned: boolean }; ret: T.ClipItemMeta }
  clipboard_delete: { args: { id: number; force: boolean }; ret: void }
  clipboard_clear: { args: { keep_pinned: boolean }; ret: number }
  clipboard_set_paused: { args: { paused: boolean }; ret: void }
  clipboard_save_image: { args: { id: number }; ret: string | null }
  clipboard_reveal: { args: { id: number }; ret: void }
  clipboard_open_link: { args: { id: number }; ret: void }
  clipboard_send_to: { args: { id: number; target: T.SendTarget }; ret: void }
  clipboard_copy_text: { args: { text: string }; ret: void }
  ocr_history_list: { args: { offset: number; limit: number }; ret: T.OcrHistoryItem[] }
  ocr_history_delete: { args: { id: number }; ret: void }
  ocr_history_clear: { args: NoArgs; ret: void }
  apps_running: { args: NoArgs; ret: T.SourceApp[] }
  // ----- 设置 / 风格 / 术语表
  settings_get: { args: NoArgs; ret: T.SettingsSnapshot }
  settings_patch: { args: { patch: unknown }; ret: T.SettingsSnapshot }
  excluded_app_upsert: { args: { entry: T.ExcludedApp }; ret: T.SettingsSnapshot }
  excluded_app_remove: { args: { os: T.OsKind; app_id: string }; ret: T.SettingsSnapshot }
  app_keymap_upsert: { args: { entry: T.AppKeymap }; ret: T.SettingsSnapshot }
  app_keymap_remove: { args: { os: T.OsKind; app_id: string }; ret: T.SettingsSnapshot }
  styles_list: { args: NoArgs; ret: T.Style[] }
  style_save: { args: { style: T.Style }; ret: T.Style[] }
  style_delete: { args: { id: string }; ret: T.Style[] }
  style_reset: { args: { id: string }; ret: T.Style[] }
  styles_reorder: { args: { ids: string[] }; ret: T.Style[] }
  style_preview: { args: { style: T.Style; sample: string }; ret: T.PromptPreview }
  glossary_get: { args: NoArgs; ret: T.GlossaryEntry[] }
  glossary_set: { args: { entries: T.GlossaryEntry[] }; ret: void }
  glossary_upsert: { args: { entry: T.GlossaryEntry; previous_source: string | null }; ret: void }
  glossary_remove: { args: { source: string }; ret: void }
  glossary_import: { args: NoArgs; ret: number }
  glossary_export: { args: NoArgs; ret: string | null }
  // ----- 模型服务
  provider_templates: { args: NoArgs; ret: T.ProviderTemplate[] }
  provider_new: { args: { template_id: string }; ret: T.Provider }
  provider_delete: { args: { provider_id: string }; ret: T.SettingsSnapshot }
  provider_save: { args: { provider: T.Provider }; ret: T.SettingsSnapshot }
  provider_add: { args: { provider: T.Provider }; ret: T.SettingsSnapshot }
  providers_reorder: { args: { ids: string[] }; ret: T.SettingsSnapshot }
  provider_test: { args: { provider: T.Provider; key: string | null }; ret: T.ProviderTestReport }
  provider_models: { args: { provider: T.Provider; key: string | null }; ret: T.ModelEntry[] }
  llm_preview_url: {
    args: { protocol: T.Protocol; base_url: string; model_id: string | null }
    ret: T.UrlPreview
  }
  secret_set: { args: { provider_id: string; key: string }; ret: void }
  secret_delete: { args: { provider_id: string }; ret: void }
  model_caps_reset: { args: { provider_id: string; model_id: string }; ret: void }
  // ----- 应用
  permissions_status: { args: NoArgs; ret: T.PermissionSet }
  permissions_request: { args: { kind: T.PermissionKind }; ret: T.PermissionSet }
  permissions_open: { args: { kind: T.PermissionKind }; ret: void }
  permissions_watch: { args: { active: boolean }; ret: void }
  app_info: { args: NoArgs; ret: T.AppMeta }
  onboarding_complete: { args: NoArgs; ret: void }
  appearance_get: { args: NoArgs; ret: T.AppearanceState }
  open_path: { args: { kind: T.PathKind }; ret: void }
  open_url: { args: { url: string }; ret: void }
  app_relaunch: { args: { resume: T.WindowTarget | null }; ret: void }
  app_quit: { args: NoArgs; ret: void }
  diagnostics_export: { args: NoArgs; ret: string | null }
  update_check: { args: NoArgs; ret: T.UpdateStatus }
  update_install: { args: NoArgs; ret: void }
  update_status: { args: NoArgs; ret: T.UpdateStatus }
  // ----- 快捷键
  hotkey_status: { args: NoArgs; ret: T.HotkeyStatus[] }
  hotkey_record_begin: { args: NoArgs; ret: void }
  hotkey_record_end: { args: NoArgs; ret: void }
  hotkey_check: { args: { target: T.HotkeyTarget; combo: string }; ret: T.ConflictReport }
  hotkey_set: {
    args: { target: T.HotkeyTarget; binding: T.Binding; force: boolean }
    ret: T.HotkeyStatus[]
  }
  hotkey_reset_defaults: { args: NoArgs; ret: T.HotkeyStatus[] }
  hotkey_test_begin: { args: { target: T.HotkeyTarget }; ret: void }
  hotkey_test_end: { args: NoArgs; ret: void }
  neighbor_apps: { args: NoArgs; ret: T.NeighborHit[] }
  keyboard_advice: { args: NoArgs; ret: T.KeyboardAdvice }
  // ----- 同步
  sync_status: { args: NoArgs; ret: T.SyncStatus }
  sync_devices: { args: NoArgs; ret: T.SyncDevice[] }
  sync_probe: { args: { account: T.SyncAccount; password: string | null }; ret: T.SyncProbe }
  sync_connect: {
    args: {
      account: T.SyncAccount
      password: string | null
      passphrase: string
      content: T.SyncContent
    }
    ret: T.SyncConnectResult
  }
  sync_now: { args: NoArgs; ret: T.SyncStatus }
  sync_disconnect: { args: NoArgs; ret: void }
  sync_reset_remote: {
    args: { account: T.SyncAccount | null; password: string | null }
    ret: void
  }
  sync_generate_passphrase: { args: NoArgs; ret: string }
}

export type CommandName = keyof CommandMap
export type CommandArgs<K extends CommandName> = CommandMap[K]['args']
export type CommandResult<K extends CommandName> = CommandMap[K]['ret']
/** 带 `on_event: Channel<StreamEvent>` 的命令 */
export type StreamCommandName = 'stream_attach' | 'translate_start' | 'ocr_start'
export type StreamListener = (event: T.StreamEvent) => void

/** 全部命令名（`check-contract` 与 mock 路由的完整性测试使用；不使用时会被 tree-shake） */
export const COMMAND_NAMES = [
  'panel_ready',
  'panel_painted',
  'panel_resize',
  'panel_set_width',
  'panel_hide',
  'panel_activate',
  'panel_drag_start',
  'panel_pin',
  'pinned_init',
  'pinned_ready',
  'pinned_set_on_top',
  'window_open',
  'stream_attach',
  'cancel',
  'translate_start',
  'ocr_start',
  'capture_release',
  'trigger_action',
  'overlay_init',
  'overlay_ready',
  'overlay_commit',
  'overlay_done',
  'overlay_cancel',
  'overlay_focus',
  'clipboard_recent',
  'clipboard_search',
  'clipboard_get',
  'clipboard_paste',
  'clipboard_pin',
  'clipboard_delete',
  'clipboard_clear',
  'clipboard_set_paused',
  'clipboard_save_image',
  'clipboard_reveal',
  'clipboard_open_link',
  'clipboard_send_to',
  'clipboard_copy_text',
  'ocr_history_list',
  'ocr_history_delete',
  'ocr_history_clear',
  'apps_running',
  'settings_get',
  'settings_patch',
  'excluded_app_upsert',
  'excluded_app_remove',
  'app_keymap_upsert',
  'app_keymap_remove',
  'styles_list',
  'style_save',
  'style_delete',
  'style_reset',
  'styles_reorder',
  'style_preview',
  'glossary_get',
  'glossary_set',
  'glossary_upsert',
  'glossary_remove',
  'glossary_import',
  'glossary_export',
  'provider_templates',
  'provider_new',
  'provider_delete',
  'provider_save',
  'provider_add',
  'providers_reorder',
  'provider_test',
  'provider_models',
  'llm_preview_url',
  'secret_set',
  'secret_delete',
  'model_caps_reset',
  'permissions_status',
  'permissions_request',
  'permissions_open',
  'permissions_watch',
  'app_info',
  'onboarding_complete',
  'appearance_get',
  'open_path',
  'open_url',
  'app_relaunch',
  'app_quit',
  'diagnostics_export',
  'update_check',
  'update_install',
  'update_status',
  'hotkey_status',
  'hotkey_record_begin',
  'hotkey_record_end',
  'hotkey_check',
  'hotkey_set',
  'hotkey_reset_defaults',
  'hotkey_test_begin',
  'hotkey_test_end',
  'neighbor_apps',
  'keyboard_advice',
  'sync_status',
  'sync_devices',
  'sync_probe',
  'sync_connect',
  'sync_now',
  'sync_disconnect',
  'sync_reset_remote',
  'sync_generate_passphrase',
] as const satisfies readonly CommandName[]

// 编译期保证 COMMAND_NAMES 覆盖 CommandMap 的全部键（缺一个就报错，纯类型、无运行时代码）
type AssertNever<Never extends never> = Never
export type MissingCommandNames = AssertNever<Exclude<CommandName, (typeof COMMAND_NAMES)[number]>>

export const STREAM_COMMANDS: readonly StreamCommandName[] = [
  'stream_attach',
  'translate_start',
  'ocr_start',
]

function call<K extends CommandName>(cmd: K, args: CommandArgs<K>): Promise<CommandResult<K>> {
  return getTransport().invoke<CommandResult<K>>(cmd, args)
}

function stream<K extends StreamCommandName>(
  cmd: K,
  args: CommandArgs<K>,
  onEvent: StreamListener,
): Promise<CommandResult<K>> {
  return getTransport().openStream<CommandResult<K>>(cmd, args, onEvent)
}

const none: NoArgs = {}

// ===== 面板 / 窗口 =====

/** 前端已用 opacity 0 同步渲染并测量完成 → Rust 同步 show；返回后立即播放进入动画。session 过期时为 null */
export function panelReady(session_id: number, content_height: number) {
  return call('panel_ready', { session_id, content_height })
}
/** 第一个 opacity > 0 的帧（在 rAF 内调用）→ perf 的 Shown 埋点 */
export function panelPainted(session_id: number) {
  return call('panel_painted', { session_id })
}
/** 卡片目标高度（逻辑 px）；Css 帧模式按阶梯调整窗口，Native 帧模式由 Rust 逐帧弹簧 */
/** `spring`：Native 帧窗口高度的弹簧（缺省 gentle；浮层预留引起的长高 / 回收传 snappy） */
export function panelResize(session_id: number, content_height: number, spring?: T.ResizeSpring) {
  return call(
    'panel_resize',
    spring ? { session_id, content_height, spring } : { session_id, content_height },
  )
}
/**
 * 视图内切换（输入 → 翻译等，`StartAck.width`）的新卡片宽度：ViewSwitch 交换的那次提交（旧内容已完全透明、新视图
 * 还是 opacity 0）调用，Rust 立即改窗口宽度（位置保持）；宽度没变时为空操作
 */
export function panelSetWidth(session_id: number, width: number) {
  return call('panel_set_width', { session_id, width })
}
/** 退出动画结束后调用 */
export function panelHide(session_id: number, reason: T.HideReason) {
  return call('panel_hide', { session_id, reason })
}
/** 翻译视图被点击后成为 key（Windows 上去掉 NOACTIVATE） */
export function panelActivate(session_id: number) {
  return call('panel_activate', { session_id })
}
/** 从面板头部 / 空白处开始拖动（所有面板视图，经 shared/hooks/usePanelDrag 调用） */
export function panelDragStart(session_id: number) {
  return call('panel_drag_start', { session_id })
}
/** 脱离到 Pinned 窗口，返回 pinned_id */
export function panelPin(session_id: number, state: T.PinState, expand: boolean) {
  return call('panel_pin', { session_id, state, expand })
}
export function pinnedInit(pinned_id: string) {
  return call('pinned_init', { pinned_id })
}
/**
 * Pinned 首帧就绪：`pinnedInit` → `streamAttach` 回放完 → 用 opacity 0 同步渲染并测量 → 调用本命令。
 * Rust 在命令内同一帧 show pinned 并隐藏 panel（无空帧）；返回后播放 `presets.pinLift`
 */
export function pinnedReady(pinned_id: string, content_height: number) {
  return call('pinned_ready', { pinned_id, content_height })
}
export function pinnedSetOnTop(pinned_id: string, on_top: boolean) {
  return call('pinned_set_on_top', { pinned_id, on_top })
}
export function windowOpen(target: T.WindowTarget) {
  return call('window_open', { target })
}

// ===== 流 / 编排 =====

/** 回放已有事件，之后继续推送实时事件；每次 attach 前应先重置视图 */
export function streamAttach(request_id: string, onEvent: StreamListener) {
  return stream('stream_attach', { request_id }, onEvent)
}
export function cancel(request_id: string) {
  return call('cancel', { request_id })
}
export function translateStart(req: T.TranslateReq, onEvent: StreamListener) {
  return stream('translate_start', { req }, onEvent)
}
export function ocrStart(req: T.OcrReq, onEvent: StreamListener) {
  return stream('ocr_start', { req }, onEvent)
}
export function captureRelease(capture_id: string) {
  return call('capture_release', { capture_id })
}
/**
 * 前端发起的全局动作。`text` 为 null 时等同按下该动作的快捷键（引导页「试一试」）；
 * 非 null 时只允许 translate_selection / translate_input，表示把这段文本直接送去翻译（剪贴板条目送翻译）。
 * `origin` 进入性能埋点与日志
 */
export function triggerAction(
  action: T.HotkeyAction,
  origin: T.ActionOrigin,
  text: string | null = null,
) {
  return call('trigger_action', { action, text, origin })
}
// overlayInit / overlayReady / overlayCommit / overlayDone / overlayCancel / overlayFocus：见 commands.overlay.ts（上方已 re-export）

// ===== 剪贴板 =====

export function clipboardRecent(offset: number, limit: number, filter: T.ClipFilter) {
  return call('clipboard_recent', { offset, limit, filter })
}
export function clipboardSearch(query: string, filter: T.ClipFilter, limit: number) {
  return call('clipboard_search', { query, filter, limit })
}
export function clipboardGet(id: number) {
  return call('clipboard_get', { id })
}
/** Rust 负责立即隐藏面板并还原焦点；只复制时由 Rust 触发 toast 视图 */
export function clipboardPaste(id: number, mode: T.PasteMode) {
  return call('clipboard_paste', { id, mode })
}
export function clipboardPin(id: number, pinned: boolean) {
  return call('clipboard_pin', { id, pinned })
}
/** 置顶项要求 force（前端负责二次确认） */
export function clipboardDelete(id: number, force: boolean) {
  return call('clipboard_delete', { id, force })
}
export function clipboardClear(keep_pinned: boolean) {
  return call('clipboard_clear', { keep_pinned })
}
export function clipboardSetPaused(paused: boolean) {
  return call('clipboard_set_paused', { paused })
}
export function clipboardSaveImage(id: number) {
  return call('clipboard_save_image', { id })
}
export function clipboardReveal(id: number) {
  return call('clipboard_reveal', { id })
}
export function clipboardOpenLink(id: number) {
  return call('clipboard_open_link', { id })
}
export function clipboardSendTo(id: number, target: T.SendTarget) {
  return call('clipboard_send_to', { id, target })
}
/**
 * 写系统剪贴板（复制按钮、OCR 自动复制兜底；≤ 1 MB）。`useCopy` 优先走它而不是 navigator.clipboard
 * （后者在非 key 的 NSPanel、自定义 scheme、WebKitGTK 上不可靠）；会正常进入历史并受抑制规则约束
 */
export function clipboardCopyText(text: string) {
  return call('clipboard_copy_text', { text })
}
export function ocrHistoryList(offset: number, limit: number) {
  return call('ocr_history_list', { offset, limit })
}
export function ocrHistoryDelete(id: number) {
  return call('ocr_history_delete', { id })
}
export function ocrHistoryClear() {
  return call('ocr_history_clear', none)
}
/**
 * 正在运行的应用（剪贴板排除名单的应用选择器，与最近来源合并）。
 * macOS = regular / accessory 应用；Windows = 有顶层窗口的进程；X11 = `_NET_CLIENT_LIST` 的 WM_CLASS（Wayland 退回 /proc
 * 与 .desktop 索引）。不含 Wisp 自己，按 id 去重、按名称排序；`icon_url` 是 `wisp://icon/` 资源路径，取不到时为 null
 */
export function appsRunning() {
  return call('apps_running', none)
}

// ===== 设置 / 风格 / 术语表 =====

export function settingsGet() {
  return call('settings_get', none)
}
/** JSON Merge Patch（RFC 7396，数组整体替换） */
export function settingsPatch(patch: unknown) {
  return call('settings_patch', { patch })
}
/** 加入或更新一个剪贴板排除应用（按系统与应用 id 只改这一项） */
export function excludedAppUpsert(entry: T.ExcludedApp) {
  return call('excluded_app_upsert', { entry })
}
/** 从剪贴板排除名单移除一个应用（已经不在了也算成功） */
export function excludedAppRemove(os: T.OsKind, app_id: string) {
  return call('excluded_app_remove', { os, app_id })
}
/** 加入或更新一条应用的复制 / 粘贴键（按系统与应用 id 只改这一项） */
export function appKeymapUpsert(entry: T.AppKeymap) {
  return call('app_keymap_upsert', { entry })
}
/** 移除一条应用的复制 / 粘贴键（已经不在了也算成功） */
export function appKeymapRemove(os: T.OsKind, app_id: string) {
  return call('app_keymap_remove', { os, app_id })
}
export function stylesList() {
  return call('styles_list', none)
}
export function styleSave(style: T.Style) {
  return call('style_save', { style })
}
export function styleDelete(id: string) {
  return call('style_delete', { id })
}
export function styleReset(id: string) {
  return call('style_reset', { id })
}
export function stylesReorder(ids: string[]) {
  return call('styles_reorder', { ids })
}
export function stylePreview(style: T.Style, sample: string) {
  return call('style_preview', { style, sample })
}
export function glossaryGet() {
  return call('glossary_get', none)
}
export function glossarySet(entries: T.GlossaryEntry[]) {
  return call('glossary_set', { entries })
}
/** 写入一条术语（只改这一条）；`previous_source`：这一行改动前的原文（新行为 null） */
export function glossaryUpsert(entry: T.GlossaryEntry, previous_source: string | null = null) {
  return call('glossary_upsert', { entry, previous_source })
}
/** 删除一条术语（按原文；已经不在了也算成功） */
export function glossaryRemove(source: string) {
  return call('glossary_remove', { source })
}
export function glossaryImport() {
  return call('glossary_import', none)
}
export function glossaryExport() {
  return call('glossary_export', none)
}

// ===== 模型服务 =====

export function providerTemplates() {
  return call('provider_templates', none)
}
/** 生成唯一 id，此时尚未保存 */
export function providerNew(template_id: string) {
  return call('provider_new', { template_id })
}
export function providerDelete(provider_id: string) {
  return call('provider_delete', { provider_id })
}
/** 保存一个已有的服务（只替换这一项）；服务已在别处删除时抛 `not_found` */
export function providerSave(provider: T.Provider) {
  return call('provider_save', { provider })
}
/** 添加服务（追加到末尾，其他服务不动） */
export function providerAdd(provider: T.Provider) {
  return call('provider_add', { provider })
}
/** 调整服务的顺序（只重排、不改内容） */
export function providersReorder(ids: string[]) {
  return call('providers_reorder', { ids })
}
/**
 * 草稿也可以测。`key`：草稿里刚输入、还没保存的 Key，只用于这一次请求，**不写入**
 * 钥匙串；null = 按 provider.id 从钥匙串读取（已保存的服务）
 */
export function providerTest(provider: T.Provider, key: string | null = null) {
  return call('provider_test', { provider, key })
}
/** 拉取模型列表；`key` 的含义同 providerTest */
export function providerModels(provider: T.Provider, key: string | null = null) {
  return call('provider_models', { provider, key })
}
export function llmPreviewUrl(protocol: T.Protocol, base_url: string, model_id: string | null) {
  return call('llm_preview_url', { protocol, base_url, model_id })
}
/** 没有读取 Key 的接口 */
export function secretSet(provider_id: string, key: string) {
  return call('secret_set', { provider_id, key })
}
export function secretDelete(provider_id: string) {
  return call('secret_delete', { provider_id })
}
export function modelCapsReset(provider_id: string, model_id: string) {
  return call('model_caps_reset', { provider_id, model_id })
}

// ===== 应用 =====

export function permissionsStatus() {
  return call('permissions_status', none)
}
export function permissionsRequest(kind: T.PermissionKind) {
  return call('permissions_request', { kind })
}
export function permissionsOpen(kind: T.PermissionKind) {
  return call('permissions_open', { kind })
}
/** 引导页可见时每 2 s 轮询一次 */
export function permissionsWatch(active: boolean) {
  return call('permissions_watch', { active })
}
export function appInfo() {
  return call('app_info', none)
}
export function onboardingComplete() {
  return call('onboarding_complete', none)
}
export function appearanceGet() {
  return call('appearance_get', none)
}
export function openPath(kind: T.PathKind) {
  return call('open_path', { kind })
}
export function openUrl(url: string) {
  return call('open_url', { url })
}
/**
 * 重启应用。`resume`：重启后直接打开的窗口与路由（例如引导的权限步骤
 * `{ kind: 'onboarding', step: 1 }`），由 Rust 持久化、下次启动时取走一次（5 分钟内有效）；null = 照常启动
 */
export function appRelaunch(resume: T.WindowTarget | null = null) {
  return call('app_relaunch', { resume })
}
export function appQuit() {
  return call('app_quit', none)
}
export function diagnosticsExport() {
  return call('diagnostics_export', none)
}
export function updateCheck() {
  return call('update_check', none)
}
export function updateInstall() {
  return call('update_install', none)
}
export function updateStatus() {
  return call('update_status', none)
}

// ===== 快捷键 =====

export function hotkeyStatus() {
  return call('hotkey_status', none)
}
/** 录制期间注销全部全局键 */
export function hotkeyRecordBegin() {
  return call('hotkey_record_begin', none)
}
export function hotkeyRecordEnd() {
  return call('hotkey_record_end', none)
}
export function hotkeyCheck(target: T.HotkeyTarget, combo: string) {
  return call('hotkey_check', { target, combo })
}
/** 先注册新键、再注销旧键 */
export function hotkeySet(target: T.HotkeyTarget, binding: T.Binding, force: boolean) {
  return call('hotkey_set', { target, binding, force })
}
export function hotkeyResetDefaults() {
  return call('hotkey_reset_defaults', none)
}
export function hotkeyTestBegin(target: T.HotkeyTarget) {
  return call('hotkey_test_begin', { target })
}
export function hotkeyTestEnd() {
  return call('hotkey_test_end', none)
}
export function neighborApps() {
  return call('neighbor_apps', none)
}
export function keyboardAdvice() {
  return call('keyboard_advice', none)
}

// ===== 同步 =====

export function syncStatus() {
  return call('sync_status', none)
}
/** 参与同步的设备（含设备名）：只在同步页打开时调用 */
export function syncDevices() {
  return call('sync_devices', none)
}
/**
 * 测试连接：登录、读写、服务器上是否已有同步数据。`password`：表单里刚输入的应用密码，只用于这一次请求；
 * null = 用凭据存储里与地址、账号匹配的那一份。传输层面的失败在报告里，不抛错
 */
export function syncProbe(account: T.SyncAccount, password: string | null = null) {
  return call('sync_probe', { account, password })
}
/**
 * 创建 / 加入 / 恢复（由服务器与本机状态决定）。`passphrase` 由调用方做过 NFC 规范化；
 * 同步密码不对时抛 `conflict`（`errors.sync.passphrase`）
 */
export function syncConnect(
  account: T.SyncAccount,
  password: string | null,
  passphrase: string,
  content: T.SyncContent,
) {
  return call('sync_connect', { account, password, passphrase, content })
}
/** 发起（或并入正在进行的）一次同步，立即返回；进度经 `sync:status` 推送 */
export function syncNow() {
  return call('sync_now', none)
}
/** 本机停止同步（服务器上的数据保留，地址与账号留着方便重新连接） */
export function syncDisconnect() {
  return call('sync_disconnect', none)
}
/** 清除服务器上的同步数据（所有设备随之停止同步）。account 为 null 时用已连接的账号与已保存的密码 */
export function syncResetRemote(
  account: T.SyncAccount | null = null,
  password: string | null = null,
) {
  return call('sync_reset_remote', { account, password })
}
export function syncGeneratePassphrase() {
  return call('sync_generate_passphrase', none)
}
