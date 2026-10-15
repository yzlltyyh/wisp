/**
 * IPC 与配置文档的 TypeScript 镜像。
 *
 * 与 `crates/wisp-types` 一一对应：字段名、枚举值统一 snake_case；
 * Rust 的 `Option<T>` 一律序列化为 `null`，所以这里写成 `T | null`（不用可选字段）；
 * 带数据的联合类型用内部标签（`type` / `kind` / `change` / `reason`）。
 *
 * 修改流程：先改 `crates/wisp-types`，再同步本文件，并重新生成黄金样例（`node scripts/gen-fixtures.mjs`）。
 * 防漂移：`__tests__/golden.test.ts` 用 Rust 生成的黄金样例逐个校验键集合。
 */

// ===== 基础 =====
export type MonitorId = string
export type RequestId = string
export interface Point {
  x: number
  y: number
}
export interface Size {
  width: number
  height: number
}
export interface Rect {
  x: number
  y: number
  width: number
  height: number
}
export interface PhysPoint {
  x: number
  y: number
}
export interface PhysSize {
  width: number
  height: number
}
/** 物理像素矩形（u32） */
export interface PhysRect {
  x: number
  y: number
  width: number
  height: number
}
export interface MonitorInfo {
  id: MonitorId
  name: string
  /** 虚拟桌面中的物理像素坐标，可以为负 */
  physical_origin: PhysPoint
  physical_size: PhysSize
  scale_factor: number
  /** 全局逻辑坐标 */
  frame: Rect
  /** 本显示器内的逻辑坐标（已扣除菜单栏、Dock、任务栏） */
  work_area: Rect
  is_primary: boolean
}
/** 显示器 id + 该显示器内的逻辑坐标（跨显示器的位置一律这样表示，不用全局坐标） */
export interface Anchor {
  monitor_id: MonitorId
  rect: Rect
}
export type OsKind = 'macos' | 'windows' | 'linux'
export type SessionKind = 'native' | 'x11' | 'wayland'
export interface AppRef {
  id: string
  alt_id: string | null
  name: string
  pid: number | null
  path: string | null
  window: number | null
}
/** `reason_key` 是 i18n key */
export interface Capability {
  available: boolean
  reason_key: string | null
}
export interface Capabilities {
  global_hotkeys: Capability
  selection: Capability
  simulated_copy: Capability
  paste_inject: Capability
  capture_overlay: Capability
  clipboard_watch: Capability
  input_hooks: Capability
}
export interface PlatformInfo {
  os: OsKind
  os_version: string
  arch: string
  session: SessionKind
  capabilities: Capabilities
}
export type Lang = 'zh-CN' | 'en' | 'ja' | 'ko' | 'other'
export type UiLanguage = 'system' | 'zh-CN' | 'en-US'

// ===== LLM =====
export type Protocol = 'openai_chat' | 'openai_responses' | 'anthropic_messages' | 'gemini'
export type ThinkingOff =
  | 'none'
  | 'deepseek'
  | 'qwen'
  | 'glm'
  | 'kimi'
  | 'minimax'
  | 'openai_effort'
  | 'anthropic_effort'
  | 'gemini_level'
export type MaxTokensField = 'max_completion_tokens' | 'max_tokens'
export type ImageDetail = 'low' | 'high' | 'original' | 'auto'
export interface Quirks {
  thinking_off: ThinkingOff
  merge_system_into_user: boolean
  strip_think_tags: boolean
  max_tokens_field: MaxTokensField | null
  dual_auth_header: boolean | null
  gemini_key_in_query: boolean
}
export type Vision = 'yes' | 'no' | 'unknown'
export type VisionSource = 'capability' | 'runtime' | 'keyword' | 'user'
/** 能力缓存，null 表示未知 */
export interface ModelCaps {
  temperature: boolean | null
  max_tokens_field: MaxTokensField | null
  thinking_param: boolean | null
  effort_param: boolean | null
  stream_options: boolean | null
  include_obfuscation: boolean | null
  anthropic_thinking_disabled: boolean | null
  anthropic_efforts: string[] | null
}
export interface ModelEntry {
  id: string
  label: string | null
  vision: Vision
  vision_source: VisionSource | null
  reasoning: boolean
  enabled: boolean
  missing_upstream: boolean
  deprecated: boolean
  context_tokens: number | null
  caps: ModelCaps
  /** 推理档位；null = 默认 low */
  reasoning_effort: ReasoningEffort | null
}
/** OpenAI 系推理模型的 reasoning_effort */
export type ReasoningEffort = 'none' | 'minimal' | 'low'
export interface Timeouts {
  connect_ms: number
  first_token_ms: number
  idle_ms: number
}
export interface Provider {
  /** 不可变 slug；钥匙串条目为 service="wisp", user=id */
  id: string
  name: string
  protocol: Protocol
  /** 用户输入原样保存（包括末尾的 #） */
  base_url: string
  headers: Record<string, string>
  quirks: Quirks
  models: ModelEntry[]
  timeouts: Timeouts
  template_id: string | null
}
export interface ModelRef {
  provider_id: string
  model_id: string
}
export interface Defaults {
  translate: ModelRef | null
  ocr: ModelRef | null
}
export interface Usage {
  input_tokens: number
  output_tokens: number
  reasoning_tokens: number | null
  total_tokens: number | null
}
export type ErrorCode =
  | 'auth'
  | 'not_found'
  | 'no_vision'
  | 'image_too_large'
  | 'param_unsupported'
  | 'too_long'
  | 'quota'
  | 'rate_limited'
  | 'upstream'
  | 'content_filter'
  | 'network'
  | 'timeout'
  | 'cancelled'
  | 'protocol'
  | 'not_configured'
  | 'keychain'
  | 'internal'
export type ErrorSubcode =
  'region' | 'thinking_exhausted' | 'connection_lost' | 'html_body' | 'version_hint'
export interface LlmError {
  code: ErrorCode
  subcode: ErrorSubcode | null
  /** 服务端原文（≤300 字符）或本地英文简述；界面文案由前端按 code 取 i18n */
  message: string
  retryable: boolean
  /** i18n key，例如 "errors.hint.try_v1" */
  hint: string | null
  status: number | null
  /** 只到 host + path，不含 query */
  url: string | null
  retry_after_s: number | null
  upstream_request_id: string | null
}
export type TruncateReason = 'max_tokens'
export type NoticeKind = 'param_degraded' | 'retried' | 'cache_hit'
export type StreamPhase = 'reviewing' | 'improving'
/** 流事件：Start →（Meta | Phase | Notice | Delta）* → 恰好一个 Done 或 Error */
export type StreamEvent =
  | {
      type: 'start'
      request_id: RequestId
      provider_id: string
      model_id: string
      model_label: string
      upstream_id: string | null
    }
  | {
      type: 'meta'
      detected_source_lang: Lang | null
      target_lang: Lang | null
      dict_mode: boolean | null
      low_confidence: boolean | null
    }
  | { type: 'phase'; phase: StreamPhase }
  | { type: 'delta'; text: string }
  | { type: 'notice'; kind: NoticeKind }
  | {
      type: 'done'
      elapsed_ms: number
      usage: Usage | null
      truncated: TruncateReason | null
      cached: boolean
      no_text: boolean
    }
  | ({ type: 'error' } & LlmError)
export type TemplateGroup = 'openai' | 'anthropic' | 'gemini' | 'other' | 'local'
export interface RegionVariant {
  label_key: string
  base_url: string
}
export interface ProviderTemplate {
  id: string
  name_key: string
  group: TemplateGroup
  provider: Provider
  key_required: boolean
  region_variants: RegionVariant[]
}
export interface UrlPreview {
  method: string
  url: string
  version_appended: boolean
  forced: boolean
  /**
   * 输入需要提醒的地方（i18n key，`settings.providers.url_warning.*`：去掉了末尾的接口路径 /
   * 查询串会附加到请求末尾 / 中间的 `#` 片段被丢弃）；没有时省略
   */
  warning?: string | null
}
export interface ProbeResult {
  ok: boolean
  status: number | null
  elapsed_ms: number
  url: string
  error: LlmError | null
}
/** partial = /models 失败但流式探针成功（标黄） */
export type TestVerdict = 'ok' | 'partial' | 'failed'
export interface ProviderTestReport {
  models: ProbeResult
  stream: ProbeResult
  overall: TestVerdict
  model_used: string | null
  fetched_models: ModelEntry[]
  /**
   * 测试本身之外要告诉用户的情况（i18n key；目前只有
   * `settings.providers.test_notice.keychain_unreadable`——凭据存储读不出来，本次没带 Key 测试）；没有时省略
   */
  notice?: string | null
}

// ===== 风格 / 提示词 =====
export type OutputFormat = 'plain_text' | 'markdown_lite'
export interface Style {
  /** 内置：natural/technical/academic/casual/learn/literal；自定义："custom-<uuid8>" */
  id: string
  /** 内置风格为 ""，前端按 styles.<id> 显示 */
  name: string
  builtin: boolean
  system_template: string
  tail_instruction: string
  temperature: number | null
  output: OutputFormat
  dictionary_mode: boolean
  model_ref: ModelRef | null
  hotkey: string | null
  /** 内置风格是否存在 override */
  modified: boolean
}
/**
 * styles.json 中内置风格的覆写（只在 Rust 侧读写）。语义与 Rust 的双层 Option 一致：
 * **键缺省 = 不覆写**（沿用内置值）；`temperature` / `model_ref` / `hotkey` 显式为 `null` = 覆写为空
 * （例如「不发 temperature」「不用专属模型」「不绑快捷键」）。其余键不接受 null。
 */
export interface StyleOverride {
  system_template?: string
  tail_instruction?: string
  temperature?: number | null
  output?: OutputFormat
  dictionary_mode?: boolean
  model_ref?: ModelRef | null
  hotkey?: string | null
}
export interface StylesFile {
  schema_version: number
  order: string[]
  overrides: Record<string, StyleOverride>
  custom: Style[]
}
export interface GlossaryEntry {
  source: string
  target: string
  note: string | null
}
export interface GlossaryFile {
  schema_version: number
  entries: GlossaryEntry[]
}
export type OcrMode = 'lines' | 'paragraphs' | 'markdown' | 'translate'
export type TaskKind = 'translate' | 'polish' | 'proofread' | 'refine'
export type DictMode = 'auto' | 'dictionary' | 'prose'
export interface PromptPreview {
  system: string
  user: string
}

// ===== 剪贴板 =====
export type ClipKind = 'text' | 'rich_text' | 'image' | 'files' | 'link' | 'color' | 'code'
export type ClipFilter = 'all' | 'text' | 'image' | 'files' | 'link' | 'code' | 'color'
export interface SourceApp {
  id: string
  name: string
  icon_url: string | null
}
export interface ImageMeta {
  width: number
  height: number
  bytes: number
  thumb_url: string | null
}
export interface FileRef {
  path: string
  name: string
  exists: boolean
  size: number | null
}
export interface ClipItemMeta {
  id: number
  kind: ClipKind
  created_at: number
  last_used_at: number
  use_count: number
  pinned: boolean
  source: SourceApp | null
  /** ≤200 字符，空白已压缩 */
  preview: string
  char_count: number | null
  line_count: number | null
  truncated: boolean
  image: ImageMeta | null
  /** 最多带前 5 个 */
  files: FileRef[] | null
  link: string | null
  color: string | null
}
/** preview 中的 UTF-16 偏移 */
export interface HighlightRange {
  start: number
  end: number
}
export interface ClipHit {
  item: ClipItemMeta
  highlights: HighlightRange[]
}
export interface ClipPage {
  items: ClipHit[]
  pinned_count: number
  next_offset: number | null
  query: string | null
  filter: ClipFilter
}
export interface ClipItemDetail {
  meta: ClipItemMeta
  text: string | null
  has_html: boolean
  has_rtf: boolean
  image_url: string | null
  files: FileRef[] | null
}
export type PasteMode = 'normal' | 'plain' | 'copy_only'
/** `manual`：已复制、请手动按粘贴键，不写原因 */
export type CopiedReason =
  | 'copy_only'
  | 'wayland'
  | 'elevated'
  | 'target_gone'
  | 'focus_timeout'
  | 'no_permission'
  | 'manual'
export type PasteOutcome = { type: 'pasted' } | { type: 'copied'; reason: CopiedReason }
export type ClipboardChanged =
  | { change: 'inserted'; item: ClipItemMeta }
  | { change: 'touched'; item: ClipItemMeta }
  /** 已有条目的元数据更新（后台缩略图生成完成：image.thumb_url 由 null 变为 URL）；按 id 原地替换，不改顺序 */
  | { change: 'updated'; item: ClipItemMeta }
  | { change: 'removed'; id: number }
  | { change: 'cleared'; keep_pinned: boolean }
  | { change: 'paused'; paused: boolean }
  | { change: 'secret_skipped' }
/** 平台读到的原始剪贴板内容（platform → clipboard，不跨 IPC；`Vec<u8>` 序列化为数字数组） */
export interface RawClipboard {
  change_id: number
  source: AppRef | null
  concealed: boolean
  from_wisp: boolean
  text: string | null
  text_truncated: boolean
  html: string | null
  rtf: number[] | null
  image_png: number[] | null
  image_size: PhysSize | null
  files: string[] | null
}
/** 写回剪贴板的内容（clipboard → platform，不跨 IPC） */
export interface ClipPayload {
  text: string | null
  html: string | null
  rtf: number[] | null
  image_png: number[] | null
  files: string[] | null
}
export interface OcrHistoryItem {
  id: number
  /** Unix 毫秒 */
  created_at: number
  mode: OcrMode
  /** 模型显示名（记录时的 Start.model_label） */
  model: string
  /** 识别结果全文（translate 模式是译文） */
  text: string
  /** 缩略图资源路径（`ocrimg/<key>`，经 resolveAssetUrl） */
  thumb_url: string | null
  /** 原图资源路径（「回看」放大）；没有保存原图时为 null */
  image_url: string | null
}
/** `ocr_history:changed` → settings：识别历史列表原地增删（有进入 / 退出动画），不整页重拉 */
export type OcrHistoryChanged =
  /** 新记录（插在最前面） */
  | { change: 'inserted'; item: OcrHistoryItem }
  /** 删除单条或按 `ocr.history_limit` 裁剪掉的旧记录 */
  | { change: 'removed'; ids: number[] }
  /** 全部清空 */
  | { change: 'cleared' }

// ===== 快捷键 / 权限 =====
export type HotkeyAction =
  'translate_selection' | 'translate_input' | 'ocr' | 'ocr_translate' | 'clipboard'
/** "default" | "none" | global-hotkey 规范串（修饰键顺序 Control+Alt+Shift+Super，例如 "Alt+KeyD"） */
export type Binding = 'default' | 'none' | (string & {})
export type HotkeyTarget =
  { kind: 'action'; action: HotkeyAction } | { kind: 'style'; style_id: string }
export type ConflictLevel = 'forbidden' | 'warning' | 'info'
export type ConflictSource =
  | 'reserved'
  | 'self_duplicate'
  | 'static_app'
  | 'system_dynamic'
  | 'registration'
  | 'neighbor'
  | 'layout'
  | 'dead_key'
export interface Conflict {
  level: ConflictLevel
  source: ConflictSource
  message_key: string
  params: Record<string, string>
}
export interface ConflictReport {
  combo: string
  display: string
  conflicts: Conflict[]
  acceptable: boolean
}
export type HotkeyState =
  'registered' | 'fallback' | 'failed' | 'unbound' | 'suspended' | 'unavailable'
export interface HotkeyStatus {
  target: HotkeyTarget
  binding: Binding
  effective: string | null
  display: string | null
  state: HotkeyState
  /** i18n key（无参数）：`hotkeys.reason.*`，或 `errors.platform.wayland_hotkeys` / `no_display` */
  reason_key: string | null
}
export type ModifierKey =
  'left_alt' | 'right_alt' | 'left_ctrl' | 'right_ctrl' | 'left_cmd' | 'right_cmd'
export type MouseButtonKind = 'middle' | 'back' | 'forward'
export type ExtTriggerState = 'off' | 'active' | 'waiting_permission' | 'unavailable'
export interface ExtTriggerStatus {
  bobber: ExtTriggerState
  double_tap: ExtTriggerState
  mouse: ExtTriggerState
  /** i18n key（无参数）：`hotkeys.ext.*`，或平台原因 `errors.platform.*` */
  reason_key: string | null
}
export interface NeighborHit {
  app_name: string
  combo: string
  display: string
  action: HotkeyAction
}
export interface KeyboardAdvice {
  has_altgr: boolean
  multi_input_lang: boolean
  /** "alt_shift" */
  recommend_family: string | null
  qq_running: boolean
}
export type PermissionKind = 'accessibility' | 'screen_recording' | 'input_monitoring'
export type PermissionState = 'granted' | 'denied' | 'not_determined' | 'not_applicable'
export interface PermissionSet {
  accessibility: PermissionState
  screen_recording: PermissionState
  input_monitoring: PermissionState
  lost_after_update: boolean
}

// ===== 面板 / 窗口 =====
export type PanelView = 'translate' | 'ocr' | 'clipboard' | 'input' | 'toast'
export type Placement = 'below' | 'above' | 'center'
export type PanelTransition = 'enter' | 'crossfade'
/** 面板帧模式：macOS = native（窗口即卡片），Windows / Linux = css（透明窗口 + CSS 卡片） */
export type FrameMode = 'native' | 'css'
/**
 * `panel_resize` 指定的窗口高度弹簧：只影响 Native 帧的原生逐帧弹簧；缺省 = gentle。
 * snappy：浮层预留引起的长高与释放预留后的回收（与 Css 帧卡片高度的选择同一规则，PanelCard）
 */
export type ResizeSpring = 'gentle' | 'snappy'
export interface LongTextInfo {
  chars: number
  est_tokens: number
  est_seconds: number | null
}
export type NotReady =
  | { reason: 'no_provider' }
  | { reason: 'no_default_model'; task: TaskKind }
  | { reason: 'no_vision_model' }
  | { reason: 'permission_missing'; permission: PermissionKind }
  | { reason: 'wayland_copy_first' }
  | { reason: 'capture_failed'; message: string }
  | { reason: 'elevated' }
  | { reason: 'keychain_unavailable' }
  /** 取词路径不可用：reason_key 是平台层的 i18n key（errors.platform.*），以输入模式打开并显示 */
  | { reason: 'selection_unavailable'; reason_key: string }
export interface TranslatePayload {
  source_text: string
  /** null 表示尚未发请求（长文本确认 / 未就绪） */
  request_id: RequestId | null
  task: TaskKind
  style_id: string
  detected_lang: Lang | null
  target_lang: Lang
  dict_mode: boolean
  long_text: LongTextInfo | null
  not_ready: NotReady | null
  /** 开发者模式显示：ax / uia / primary / copy */
  selection_method: string | null
}
export interface OcrPayload {
  capture_id: string
  request_id: RequestId | null
  mode: OcrMode
  thumb_url: string
  image_size: PhysSize
  target_lang: Lang
  not_ready: NotReady | null
}
export interface ClipboardPayload {
  page: ClipPage
  paused: boolean
  hint_bar: boolean
  paste_on_double_click: boolean
  /**
   * 历史库不可用时的原因（i18n key：`clipboard.history_too_new` 历史库来自更新版本 /
   * `clipboard.history_unavailable` 打不开）；面板在列表顶部显示横幅，空列表时不显示「复制点什么」的空状态。可用时省略
   */
  unavailable?: string | null
  /** 监听状态；正在记录时省略（= `{ state: 'recording' }`），`paused` 与它同步 */
  recording?: ClipboardRecording
}
/**
 * 剪贴板监听状态（ClipboardPayload.recording）：paused = 托盘「暂停剪贴板监听」；disabled = 设置里关闭了监听；
 * unavailable = 监听装不上（`reason_key` 是 `errors.platform.*`：X11 缺 XFIXES、Wayland 没有 data-control 等）
 */
export type ClipboardRecording =
  | { state: 'recording' }
  | { state: 'paused' }
  | { state: 'disabled' }
  | { state: 'unavailable'; reason_key: string }
export interface InputPayload {
  initial_text: string
  target_lang: Lang | null
  hint: NotReady | null
}
/** paste_key：当前平台 / 目标应用实际的粘贴键显示串，如 "⌘V"、"Ctrl+Shift+V" */
export interface ToastPayload {
  reason: CopiedReason
  paste_key: string
}
export type PanelPayload =
  | ({ kind: 'translate' } & TranslatePayload)
  | ({ kind: 'ocr' } & OcrPayload)
  | ({ kind: 'clipboard' } & ClipboardPayload)
  | ({ kind: 'input' } & InputPayload)
  | ({ kind: 'toast' } & ToastPayload)
export interface PanelPrepare {
  session_id: number
  view: PanelView
  payload: PanelPayload
  width: number
  max_height: number
  placement: Placement
  /** 锚点 x 相对卡片左边的偏移（逻辑 px），CSS 帧模式的 transform-origin 用 */
  origin_x: number
  /** 显示后是否持有键盘焦点（输入 / 剪贴板视图为 true） */
  focused: boolean
  transition: PanelTransition
}
export type HideReason =
  | 'escape'
  | 'close_button'
  | 'command_w'
  | 'click_outside'
  | 'mouse_leave'
  | 'blur'
  | 'toggle'
  | 'paste'
  | 'pin'
  | 'retrigger'
  | 'capture'
  | 'other'
export interface PanelShown {
  session_id: number
  focused: boolean
}
export interface PanelHideRequest {
  session_id: number
  reason: HideReason
}
export interface PanelFocusChanged {
  session_id: number
  focused: boolean
}
export interface LatencyMarks {
  session_id: number
  origin: string
  method: string | null
  selection_ms: number | null
  prepare_ms: number
  ready_ms: number
  /** 到前端第一个 opacity > 0 帧 */
  shown_ms: number
  first_token_ms: number | null
  ready_timeout: boolean
}
export interface TranslateReq {
  session_id: number
  text: string
  task: TaskKind
  style_id: string
  /** null = 按自动方向 */
  target_lang: Lang | null
  model_ref: ModelRef | null
  dict_mode: DictMode
  /** 重新生成（⌘R）时为 true */
  bypass_cache: boolean
  /** task = refine 时传入当前译文 */
  draft: string | null
  /**
   * 用户已确认长文本。缺省 / false 时，translate / polish /
   * proofread 且超过阈值的请求不会发出，`StartAck.long_text` 带回估算；已经确认过的原文一律传 true
   */
  confirm_long?: boolean
}
export interface OcrReq {
  session_id: number
  capture_id: string
  mode: OcrMode
  model_ref: ModelRef | null
  target_lang: Lang | null
}
export interface StartAck {
  /** 已登记的请求；`long_text` 非 null 时请求没有发出，为空串 */
  request_id: RequestId
  /** 宽度变化（例如输入 → 翻译）时返回新宽度 */
  width: number | null
  /**
   * 原文超过长文本阈值且 `confirm_long` 为 false：请求**没有发出**，这是确认卡片的估算。
   * Rust 总是带上这个字段；mock 与旧代码可以省略（视为 null）
   */
  long_text?: LongTextInfo | null
}
export type PinKind = 'translate' | 'ocr'
export interface PinState {
  kind: PinKind
  source_text: string
  result_text: string
  task: TaskKind
  style_id: string
  target_lang: Lang
  detected_lang: Lang | null
  /**
   * 要转给固定窗口的请求。流已结束也可以填：OCR 视图据此把截图一起转给 pinned；
   * 翻译视图只在流式进行中填
   */
  request_id: RequestId | null
  capture_id: string | null
  ocr_text: string | null
  ocr_mode: OcrMode | null
  show_source: boolean
  status_line: string | null
  // ---- 以下字段在 Rust 侧 `skip_serializing_if`，缺省 = 默认值。
  // 前 5 个由面板视图在 `panel_pin` 时填写；后 2 个由 Rust 在 `pinned_init` 返回时填写（panel_pin 传入的值被忽略）
  /** 词典模式（固定窗口里换风格 / 重新生成时沿用）；缺省 = false */
  dict_mode?: boolean
  /** 本次结果实际使用的模型；缺省 / null = 按默认模型 */
  model_ref?: ModelRef | null
  /** 结构化状态行（代替解析 status_line；两者都给时以它为准） */
  status?: PinStatus | null
  /** OCR：截图缩略图的资源路径（= OcrPayload.thumb_url，经 resolveAssetUrl） */
  thumb_url?: string | null
  /** OCR：原图尺寸（物理像素；图片加载前按宽高比占位） */
  image_size?: PhysSize | null
  /** Rust 填写：「展开」⤢ 时首帧测量用的卡片宽度（逻辑 px，已按工作区收窄）；普通固定为缺省 / null */
  expand_width?: number | null
  /** Rust 填写：窗口当前是否置顶；缺省 / null = 旧 Rust（退回前端自己的初值） */
  on_top?: boolean | null
}
/** 固定窗口的结构化状态行（`PinState.status`）：与面板 StatusLine 的各段一一对应 */
export interface PinStatus {
  /** 模型显示名（Start.model_label） */
  model_label: string
  /** 请求耗时；命中缓存时为 null（不显示耗时） */
  elapsed_ms: number | null
  usage: Usage | null
  /** 结果来自缓存 */
  cached: boolean
  /** 已取消（只显示「已取消」） */
  cancelled: boolean
  truncated: TruncateReason | null
}
/**
 * 可从前端打开的窗口；同时是 settings / onboarding 的路由（`Boot.route`、`window:navigate`）。
 * `anchor`：分组内的锚点，只允许 `sections.ts` 的 `SETTINGS_ANCHORS` 里登记的组合
 * （Rust 拒绝未登记的组合）；缺省 / null = 分组顶部
 */
export type WindowTarget =
  | { kind: 'settings'; section: string | null; anchor?: string | null }
  | { kind: 'onboarding'; step: number | null }
export type OverlayMode = 'ocr' | 'ocr_translate'
export type OverlaySelectionKind = 'drag' | 'window' | 'fullscreen'
/**
 * `overlay_init()` 的返回值。命令没有参数：Rust 按调用方窗口的 label 反查本次会话绑定的显示器；
 * 窗口当前没有会话（预建、空闲中）时返回 null，页面等待 `overlay:session` 后再调用一次。
 */
export interface OverlayInit {
  session_id: number
  monitor: MonitorInfo
  image_url: string
  image_size: PhysSize
  /** 本显示器内的窗口矩形（逻辑坐标，按 z 序，最上层在前） */
  windows: Rect[]
  mode: OverlayMode
  window_detect: boolean
  fly_animation: boolean
  reduced_motion: boolean
  /** true（macOS）：冻结帧已由原生 CALayer 显示，前端不画冻结帧，image_url 只给放大镜用 */
  frozen_native: boolean
  /** Windows / X11：先画的 1/2 降采样帧；macOS 为 null */
  half_url: string | null
  /**
   * `frozen_native` 时放大镜与飞入按需拉取的小块：资源路径前缀 `capture/<s>/<m-hex>/region`，
   * 页面接上 `/<x>/<y>/<w>/<h>`（物理像素，必须在帧内；要更小的输出时再接 `/<max_px>`，1..=4 MP 的面积上限，放大镜的
   * 低分辨率底图用）再交给 `resolveAssetUrl`；响应是带尺寸头的原始 RGBA（大块降采样）。
   * 不是原生冻结帧时缺省（Rust 序列化时省略）
   */
  region_url?: string | null
}
/**
 * `overlay:session`：复用预建的 Overlay 开始新会话（emit_to 该窗口）。页面重置后重新 `overlayInit()`。
 * 防竞态：先订阅本事件、再调用 `overlayInit()`；init 为 null 时等事件；按 session_id 去重。
 */
export interface OverlaySessionStart {
  session_id: number
  monitor_id: MonitorId
}
/**
 * `overlay:end` 的原因：done = commit 之后正常拆除；cancelled = 取消（页面里的 Esc / 右键、
 * 全局 Esc、另一块屏的页面取消、托盘 / CLI 打断）；retrigger = 会话期间再按截图快捷键；failed = 页面没能初始化 / 没有一块屏显示出来
 */
export type OverlayEndReason = 'done' | 'cancelled' | 'retrigger' | 'failed'
/**
 * `overlay:end`：Rust 结束截图会话、拆除 Overlay **之前** emit_to 绑定该会话的每个 overlay-<n>。页面按 session_id
 * 核对后立即收尾（结束交互、释放冻结帧缓冲区与位图），**不再回调** overlayCancel / overlayDone；重复收尾必须无害
 */
export interface OverlaySessionEnd {
  session_id: number
  reason: OverlayEndReason
}
export interface OverlayCommit {
  session_id: number
  monitor_id: MonitorId
  rect: Rect
  kind: OverlaySelectionKind
}
/** 面板的目标矩形（本显示器内逻辑坐标）；null 表示不飞入 */
export interface OverlayCommitAck {
  fly_to: Rect | null
}

// ===== 应用 =====
export type KeychainState = 'available' | 'session_only' | 'unavailable'
export type UpdateState =
  'idle' | 'checking' | 'up_to_date' | 'available' | 'downloading' | 'ready' | 'error' | 'disabled'
export interface UpdateStatus {
  state: UpdateState
  version: string | null
  notes: string | null
  progress: number | null
  error: string | null
}
export interface RuntimeInfo {
  version: string
  platform: PlatformInfo
  has_key: Record<string, boolean>
  keychain: KeychainState
  hotkeys: HotkeyStatus[]
  ext_triggers: ExtTriggerStatus
  permissions: PermissionSet
  clipboard_paused: boolean
  update: UpdateStatus
  glossary_count: number
  /** 配置文件最近一次读取失败的情况；设置窗口据此提示 `settings.invalid_file` */
  settings_issue: SettingsFileIssue | null
  /** 已删除的 Provider 中、Key 还没能从系统凭据存储删掉的个数（之后的钥匙串操作时重试清理） */
  orphan_keys: number
  /**
   * 「开机自启」开着、却没能写进系统时的原因（i18n key：
   * `settings.general.autostart_issue.translocated` / `failed`）；没有问题时省略
   */
  autostart_issue?: string | null
}
/**
 * invalid_json：热加载时 JSON 非法，保留旧值；reset_to_default：启动时损坏，已备份并改用默认值；
 * newer_version：文件由更新版本的 Wisp 写出，不认识的设置不生效但保存时保留
 */
export type SettingsIssueKind = 'invalid_json' | 'reset_to_default' | 'newer_version'
export interface SettingsFileIssue {
  kind: SettingsIssueKind
  /** 例如 "settings.json" */
  file: string
  /** reset_to_default 时的备份完整路径 */
  backup_path: string | null
  /** Unix 毫秒 */
  at_ms: number
}
export interface SettingsSnapshot {
  settings: Settings
  styles: Style[]
  runtime: RuntimeInfo
}
export type ResolvedTheme = 'light' | 'dark'
export type ResolvedMaterial = 'native' | 'solid'
export interface AppearanceState {
  theme: ResolvedTheme
  /** "#RRGGBB"；空串表示沿用 tokens.css 的默认强调色（与 Boot.accent 一致） */
  accent: string
  reduce_motion: boolean
  reduce_transparency: boolean
  material: ResolvedMaterial
  font_size: number
  os: OsKind
  ui_language: 'zh-CN' | 'en-US'
}
export interface AppMeta {
  version: string
  platform: PlatformInfo
  config_dir: string
  data_dir: string
  log_dir: string
  dev_mode: boolean
}
export type PathKind = 'config' | 'data' | 'log'
export type SendTarget = 'translate' | 'ocr'
/**
 * `trigger_action` 的来源：引导页「试一试」/ 剪贴板条目送翻译 / 面板里的按钮（截图失败卡片的「重新截图」）/
 * 设置 › OCR › 识别历史的「重新翻译」
 */
export type ActionOrigin = 'onboarding' | 'clipboard' | 'panel' | 'ocr_history'
export interface OnboardingHotkey {
  action: HotkeyAction
}
export interface HotkeyTestReceived {
  target: HotkeyTarget
}
export type IpcErrorCode =
  | 'invalid_argument'
  | 'not_found'
  | 'not_ready'
  | 'permission'
  | 'platform'
  | 'keychain'
  | 'io'
  | 'llm'
  | 'conflict'
  | 'not_implemented'
  | 'internal'
/** 所有命令的错误类型；前端封装把它作为 `WispIpcError` 抛出 */
export interface IpcError {
  code: IpcErrorCode
  message: string
  llm: LlmError | null
}

// ===== settings.json（wisp-types/settings.rs）=====
export type UpdatePolicy = 'notify' | 'auto_install' | 'off'
export interface General {
  /** 默认 true：开机自启 */
  launch_at_login: boolean
  /** 默认 true：静默启动到托盘 */
  start_hidden: boolean
  /** 默认 "system" */
  ui_language: UiLanguage
  /** 默认 "notify" */
  update_policy: UpdatePolicy
  /** 默认 false：面板底部显示延迟埋点 */
  developer_mode: boolean
}
export interface BobberSettings {
  enabled: boolean
}
export interface DoubleTapSettings {
  enabled: boolean
  /** null = 平台默认（mac right_alt，win / linux right_ctrl） */
  key: ModifierKey | null
  /** 只允许 200 / 300 / 400 */
  interval_ms: number
}
export interface MouseBinding {
  enabled: boolean
  action: HotkeyAction | null
}
export interface MouseSettings {
  enabled: boolean
  long_press: boolean
  middle: MouseBinding
  back: MouseBinding
  forward: MouseBinding
}
export interface AppKeymap {
  os: OsKind
  app_id: string
  /** "none" = 不发复制键 */
  copy: string | null
  paste: string | null
}
export interface HotkeySettings {
  /** 缺失的动作视为 "default" */
  bindings: Partial<Record<HotkeyAction, Binding>>
  bobber: BobberSettings
  double_tap: DoubleTapSettings
  mouse: MouseSettings
  app_keymaps: AppKeymap[]
  /** 默认 150（50–500） */
  copy_wait_ms: number
}
export type TranslateAnchor = 'selection' | 'cursor'
export type InputAnchor = 'cursor' | 'center'
export interface TranslateSettings {
  primary_lang: Lang
  secondary_lang: Lang
  default_style: string
  /** 默认 3000（500–20000） */
  long_text_threshold: number
  close_on_click_outside: boolean
  close_on_mouse_leave: boolean
  /** 默认 160（80–400） */
  mouse_leave_px: number
  anchor: TranslateAnchor
  input_anchor: InputAnchor
  cache_enabled: boolean
  show_source_by_default: boolean
}
export interface OcrSettings {
  /** 只允许 lines / paragraphs / markdown */
  default_mode: OcrMode
  /** 默认 2048（512–4096） */
  max_edge_px: number
  auto_copy: boolean
  history: boolean
  /** 默认 50（10–500） */
  history_limit: number
  window_detect: boolean
  fly_animation: boolean
  direct_translate_transcribe: boolean
  image_detail: ImageDetail
}
export type ClipPanelPosition = 'center' | 'cursor' | 'remember'
export type PasteOn = 'single_click' | 'double_click'
export interface ExcludedApp {
  os: OsKind
  app_id: string
  name: string
}
export interface ClipboardSettings {
  enabled: boolean
  record_text: boolean
  record_rich_text: boolean
  record_images: boolean
  record_files: boolean
  /** 默认 1000（不含置顶） */
  max_items: number
  /** null（不限）| 7 | 30 | 90 */
  max_age_days: number | null
  max_text_bytes: number
  max_image_bytes: number
  excluded_apps: ExcludedApp[]
  secret_detection: boolean
  secret_tray_flash: boolean
  panel_position: ClipPanelPosition
  paste_on: PasteOn
  show_hint_bar: boolean
}
export type ThemePref = 'system' | 'light' | 'dark'
export type MaterialPref = 'system' | 'solid'
export type ReduceMotionPref = 'system' | 'on' | 'off'
export interface Appearance {
  theme: ThemePref
  /** "system" | 预设名（blue/purple/pink/red/orange/yellow/green/graphite）| "#RRGGBB" */
  accent: string
  /** 默认 14（12–18） */
  font_size: number
  /** 默认 360（320–480） */
  translate_min_width: number
  /** 默认 520（440–720） */
  translate_max_width: number
  /** 默认 420（360–560） */
  clipboard_width: number
  /** 默认 0.6（0.4–0.8） */
  max_height_ratio: number
  material: MaterialPref
  reduce_motion: ReduceMotionPref
}
export type LogLevel = 'error' | 'warn' | 'info' | 'debug'
export interface Advanced {
  log_level: LogLevel
  webview_low_memory_when_hidden: boolean
  /** null（关）| 30–1440 */
  unload_panel_after_idle_min: number | null
}
export interface Settings {
  schema_version: number
  general: General
  hotkeys: HotkeySettings
  providers: Provider[]
  defaults: Defaults
  translate: TranslateSettings
  ocr: OcrSettings
  clipboard: ClipboardSettings
  appearance: Appearance
  advanced: Advanced
}

// ===== Boot（window.__WISP_BOOT__；Rust 侧为 wisp_types::Boot）=====
export interface Boot {
  /** 窗口 label：panel / settings / onboarding / pinned-<id> / overlay-<n> */
  label: string
  os: OsKind
  os_version: string
  theme: ResolvedTheme
  /** "#RRGGBB"；空串表示沿用 tokens.css 的默认强调色 */
  accent: string
  reduce_motion: boolean
  reduce_transparency: boolean
  material: ResolvedMaterial
  /** macOS = native（原生帧不可用时为 css），其他平台 = css */
  frame_mode: FrameMode
  /**
   * Css 帧模式下卡片四周留给阴影的透明边距（逻辑 px；native 为 0）。唯一来源是 Rust（windows::sizing::SHADOW_MARGIN），
   * 同时写到 CSS 变量 `--shadow-margin`；前端不得另写一份
   */
  shadow_margin: number
  font_size: number
  ui_language: 'zh-CN' | 'en-US'
  /**
   * 是否显示开发者信息（延迟埋点、FrameMonitor）= `settings.general.developer_mode || debug_build`。
   * 创建窗口时的值；之后在 `settings:changed` 时由 `applyDeveloperMode` 原地更新
   */
  dev: boolean
  /** debug 构建 */
  debug_build: boolean
  /**
   * 初始路由：settings / onboarding 首次创建时的目标分组 / 引导步骤；其他窗口为 null。
   * 窗口已打开时的跳转走 `window:navigate` 事件
   */
  route: WindowTarget | null
  /**
   * 面板冷唤出时随初始化脚本送进新页面的那次 prepare：面板入口在第一次渲染时当作第一条
   * `panel:prepare` 处理。其他窗口与预热创建的面板没有这个键（Rust 序列化时省略）
   */
  prepare?: PanelPrepare | null
}

// ===== 事件表（events.ts 用它做类型推导）=====
export interface EventMap {
  'panel:prepare': PanelPrepare
  'panel:hide_request': PanelHideRequest
  'panel:focus_changed': PanelFocusChanged
  'panel:latency': LatencyMarks
  'clipboard:changed': ClipboardChanged
  'settings:changed': SettingsSnapshot
  'appearance:changed': AppearanceState
  'permissions:changed': PermissionSet
  'hotkeys:changed': HotkeyStatus[]
  'hotkey:test_received': HotkeyTestReceived
  'update:status': UpdateStatus
  'onboarding:hotkey': OnboardingHotkey
  /** 复用的预建 Overlay 开始新会话 → overlay-<n> */
  'overlay:session': OverlaySessionStart
  /** 已打开的 settings / onboarding 深链跳转（分组 / 引导步骤） */
  'window:navigate': WindowTarget
  /**
   * settings / onboarding 的原生关闭按钮被点了：提交延迟写入、等在途命令返回之后自己 `window.close()`；1.5 s 内没关
   * 由 Rust 照样关闭
   */
  'window:close_requested': null
  /** Rust 结束截图会话、拆除 Overlay 之前 → 绑定该会话的每个 overlay-<n>（页面收尾，不回调） */
  'overlay:end': OverlaySessionEnd
  /** OCR 历史新增 / 删除 / 裁剪 / 清空 → settings */
  'ocr_history:changed': OcrHistoryChanged
}
export type EventName = keyof EventMap
