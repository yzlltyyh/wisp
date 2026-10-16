/**
 * 与 types.ts 配套的手写运行时形状表。
 *
 * 每个对象 / 联合类型都用 `obj<T>()` / `union<T, Tag>()` 定义：键集合在**编译期**与 TS 类型逐一对齐
 * （少一个键是类型错误，多一个键是对象字面量的多余属性错误），在**运行期**用来校验 Rust 生成的黄金样例
 * 的键集合完全一致（既不能多也不能少）。枚举用 `enumOf<T>()({ a: 1, b: 1 })`，同样在编译期穷尽。
 */
import type * as T from '../types'

export type Spec =
  | { readonly t: 'string' }
  | { readonly t: 'number' }
  | { readonly t: 'bool' }
  | { readonly t: 'unknown' }
  | { readonly t: 'enum'; readonly name: string; readonly values: ReadonlySet<string> }
  | { readonly t: 'nullable'; readonly of: Spec }
  /** 可缺省的键（TS 的 `key?:`；只用于 Rust `skip_serializing_if` 的字段，例如 StyleOverride） */
  | { readonly t: 'optional'; readonly of: Spec }
  | { readonly t: 'array'; readonly of: Spec }
  | { readonly t: 'record'; readonly of: Spec; readonly keys?: Spec }
  | { readonly t: 'object'; readonly name: string; readonly fields: Readonly<Record<string, Spec>> }
  | {
      readonly t: 'union'
      readonly name: string
      readonly tag: string
      readonly variants: Readonly<Record<string, Readonly<Record<string, Spec>>>>
    }
  | { readonly t: 'ref'; readonly name: string; readonly get: () => Spec }

const str: Spec = { t: 'string' }
const num: Spec = { t: 'number' }
const bool: Spec = { t: 'bool' }
const nul = (of: Spec): Spec => ({ t: 'nullable', of })
const opt = (of: Spec): Spec => ({ t: 'optional', of })
const arr = (of: Spec): Spec => ({ t: 'array', of })
const rec = (of: Spec, keys?: Spec): Spec =>
  keys ? { t: 'record', of, keys } : { t: 'record', of }
/** 延迟引用（类型之间互相引用时避免初始化顺序问题）；名字拼错会在第一次校验时抛错 */
const ref = (name: string): Spec => ({
  t: 'ref',
  name,
  get: () => {
    const spec = (registry as Record<string, Spec>)[name]
    if (!spec) throw new Error(`shapes: 未知的类型引用 ${name}`)
    return spec
  },
})

type Fields<O> = { readonly [K in keyof O]-?: Spec }

function obj<O>() {
  return (name: string, fields: Fields<O>): Spec => ({ t: 'object', name, fields })
}

type Variants<U, Tag extends keyof U> = {
  readonly [V in U[Tag] & string]: Fields<Omit<Extract<U, Record<Tag, V>>, Tag>>
}

function union<U, Tag extends keyof U & string>() {
  return (name: string, tag: Tag, variants: Variants<U, Tag>): Spec => ({
    t: 'union',
    name,
    tag,
    variants: variants as Record<string, Record<string, Spec>>,
  })
}

function enumOf<E extends string>() {
  return (name: string, values: { readonly [K in E]: 1 }): Spec => ({
    t: 'enum',
    name,
    values: new Set(Object.keys(values)),
  })
}

// ===== 枚举 =====
const OsKind = enumOf<T.OsKind>()('OsKind', { macos: 1, windows: 1, linux: 1 })
const SessionKind = enumOf<T.SessionKind>()('SessionKind', { native: 1, x11: 1, wayland: 1 })
const Lang = enumOf<T.Lang>()('Lang', { 'zh-CN': 1, en: 1, ja: 1, ko: 1, other: 1 })
const UiLanguage = enumOf<T.UiLanguage>()('UiLanguage', { system: 1, 'zh-CN': 1, 'en-US': 1 })
const Protocol = enumOf<T.Protocol>()('Protocol', {
  openai_chat: 1,
  openai_responses: 1,
  anthropic_messages: 1,
  gemini: 1,
  paddle_ocr: 1,
})
const ThinkingOff = enumOf<T.ThinkingOff>()('ThinkingOff', {
  none: 1,
  deepseek: 1,
  qwen: 1,
  glm: 1,
  kimi: 1,
  minimax: 1,
  openai_effort: 1,
  anthropic_effort: 1,
  gemini_level: 1,
  chat_template_kwargs: 1,
})
const MaxTokensField = enumOf<T.MaxTokensField>()('MaxTokensField', {
  max_completion_tokens: 1,
  max_tokens: 1,
})
const ReasoningEffort = enumOf<T.ReasoningEffort>()('ReasoningEffort', {
  none: 1,
  minimal: 1,
  low: 1,
})
const ImageDetail = enumOf<T.ImageDetail>()('ImageDetail', {
  low: 1,
  high: 1,
  original: 1,
  auto: 1,
})
const Vision = enumOf<T.Vision>()('Vision', { yes: 1, no: 1, unknown: 1 })
const VisionSource = enumOf<T.VisionSource>()('VisionSource', {
  capability: 1,
  runtime: 1,
  keyword: 1,
  user: 1,
})
const ErrorCode = enumOf<T.ErrorCode>()('ErrorCode', {
  auth: 1,
  not_found: 1,
  no_vision: 1,
  image_too_large: 1,
  param_unsupported: 1,
  too_long: 1,
  quota: 1,
  rate_limited: 1,
  upstream: 1,
  content_filter: 1,
  network: 1,
  timeout: 1,
  cancelled: 1,
  protocol: 1,
  not_configured: 1,
  keychain: 1,
  internal: 1,
  local_runtime: 1,
})
const ErrorSubcode = enumOf<T.ErrorSubcode>()('ErrorSubcode', {
  region: 1,
  thinking_exhausted: 1,
  connection_lost: 1,
  html_body: 1,
  version_hint: 1,
})
const TruncateReason = enumOf<T.TruncateReason>()('TruncateReason', { max_tokens: 1 })
const NoticeKind = enumOf<T.NoticeKind>()('NoticeKind', {
  param_degraded: 1,
  retried: 1,
  cache_hit: 1,
  partial: 1,
})
const StreamPhase = enumOf<T.StreamPhase>()('StreamPhase', {
  reviewing: 1,
  improving: 1,
  loading_model: 1,
  queued: 1,
})
const TemplateGroup = enumOf<T.TemplateGroup>()('TemplateGroup', {
  openai: 1,
  anthropic: 1,
  gemini: 1,
  other: 1,
  local: 1,
  ocr: 1,
})
const TestVerdict = enumOf<T.TestVerdict>()('TestVerdict', { ok: 1, partial: 1, failed: 1 })
const OutputFormat = enumOf<T.OutputFormat>()('OutputFormat', { plain_text: 1, markdown_lite: 1 })
const OcrMode = enumOf<T.OcrMode>()('OcrMode', {
  lines: 1,
  paragraphs: 1,
  markdown: 1,
  translate: 1,
})
const TaskKind = enumOf<T.TaskKind>()('TaskKind', {
  translate: 1,
  polish: 1,
  proofread: 1,
  refine: 1,
})
const DictMode = enumOf<T.DictMode>()('DictMode', { auto: 1, dictionary: 1, prose: 1 })
const LocalModelId = enumOf<T.LocalModelId>()('LocalModelId', {
  pp_ocrv6_tiny: 1,
  pp_ocrv6_small: 1,
  paddleocr_vl_1_6: 1,
  index_translate_2b: 1,
  index_translate_9b: 1,
})
const LocalModelKind = enumOf<T.LocalModelKind>()('LocalModelKind', { ocr: 1, translate: 1 })
const LocalRuntimeId = enumOf<T.LocalRuntimeId>()('LocalRuntimeId', {
  llama_cpp: 1,
  onnx_runtime: 1,
})
const DownloadSource = enumOf<T.DownloadSource>()('DownloadSource', {
  auto: 1,
  model_scope: 1,
  hugging_face: 1,
})
const DownloadErrorCode = enumOf<T.DownloadErrorCode>()('DownloadErrorCode', {
  network: 1,
  server: 1,
  checksum_mismatch: 1,
  disk_full: 1,
  io: 1,
  extract: 1,
})
const LoadState = enumOf<T.LoadState>()('LoadState', { unloaded: 1, loading: 1, loaded: 1 })
const FitnessLevel = enumOf<T.FitnessLevel>()('FitnessLevel', {
  good: 1,
  slow: 1,
  insufficient: 1,
  unknown: 1,
})
const ClipKind = enumOf<T.ClipKind>()('ClipKind', {
  text: 1,
  rich_text: 1,
  image: 1,
  files: 1,
  link: 1,
  color: 1,
  code: 1,
})
const ClipFilter = enumOf<T.ClipFilter>()('ClipFilter', {
  all: 1,
  favorite: 1,
  text: 1,
  image: 1,
  files: 1,
  link: 1,
  code: 1,
  color: 1,
})
const EntityKind = enumOf<T.EntityKind>()('EntityKind', {
  email: 1,
  phone: 1,
  url: 1,
  ip: 1,
  otp: 1,
  tracking: 1,
  order: 1,
  amount: 1,
  datetime: 1,
  id_card: 1,
  bank_card: 1,
})
const TransformId = enumOf<T.TransformId>()('TransformId', {
  join_lines: 1,
  trim: 1,
  upper: 1,
  lower: 1,
  title: 1,
  half_width: 1,
  full_width: 1,
  json_pretty: 1,
  json_minify: 1,
  strip_tracking: 1,
})
const PasteMode = enumOf<T.PasteMode>()('PasteMode', { normal: 1, plain: 1, copy_only: 1 })
const CopiedReason = enumOf<T.CopiedReason>()('CopiedReason', {
  copy_only: 1,
  wayland: 1,
  elevated: 1,
  target_gone: 1,
  focus_timeout: 1,
  no_permission: 1,
  manual: 1,
})
const HotkeyAction = enumOf<T.HotkeyAction>()('HotkeyAction', {
  translate_selection: 1,
  translate_input: 1,
  ocr: 1,
  ocr_translate: 1,
  clipboard: 1,
})
const ConflictLevel = enumOf<T.ConflictLevel>()('ConflictLevel', {
  forbidden: 1,
  warning: 1,
  info: 1,
})
const ConflictSource = enumOf<T.ConflictSource>()('ConflictSource', {
  reserved: 1,
  self_duplicate: 1,
  static_app: 1,
  system_dynamic: 1,
  registration: 1,
  neighbor: 1,
  layout: 1,
  dead_key: 1,
})
const HotkeyState = enumOf<T.HotkeyState>()('HotkeyState', {
  registered: 1,
  fallback: 1,
  failed: 1,
  unbound: 1,
  suspended: 1,
  unavailable: 1,
})
const ModifierKey = enumOf<T.ModifierKey>()('ModifierKey', {
  left_alt: 1,
  right_alt: 1,
  left_ctrl: 1,
  right_ctrl: 1,
  left_cmd: 1,
  right_cmd: 1,
})
const MouseButtonKind = enumOf<T.MouseButtonKind>()('MouseButtonKind', {
  middle: 1,
  back: 1,
  forward: 1,
})
const ExtTriggerState = enumOf<T.ExtTriggerState>()('ExtTriggerState', {
  off: 1,
  active: 1,
  waiting_permission: 1,
  unavailable: 1,
})
const PermissionKind = enumOf<T.PermissionKind>()('PermissionKind', {
  accessibility: 1,
  screen_recording: 1,
  input_monitoring: 1,
})
const PermissionState = enumOf<T.PermissionState>()('PermissionState', {
  granted: 1,
  denied: 1,
  not_determined: 1,
  not_applicable: 1,
})
const PanelView = enumOf<T.PanelView>()('PanelView', {
  translate: 1,
  ocr: 1,
  clipboard: 1,
  input: 1,
  toast: 1,
})
const Placement = enumOf<T.Placement>()('Placement', { below: 1, above: 1, center: 1 })
const PanelTransition = enumOf<T.PanelTransition>()('PanelTransition', { enter: 1, crossfade: 1 })
const HideReason = enumOf<T.HideReason>()('HideReason', {
  escape: 1,
  close_button: 1,
  command_w: 1,
  click_outside: 1,
  mouse_leave: 1,
  blur: 1,
  toggle: 1,
  paste: 1,
  pin: 1,
  retrigger: 1,
  capture: 1,
  other: 1,
})
const PinKind = enumOf<T.PinKind>()('PinKind', { translate: 1, ocr: 1 })
const OverlayMode = enumOf<T.OverlayMode>()('OverlayMode', { ocr: 1, ocr_translate: 1 })
const OverlaySelectionKind = enumOf<T.OverlaySelectionKind>()('OverlaySelectionKind', {
  drag: 1,
  window: 1,
  fullscreen: 1,
})
const KeychainState = enumOf<T.KeychainState>()('KeychainState', {
  available: 1,
  session_only: 1,
  unavailable: 1,
})
const ResolvedTheme = enumOf<T.ResolvedTheme>()('ResolvedTheme', { light: 1, dark: 1 })
const ResolvedMaterial = enumOf<T.ResolvedMaterial>()('ResolvedMaterial', { native: 1, solid: 1 })
const PathKind = enumOf<T.PathKind>()('PathKind', { config: 1, data: 1, log: 1 })
const SyncPhase = enumOf<T.SyncPhase>()('SyncPhase', { off: 1, idle: 1, syncing: 1, error: 1 })
const SyncErrorKind = enumOf<T.SyncErrorKind>()('SyncErrorKind', {
  network: 1,
  auth: 1,
  forbidden: 1,
  not_found: 1,
  quota: 1,
  server: 1,
  https_required: 1,
  passphrase: 1,
  vault_reset: 1,
  format_too_new: 1,
  credentials_missing: 1,
  keychain: 1,
  corrupt: 1,
  internal: 1,
})
const SyncConnectOutcome = enumOf<T.SyncConnectOutcome>()('SyncConnectOutcome', {
  created: 1,
  joined: 1,
  restored: 1,
})
const UpdateState = enumOf<T.UpdateState>()('UpdateState', {
  idle: 1,
  checking: 1,
  up_to_date: 1,
  available: 1,
  downloading: 1,
  ready: 1,
  error: 1,
  disabled: 1,
})
const SendTarget = enumOf<T.SendTarget>()('SendTarget', { translate: 1, ocr: 1 })
const ActionOrigin = enumOf<T.ActionOrigin>()('ActionOrigin', {
  onboarding: 1,
  clipboard: 1,
  panel: 1,
  ocr_history: 1,
})
const OverlayEndReason = enumOf<T.OverlayEndReason>()('OverlayEndReason', {
  done: 1,
  cancelled: 1,
  retrigger: 1,
  failed: 1,
})
const FrameMode = enumOf<T.FrameMode>()('FrameMode', { native: 1, css: 1 })
const UiLocale = enumOf<'zh-CN' | 'en-US'>()('UiLocale', { 'zh-CN': 1, 'en-US': 1 })
const SettingsIssueKind = enumOf<T.SettingsIssueKind>()('SettingsIssueKind', {
  invalid_json: 1,
  reset_to_default: 1,
  newer_version: 1,
})
const IpcErrorCode = enumOf<T.IpcErrorCode>()('IpcErrorCode', {
  invalid_argument: 1,
  not_found: 1,
  not_ready: 1,
  permission: 1,
  platform: 1,
  keychain: 1,
  io: 1,
  llm: 1,
  conflict: 1,
  not_implemented: 1,
  internal: 1,
})
const UpdatePolicy = enumOf<T.UpdatePolicy>()('UpdatePolicy', {
  notify: 1,
  auto_install: 1,
  off: 1,
})
const TranslateAnchor = enumOf<T.TranslateAnchor>()('TranslateAnchor', { selection: 1, cursor: 1 })
const InputAnchor = enumOf<T.InputAnchor>()('InputAnchor', { cursor: 1, center: 1 })
const ClipPanelPosition = enumOf<T.ClipPanelPosition>()('ClipPanelPosition', {
  center: 1,
  cursor: 1,
  remember: 1,
})
const PasteOn = enumOf<T.PasteOn>()('PasteOn', { single_click: 1, double_click: 1 })
const ThemePref = enumOf<T.ThemePref>()('ThemePref', { system: 1, light: 1, dark: 1 })
const MaterialPref = enumOf<T.MaterialPref>()('MaterialPref', { system: 1, solid: 1 })
const ReduceMotionPref = enumOf<T.ReduceMotionPref>()('ReduceMotionPref', {
  system: 1,
  on: 1,
  off: 1,
})
const LogLevel = enumOf<T.LogLevel>()('LogLevel', { error: 1, warn: 1, info: 1, debug: 1 })

// ===== 对象与联合 =====
const LlmErrorFields: Fields<T.LlmError> = {
  code: ErrorCode,
  subcode: nul(ErrorSubcode),
  message: str,
  retryable: bool,
  hint: nul(str),
  status: nul(num),
  url: nul(str),
  retry_after_s: nul(num),
  upstream_request_id: nul(str),
}
const TranslatePayloadFields: Fields<T.TranslatePayload> = {
  source_text: str,
  request_id: nul(str),
  task: TaskKind,
  style_id: str,
  detected_lang: nul(Lang),
  target_lang: Lang,
  dict_mode: bool,
  long_text: nul(ref('LongTextInfo')),
  not_ready: nul(ref('NotReady')),
  selection_method: nul(str),
}
const OcrPayloadFields: Fields<T.OcrPayload> = {
  capture_id: str,
  request_id: nul(str),
  mode: OcrMode,
  thumb_url: str,
  image_size: ref('PhysSize'),
  target_lang: Lang,
  not_ready: nul(ref('NotReady')),
}
const ClipboardPayloadFields: Fields<T.ClipboardPayload> = {
  page: ref('ClipPage'),
  paused: bool,
  hint_bar: bool,
  paste_on_double_click: bool,
  unavailable: opt(nul(str)),
  // 不进 registry：golden 样例里只有默认值（正在记录时省略），各变体的形状由 wisp-types 的 clipboard_payload_optional_fields 固定
  recording: opt(
    union<T.ClipboardRecording, 'state'>()('ClipboardRecording', 'state', {
      recording: {},
      paused: {},
      disabled: {},
      unavailable: { reason_key: str },
    }),
  ),
  favorites_collapsed: opt(bool),
  queue_unavailable: opt(nul(str)),
}
const InputPayloadFields: Fields<T.InputPayload> = {
  initial_text: str,
  target_lang: nul(Lang),
  hint: nul(ref('NotReady')),
}
const ToastPayloadFields: Fields<T.ToastPayload> = {
  reason: CopiedReason,
  paste_key: str,
  queue: opt(nul(ref('QueueStatus'))),
}
const capability = ref('Capability')
const modelRef = ref('ModelRef')

export const registry = {
  // ----- 基础
  MonitorId: str,
  RequestId: str,
  Point: obj<T.Point>()('Point', { x: num, y: num }),
  Size: obj<T.Size>()('Size', { width: num, height: num }),
  Rect: obj<T.Rect>()('Rect', { x: num, y: num, width: num, height: num }),
  PhysPoint: obj<T.PhysPoint>()('PhysPoint', { x: num, y: num }),
  PhysSize: obj<T.PhysSize>()('PhysSize', { width: num, height: num }),
  PhysRect: obj<T.PhysRect>()('PhysRect', { x: num, y: num, width: num, height: num }),
  MonitorInfo: obj<T.MonitorInfo>()('MonitorInfo', {
    id: str,
    name: str,
    physical_origin: ref('PhysPoint'),
    physical_size: ref('PhysSize'),
    scale_factor: num,
    frame: ref('Rect'),
    work_area: ref('Rect'),
    is_primary: bool,
  }),
  Anchor: obj<T.Anchor>()('Anchor', { monitor_id: str, rect: ref('Rect') }),
  OsKind,
  SessionKind,
  AppRef: obj<T.AppRef>()('AppRef', {
    id: str,
    alt_id: nul(str),
    name: str,
    pid: nul(num),
    path: nul(str),
    window: nul(num),
  }),
  Capability: obj<T.Capability>()('Capability', { available: bool, reason_key: nul(str) }),
  Capabilities: obj<T.Capabilities>()('Capabilities', {
    global_hotkeys: capability,
    selection: capability,
    simulated_copy: capability,
    paste_inject: capability,
    capture_overlay: capability,
    clipboard_watch: capability,
    input_hooks: capability,
  }),
  PlatformInfo: obj<T.PlatformInfo>()('PlatformInfo', {
    os: OsKind,
    os_version: str,
    arch: str,
    session: SessionKind,
    capabilities: ref('Capabilities'),
  }),
  Lang,
  UiLanguage,
  // ----- LLM
  Protocol,
  ThinkingOff,
  MaxTokensField,
  ImageDetail,
  ReasoningEffort,
  Quirks: obj<T.Quirks>()('Quirks', {
    thinking_off: ThinkingOff,
    merge_system_into_user: bool,
    strip_think_tags: bool,
    max_tokens_field: nul(MaxTokensField),
    dual_auth_header: nul(bool),
    gemini_key_in_query: bool,
  }),
  Vision,
  VisionSource,
  ModelCaps: obj<T.ModelCaps>()('ModelCaps', {
    temperature: nul(bool),
    max_tokens_field: nul(MaxTokensField),
    thinking_param: nul(bool),
    effort_param: nul(bool),
    stream_options: nul(bool),
    include_obfuscation: nul(bool),
    anthropic_thinking_disabled: nul(bool),
    anthropic_efforts: nul(arr(str)),
  }),
  ModelEntry: obj<T.ModelEntry>()('ModelEntry', {
    id: str,
    label: nul(str),
    vision: Vision,
    vision_source: nul(VisionSource),
    reasoning: bool,
    enabled: bool,
    missing_upstream: bool,
    deprecated: bool,
    context_tokens: nul(num),
    caps: ref('ModelCaps'),
    reasoning_effort: nul(ReasoningEffort),
  }),
  Timeouts: obj<T.Timeouts>()('Timeouts', { connect_ms: num, first_token_ms: num, idle_ms: num }),
  Provider: obj<T.Provider>()('Provider', {
    id: str,
    name: str,
    protocol: Protocol,
    base_url: str,
    headers: rec(str),
    quirks: ref('Quirks'),
    models: arr(ref('ModelEntry')),
    timeouts: ref('Timeouts'),
    template_id: nul(str),
  }),
  ModelRef: obj<T.ModelRef>()('ModelRef', { provider_id: str, model_id: str }),
  Defaults: obj<T.Defaults>()('Defaults', { translate: nul(modelRef), ocr: nul(modelRef) }),
  Usage: obj<T.Usage>()('Usage', {
    input_tokens: num,
    output_tokens: num,
    reasoning_tokens: nul(num),
    total_tokens: nul(num),
  }),
  ErrorCode,
  ErrorSubcode,
  LlmError: obj<T.LlmError>()('LlmError', LlmErrorFields),
  TruncateReason,
  NoticeKind,
  StreamPhase,
  StreamEvent: union<T.StreamEvent, 'type'>()('StreamEvent', 'type', {
    start: {
      request_id: str,
      provider_id: str,
      model_id: str,
      model_label: str,
      upstream_id: nul(str),
    },
    meta: {
      detected_source_lang: nul(Lang),
      target_lang: nul(Lang),
      dict_mode: nul(bool),
      low_confidence: nul(bool),
    },
    phase: { phase: StreamPhase },
    delta: { text: str },
    notice: { kind: NoticeKind },
    done: {
      elapsed_ms: num,
      usage: nul(ref('Usage')),
      truncated: nul(TruncateReason),
      cached: bool,
      no_text: bool,
    },
    error: LlmErrorFields,
  }),
  TemplateGroup,
  RegionVariant: obj<T.RegionVariant>()('RegionVariant', { label_key: str, base_url: str }),
  ProviderTemplate: obj<T.ProviderTemplate>()('ProviderTemplate', {
    id: str,
    name_key: str,
    group: TemplateGroup,
    provider: ref('Provider'),
    key_required: bool,
    region_variants: arr(ref('RegionVariant')),
  }),
  UrlPreview: obj<T.UrlPreview>()('UrlPreview', {
    method: str,
    url: str,
    version_appended: bool,
    forced: bool,
    warning: opt(nul(str)),
  }),
  ProbeResult: obj<T.ProbeResult>()('ProbeResult', {
    ok: bool,
    status: nul(num),
    elapsed_ms: num,
    url: str,
    error: nul(ref('LlmError')),
  }),
  TestVerdict,
  ProviderTestReport: obj<T.ProviderTestReport>()('ProviderTestReport', {
    models: ref('ProbeResult'),
    stream: ref('ProbeResult'),
    overall: TestVerdict,
    model_used: nul(str),
    fetched_models: arr(ref('ModelEntry')),
    notice: opt(nul(str)),
  }),
  // ----- 风格
  OutputFormat,
  Style: obj<T.Style>()('Style', {
    id: str,
    name: str,
    builtin: bool,
    system_template: str,
    tail_instruction: str,
    temperature: nul(num),
    output: OutputFormat,
    dictionary_mode: bool,
    model_ref: nul(modelRef),
    hotkey: nul(str),
    modified: bool,
  }),
  // 键缺省 = 不覆写；temperature / model_ref / hotkey 显式 null = 覆写为空
  StyleOverride: obj<T.StyleOverride>()('StyleOverride', {
    system_template: opt(str),
    tail_instruction: opt(str),
    temperature: opt(nul(num)),
    output: opt(OutputFormat),
    dictionary_mode: opt(bool),
    model_ref: opt(nul(modelRef)),
    hotkey: opt(nul(str)),
  }),
  StylesFile: obj<T.StylesFile>()('StylesFile', {
    schema_version: num,
    order: arr(str),
    overrides: rec(ref('StyleOverride')),
    custom: arr(ref('Style')),
  }),
  GlossaryEntry: obj<T.GlossaryEntry>()('GlossaryEntry', {
    source: str,
    target: str,
    note: nul(str),
  }),
  GlossaryFile: obj<T.GlossaryFile>()('GlossaryFile', {
    schema_version: num,
    entries: arr(ref('GlossaryEntry')),
  }),
  OcrMode,
  TaskKind,
  DictMode,
  PromptPreview: obj<T.PromptPreview>()('PromptPreview', { system: str, user: str }),
  // ----- 剪贴板
  ClipKind,
  ClipFilter,
  SourceApp: obj<T.SourceApp>()('SourceApp', { id: str, name: str, icon_url: nul(str) }),
  ImageMeta: obj<T.ImageMeta>()('ImageMeta', {
    width: num,
    height: num,
    bytes: num,
    thumb_url: nul(str),
  }),
  FileRef: obj<T.FileRef>()('FileRef', { path: str, name: str, exists: bool, size: nul(num) }),
  ClipItemMeta: obj<T.ClipItemMeta>()('ClipItemMeta', {
    id: num,
    kind: ClipKind,
    created_at: num,
    last_used_at: num,
    use_count: num,
    pinned: bool,
    source: nul(ref('SourceApp')),
    preview: str,
    char_count: nul(num),
    line_count: nul(num),
    truncated: bool,
    image: nul(ref('ImageMeta')),
    files: nul(arr(ref('FileRef'))),
    link: nul(str),
    color: nul(str),
    name: opt(nul(str)),
    expires_at: opt(nul(num)),
  }),
  EntityKind,
  Entity: obj<T.Entity>()('Entity', {
    kind: EntityKind,
    value: str,
    start: num,
    end: num,
    sensitive: bool,
  }),
  TransformId,
  ClipInspect: obj<T.ClipInspect>()('ClipInspect', {
    entities: arr(ref('Entity')),
    transforms: arr(TransformId),
  }),
  QueueStatus: obj<T.QueueStatus>()('QueueStatus', { active: bool, index: num, total: num }),
  ClipDragData: obj<T.ClipDragData>()('ClipDragData', { text: str, html: nul(str), uri: nul(str) }),
  HighlightRange: obj<T.HighlightRange>()('HighlightRange', { start: num, end: num }),
  ClipHit: obj<T.ClipHit>()('ClipHit', {
    item: ref('ClipItemMeta'),
    highlights: arr(ref('HighlightRange')),
  }),
  ClipPage: obj<T.ClipPage>()('ClipPage', {
    items: arr(ref('ClipHit')),
    pinned_count: num,
    next_offset: nul(num),
    query: nul(str),
    filter: ClipFilter,
  }),
  ClipItemDetail: obj<T.ClipItemDetail>()('ClipItemDetail', {
    meta: ref('ClipItemMeta'),
    text: nul(str),
    has_html: bool,
    has_rtf: bool,
    image_url: nul(str),
    files: nul(arr(ref('FileRef'))),
  }),
  PasteMode,
  CopiedReason,
  PasteOutcome: union<T.PasteOutcome, 'type'>()('PasteOutcome', 'type', {
    pasted: {},
    copied: { reason: CopiedReason },
  }),
  ClipboardChanged: union<T.ClipboardChanged, 'change'>()('ClipboardChanged', 'change', {
    inserted: { item: ref('ClipItemMeta') },
    touched: { item: ref('ClipItemMeta') },
    updated: { item: ref('ClipItemMeta') },
    removed: { id: num },
    cleared: { keep_pinned: bool },
    paused: { paused: bool },
    secret_skipped: {},
    reloaded: {},
  }),
  RawClipboard: obj<T.RawClipboard>()('RawClipboard', {
    change_id: num,
    source: nul(ref('AppRef')),
    concealed: bool,
    from_wisp: bool,
    text: nul(str),
    text_truncated: bool,
    html: nul(str),
    rtf: nul(arr(num)),
    image_png: nul(arr(num)),
    image_size: nul(ref('PhysSize')),
    files: nul(arr(str)),
  }),
  ClipPayload: obj<T.ClipPayload>()('ClipPayload', {
    text: nul(str),
    html: nul(str),
    rtf: nul(arr(num)),
    image_png: nul(arr(num)),
    files: nul(arr(str)),
  }),
  OcrHistoryItem: obj<T.OcrHistoryItem>()('OcrHistoryItem', {
    id: num,
    created_at: num,
    mode: OcrMode,
    model: str,
    text: str,
    thumb_url: nul(str),
    image_url: nul(str),
  }),
  OcrHistoryChanged: union<T.OcrHistoryChanged, 'change'>()('OcrHistoryChanged', 'change', {
    inserted: { item: ref('OcrHistoryItem') },
    removed: { ids: arr(num) },
    cleared: {},
  }),
  // ----- 快捷键 / 权限
  HotkeyAction,
  Binding: str,
  HotkeyTarget: union<T.HotkeyTarget, 'kind'>()('HotkeyTarget', 'kind', {
    action: { action: HotkeyAction },
    style: { style_id: str },
  }),
  ConflictLevel,
  ConflictSource,
  Conflict: obj<T.Conflict>()('Conflict', {
    level: ConflictLevel,
    source: ConflictSource,
    message_key: str,
    params: rec(str),
  }),
  ConflictReport: obj<T.ConflictReport>()('ConflictReport', {
    combo: str,
    display: str,
    conflicts: arr(ref('Conflict')),
    acceptable: bool,
  }),
  HotkeyState,
  HotkeyStatus: obj<T.HotkeyStatus>()('HotkeyStatus', {
    target: ref('HotkeyTarget'),
    binding: str,
    effective: nul(str),
    display: nul(str),
    state: HotkeyState,
    reason_key: nul(str),
  }),
  ModifierKey,
  MouseButtonKind,
  ExtTriggerState,
  ExtTriggerStatus: obj<T.ExtTriggerStatus>()('ExtTriggerStatus', {
    bobber: ExtTriggerState,
    double_tap: ExtTriggerState,
    mouse: ExtTriggerState,
    reason_key: nul(str),
  }),
  NeighborHit: obj<T.NeighborHit>()('NeighborHit', {
    app_name: str,
    combo: str,
    display: str,
    action: HotkeyAction,
  }),
  KeyboardAdvice: obj<T.KeyboardAdvice>()('KeyboardAdvice', {
    has_altgr: bool,
    multi_input_lang: bool,
    recommend_family: nul(str),
    qq_running: bool,
  }),
  PermissionKind,
  PermissionState,
  PermissionSet: obj<T.PermissionSet>()('PermissionSet', {
    accessibility: PermissionState,
    screen_recording: PermissionState,
    input_monitoring: PermissionState,
    lost_after_update: bool,
  }),
  // ----- 面板 / 窗口
  PanelView,
  Placement,
  PanelTransition,
  LongTextInfo: obj<T.LongTextInfo>()('LongTextInfo', {
    chars: num,
    est_tokens: num,
    est_seconds: nul(num),
  }),
  NotReady: union<T.NotReady, 'reason'>()('NotReady', 'reason', {
    no_provider: {},
    no_default_model: { task: TaskKind },
    no_vision_model: {},
    permission_missing: { permission: PermissionKind },
    wayland_copy_first: {},
    capture_failed: { message: str },
    elevated: {},
    keychain_unavailable: {},
    selection_unavailable: { reason_key: str },
    offline_model_missing: { model: LocalModelId },
    offline_model_downloading: { model: LocalModelId },
    offline_task_unsupported: { task: TaskKind },
  }),
  TranslatePayload: obj<T.TranslatePayload>()('TranslatePayload', TranslatePayloadFields),
  OcrPayload: obj<T.OcrPayload>()('OcrPayload', OcrPayloadFields),
  ClipboardPayload: obj<T.ClipboardPayload>()('ClipboardPayload', ClipboardPayloadFields),
  ClipFloatInit: obj<T.ClipFloatInit>()('ClipFloatInit', {
    query: str,
    filter: ClipFilter,
    selected_id: nul(num),
    on_top: bool,
    payload: ref('ClipboardPayload'),
  }),
  InputPayload: obj<T.InputPayload>()('InputPayload', InputPayloadFields),
  ToastPayload: obj<T.ToastPayload>()('ToastPayload', ToastPayloadFields),
  PanelPayload: union<T.PanelPayload, 'kind'>()('PanelPayload', 'kind', {
    translate: TranslatePayloadFields,
    ocr: OcrPayloadFields,
    clipboard: ClipboardPayloadFields,
    input: InputPayloadFields,
    toast: ToastPayloadFields,
  }),
  PanelPrepare: obj<T.PanelPrepare>()('PanelPrepare', {
    session_id: num,
    view: PanelView,
    payload: ref('PanelPayload'),
    width: num,
    max_height: num,
    placement: Placement,
    origin_x: num,
    focused: bool,
    transition: PanelTransition,
  }),
  HideReason,
  PanelShown: obj<T.PanelShown>()('PanelShown', { session_id: num, focused: bool }),
  PanelHideRequest: obj<T.PanelHideRequest>()('PanelHideRequest', {
    session_id: num,
    reason: HideReason,
  }),
  PanelFocusChanged: obj<T.PanelFocusChanged>()('PanelFocusChanged', {
    session_id: num,
    focused: bool,
  }),
  LatencyMarks: obj<T.LatencyMarks>()('LatencyMarks', {
    session_id: num,
    origin: str,
    method: nul(str),
    selection_ms: nul(num),
    prepare_ms: num,
    ready_ms: num,
    shown_ms: num,
    first_token_ms: nul(num),
    ready_timeout: bool,
  }),
  TranslateReq: obj<T.TranslateReq>()('TranslateReq', {
    session_id: num,
    text: str,
    task: TaskKind,
    style_id: str,
    target_lang: nul(Lang),
    model_ref: nul(modelRef),
    dict_mode: DictMode,
    bypass_cache: bool,
    draft: nul(str),
    confirm_long: opt(bool),
  }),
  OcrReq: obj<T.OcrReq>()('OcrReq', {
    session_id: num,
    capture_id: str,
    mode: OcrMode,
    model_ref: nul(modelRef),
    target_lang: nul(Lang),
  }),
  StartAck: obj<T.StartAck>()('StartAck', {
    request_id: str,
    width: nul(num),
    long_text: opt(nul(ref('LongTextInfo'))),
  }),
  PinKind,
  PinState: obj<T.PinState>()('PinState', {
    kind: PinKind,
    source_text: str,
    result_text: str,
    task: TaskKind,
    style_id: str,
    target_lang: Lang,
    detected_lang: nul(Lang),
    request_id: nul(str),
    capture_id: nul(str),
    ocr_text: nul(str),
    ocr_mode: nul(OcrMode),
    show_source: bool,
    status_line: nul(str),
    dict_mode: opt(bool),
    model_ref: opt(nul(modelRef)),
    status: opt(nul(ref('PinStatus'))),
    thumb_url: opt(nul(str)),
    image_size: opt(nul(ref('PhysSize'))),
    expand_width: opt(nul(num)),
    on_top: opt(nul(bool)),
  }),
  PinStatus: obj<T.PinStatus>()('PinStatus', {
    model_label: str,
    elapsed_ms: nul(num),
    usage: nul(ref('Usage')),
    cached: bool,
    cancelled: bool,
    truncated: nul(TruncateReason),
  }),
  WindowTarget: union<T.WindowTarget, 'kind'>()('WindowTarget', 'kind', {
    settings: { section: nul(str), anchor: opt(nul(str)) },
    onboarding: { step: nul(num) },
  }),
  OverlayMode,
  OverlaySelectionKind,
  OverlayInit: obj<T.OverlayInit>()('OverlayInit', {
    session_id: num,
    monitor: ref('MonitorInfo'),
    image_url: str,
    image_size: ref('PhysSize'),
    windows: arr(ref('Rect')),
    mode: OverlayMode,
    window_detect: bool,
    fly_animation: bool,
    reduced_motion: bool,
    frozen_native: bool,
    half_url: nul(str),
    region_url: opt(nul(str)),
  }),
  OverlaySessionStart: obj<T.OverlaySessionStart>()('OverlaySessionStart', {
    session_id: num,
    monitor_id: str,
  }),
  OverlayEndReason,
  OverlaySessionEnd: obj<T.OverlaySessionEnd>()('OverlaySessionEnd', {
    session_id: num,
    reason: OverlayEndReason,
  }),
  OverlayCommit: obj<T.OverlayCommit>()('OverlayCommit', {
    session_id: num,
    monitor_id: str,
    rect: ref('Rect'),
    kind: OverlaySelectionKind,
  }),
  OverlayCommitAck: obj<T.OverlayCommitAck>()('OverlayCommitAck', { fly_to: nul(ref('Rect')) }),
  // ----- 应用
  KeychainState,
  UpdateState,
  UpdateStatus: obj<T.UpdateStatus>()('UpdateStatus', {
    state: UpdateState,
    version: nul(str),
    notes: nul(str),
    progress: nul(num),
    error: nul(str),
  }),
  RuntimeInfo: obj<T.RuntimeInfo>()('RuntimeInfo', {
    version: str,
    platform: ref('PlatformInfo'),
    has_key: rec(bool),
    keychain: KeychainState,
    hotkeys: arr(ref('HotkeyStatus')),
    ext_triggers: ref('ExtTriggerStatus'),
    permissions: ref('PermissionSet'),
    clipboard_paused: bool,
    update: ref('UpdateStatus'),
    glossary_count: num,
    settings_issue: nul(ref('SettingsFileIssue')),
    orphan_keys: num,
    autostart_issue: opt(nul(str)),
    sync: ref('SyncStatus'),
    offline: ref('OfflineSummary'),
  }),
  SettingsIssueKind,
  SettingsFileIssue: obj<T.SettingsFileIssue>()('SettingsFileIssue', {
    kind: SettingsIssueKind,
    file: str,
    backup_path: nul(str),
    at_ms: num,
  }),
  SettingsSnapshot: obj<T.SettingsSnapshot>()('SettingsSnapshot', {
    settings: ref('Settings'),
    styles: arr(ref('Style')),
    runtime: ref('RuntimeInfo'),
  }),
  ResolvedTheme,
  ResolvedMaterial,
  AppearanceState: obj<T.AppearanceState>()('AppearanceState', {
    theme: ResolvedTheme,
    accent: str,
    reduce_motion: bool,
    reduce_transparency: bool,
    material: ResolvedMaterial,
    font_size: num,
    os: OsKind,
    ui_language: UiLocale,
  }),
  AppMeta: obj<T.AppMeta>()('AppMeta', {
    version: str,
    platform: ref('PlatformInfo'),
    config_dir: str,
    data_dir: str,
    log_dir: str,
    dev_mode: bool,
  }),
  PathKind,
  SendTarget,
  ActionOrigin,
  FrameMode,
  Boot: obj<T.Boot>()('Boot', {
    label: str,
    os: OsKind,
    os_version: str,
    theme: ResolvedTheme,
    accent: str,
    reduce_motion: bool,
    reduce_transparency: bool,
    material: ResolvedMaterial,
    frame_mode: FrameMode,
    shadow_margin: num,
    font_size: num,
    ui_language: UiLocale,
    dev: bool,
    debug_build: bool,
    route: nul(ref('WindowTarget')),
    prepare: opt(nul(ref('PanelPrepare'))),
  }),
  OnboardingHotkey: obj<T.OnboardingHotkey>()('OnboardingHotkey', { action: HotkeyAction }),
  HotkeyTestReceived: obj<T.HotkeyTestReceived>()('HotkeyTestReceived', {
    target: ref('HotkeyTarget'),
  }),
  IpcErrorCode,
  IpcError: obj<T.IpcError>()('IpcError', {
    code: IpcErrorCode,
    message: str,
    llm: nul(ref('LlmError')),
  }),
  // ----- settings.json
  UpdatePolicy,
  General: obj<T.General>()('General', {
    launch_at_login: bool,
    start_hidden: bool,
    ui_language: UiLanguage,
    update_policy: UpdatePolicy,
    developer_mode: bool,
  }),
  BobberSettings: obj<T.BobberSettings>()('BobberSettings', { enabled: bool }),
  DoubleTapSettings: obj<T.DoubleTapSettings>()('DoubleTapSettings', {
    enabled: bool,
    key: nul(ModifierKey),
    interval_ms: num,
  }),
  MouseBinding: obj<T.MouseBinding>()('MouseBinding', { enabled: bool, action: nul(HotkeyAction) }),
  MouseSettings: obj<T.MouseSettings>()('MouseSettings', {
    enabled: bool,
    long_press: bool,
    middle: ref('MouseBinding'),
    back: ref('MouseBinding'),
    forward: ref('MouseBinding'),
  }),
  AppKeymap: obj<T.AppKeymap>()('AppKeymap', {
    os: OsKind,
    app_id: str,
    copy: nul(str),
    paste: nul(str),
  }),
  HotkeySettings: obj<T.HotkeySettings>()('HotkeySettings', {
    bindings: rec(str, HotkeyAction),
    bobber: ref('BobberSettings'),
    double_tap: ref('DoubleTapSettings'),
    mouse: ref('MouseSettings'),
    app_keymaps: arr(ref('AppKeymap')),
    copy_wait_ms: num,
  }),
  TranslateAnchor,
  InputAnchor,
  TranslateSettings: obj<T.TranslateSettings>()('TranslateSettings', {
    primary_lang: Lang,
    secondary_lang: Lang,
    default_style: str,
    long_text_threshold: num,
    close_on_click_outside: bool,
    close_on_mouse_leave: bool,
    mouse_leave_px: num,
    anchor: TranslateAnchor,
    input_anchor: InputAnchor,
    cache_enabled: bool,
    show_source_by_default: bool,
  }),
  OcrSettings: obj<T.OcrSettings>()('OcrSettings', {
    default_mode: OcrMode,
    max_edge_px: num,
    auto_copy: bool,
    history: bool,
    history_limit: num,
    window_detect: bool,
    fly_animation: bool,
    direct_translate_transcribe: bool,
    image_detail: ImageDetail,
  }),
  ClipPanelPosition,
  PasteOn,
  ExcludedApp: obj<T.ExcludedApp>()('ExcludedApp', { os: OsKind, app_id: str, name: str }),
  ClipboardSettings: obj<T.ClipboardSettings>()('ClipboardSettings', {
    enabled: bool,
    record_text: bool,
    record_rich_text: bool,
    record_images: bool,
    record_files: bool,
    max_items: num,
    max_age_days: nul(num),
    max_text_bytes: num,
    max_image_bytes: num,
    excluded_apps: arr(ref('ExcludedApp')),
    secret_detection: bool,
    secret_tray_flash: bool,
    panel_position: ClipPanelPosition,
    paste_on: PasteOn,
    show_hint_bar: bool,
    smart: ref('SmartSettings'),
  }),
  SmartSettings: obj<T.SmartSettings>()('SmartSettings', {
    contacts: bool,
    codes: bool,
    numbers: bool,
    identity: bool,
    otp_auto_delete: bool,
  }),
  ThemePref,
  MaterialPref,
  ReduceMotionPref,
  Appearance: obj<T.Appearance>()('Appearance', {
    theme: ThemePref,
    accent: str,
    font_size: num,
    translate_min_width: num,
    translate_max_width: num,
    clipboard_width: num,
    max_height_ratio: num,
    material: MaterialPref,
    reduce_motion: ReduceMotionPref,
  }),
  LogLevel,
  Advanced: obj<T.Advanced>()('Advanced', {
    log_level: LogLevel,
    webview_low_memory_when_hidden: bool,
    unload_panel_after_idle_min: nul(num),
  }),
  Settings: obj<T.Settings>()('Settings', {
    schema_version: num,
    general: ref('General'),
    hotkeys: ref('HotkeySettings'),
    providers: arr(ref('Provider')),
    defaults: ref('Defaults'),
    translate: ref('TranslateSettings'),
    ocr: ref('OcrSettings'),
    clipboard: ref('ClipboardSettings'),
    appearance: ref('Appearance'),
    advanced: ref('Advanced'),
    sync: ref('SyncSettings'),
    offline: ref('OfflineSettings'),
  }),
  OfflineSettings: obj<T.OfflineSettings>()('OfflineSettings', {
    ocr_model: nul(LocalModelId),
    translate_model: nul(LocalModelId),
    idle_release_min: num,
    use_gpu: bool,
    download_source: DownloadSource,
  }),
  SyncContent: obj<T.SyncContent>()('SyncContent', {
    settings: bool,
    api_keys: bool,
    clipboard: bool,
    images: bool,
    ocr_history: bool,
    verification_codes: bool,
  }),
  SyncSettings: obj<T.SyncSettings>()('SyncSettings', {
    enabled: bool,
    server_url: str,
    username: str,
    auto: bool,
    content: ref('SyncContent'),
  }),
  // ----- 同步
  SyncPhase,
  SyncErrorKind,
  SyncProgress: obj<T.SyncProgress>()('SyncProgress', { done: num, total: num }),
  SyncStatus: obj<T.SyncStatus>()('SyncStatus', {
    phase: SyncPhase,
    last_ok_ms: nul(num),
    pending: bool,
    error: nul(SyncErrorKind),
    transient: bool,
    error_detail: nul(str),
    progress: nul(ref('SyncProgress')),
    device_count: num,
  }),
  SyncDevice: obj<T.SyncDevice>()('SyncDevice', {
    id: str,
    name: str,
    os: OsKind,
    app_version: str,
    last_seen_ms: num,
    this_device: bool,
  }),
  SyncAccount: obj<T.SyncAccount>()('SyncAccount', { server_url: str, username: str }),
  SyncVaultInfo: obj<T.SyncVaultInfo>()('SyncVaultInfo', {
    supported: bool,
    device_count: num,
    last_modified_ms: nul(num),
    restorable: bool,
  }),
  SyncProbe: obj<T.SyncProbe>()('SyncProbe', {
    ok: bool,
    error: nul(SyncErrorKind),
    error_detail: nul(str),
    vault: nul(ref('SyncVaultInfo')),
  }),
  SyncConnectOutcome,
  SyncConnectResult: obj<T.SyncConnectResult>()('SyncConnectResult', {
    outcome: SyncConnectOutcome,
    status: ref('SyncStatus'),
  }),
  // ----- 离线模型
  LocalModelId,
  LocalModelKind,
  LocalRuntimeId,
  DownloadSource,
  DownloadErrorCode,
  LoadState,
  FitnessLevel,
  LocalModelState: union<T.LocalModelState, 'state'>()('LocalModelState', 'state', {
    not_installed: {},
    queued: {},
    downloading: { done: num, total: num, bytes_per_sec: num },
    paused: { done: num, total: num },
    verifying: {},
    installed: {},
    broken: {},
    failed: { code: DownloadErrorCode },
  }),
  Fitness: obj<T.Fitness>()('Fitness', { level: FitnessLevel, reason_key: nul(str) }),
  LocalModelInfo: obj<T.LocalModelInfo>()('LocalModelInfo', {
    id: LocalModelId,
    kind: LocalModelKind,
    download_bytes: num,
    pending_runtime_bytes: num,
    state: ref('LocalModelState'),
    load: LoadState,
    memory_estimate_mb: num,
    fitness: ref('Fitness'),
    update_available: bool,
  }),
  LocalRuntimeInfo: obj<T.LocalRuntimeInfo>()('LocalRuntimeInfo', {
    id: LocalRuntimeId,
    version: str,
    installed: bool,
    size: num,
    in_use: bool,
  }),
  GpuInfo: obj<T.GpuInfo>()('GpuInfo', { name: str, vram_total_mb: num, integrated: bool }),
  DeviceInfo: obj<T.DeviceInfo>()('DeviceInfo', {
    total_memory: nul(num),
    apple_silicon: bool,
    gpus: arr(ref('GpuInfo')),
    gpu_probed: bool,
  }),
  LocalModelsSnapshot: obj<T.LocalModelsSnapshot>()('LocalModelsSnapshot', {
    models: arr(ref('LocalModelInfo')),
    runtimes: arr(ref('LocalRuntimeInfo')),
    device: ref('DeviceInfo'),
    disk_used: num,
    disk_free: nul(num),
  }),
  OfflineSummary: obj<T.OfflineSummary>()('OfflineSummary', {
    installed: arr(LocalModelId),
  }),
} as const satisfies Record<string, Spec>

export type TypeName = keyof typeof registry

/**
 * 只在 Rust 内部流转、不跨 IPC 的类型：有黄金样例就校验，没有不算缺失。
 * 其余对象 / 联合类型都必须至少有一个样例（IPC 与配置文档的每个类型）。
 */
export const RUST_INTERNAL: ReadonlySet<TypeName> = new Set<TypeName>([
  'PhysRect',
  'RawClipboard',
  'ClipPayload',
  'StylesFile',
  'StyleOverride',
  'GlossaryFile',
])

// ---------------------------------------------------------------------------
// 运行期校验

export interface ValidationResult {
  errors: string[]
  /** 本次校验中出现过的联合变体：`<Union>.<tag 值>` */
  variants: Set<string>
}

export function validate(spec: Spec, value: unknown, path = '$', out?: ValidationResult) {
  const result = out ?? { errors: [], variants: new Set<string>() }
  check(spec, value, path, result)
  return result
}

function check(spec: Spec, value: unknown, path: string, r: ValidationResult): void {
  const fail = (msg: string): void => {
    r.errors.push(`${path}: ${msg}（实际 ${preview(value)}）`)
  }
  switch (spec.t) {
    case 'unknown':
      return
    case 'string':
      if (typeof value !== 'string') fail('应为 string')
      return
    case 'number':
      if (typeof value !== 'number' || !Number.isFinite(value)) fail('应为 number')
      return
    case 'bool':
      if (typeof value !== 'boolean') fail('应为 boolean')
      return
    case 'enum':
      if (typeof value !== 'string' || !spec.values.has(value)) {
        fail(`应为 ${spec.name}（${[...spec.values].join(' | ')}）`)
      }
      return
    case 'nullable':
      if (value !== null) check(spec.of, value, path, r)
      return
    case 'optional':
      // 键存在时才会走到这里（缺省由 checkFields 放行）；undefined 不是合法 JSON 值
      if (value === undefined) fail('可缺省的键不能为 undefined')
      else check(spec.of, value, path, r)
      return
    case 'array':
      if (!Array.isArray(value)) return fail('应为数组')
      value.forEach((v, i) => check(spec.of, v, `${path}[${i}]`, r))
      return
    case 'record':
      if (!isObject(value)) return fail('应为对象（map）')
      for (const [k, v] of Object.entries(value)) {
        if (spec.keys) check(spec.keys, k, `${path}{key ${k}}`, r)
        check(spec.of, v, `${path}.${k}`, r)
      }
      return
    case 'object':
      if (!isObject(value)) return fail(`应为 ${spec.name} 对象`)
      checkFields(spec.name, spec.fields, value, path, r, [])
      return
    case 'union': {
      if (!isObject(value)) return fail(`应为 ${spec.name} 对象`)
      const tag = value[spec.tag]
      const variant = typeof tag === 'string' ? spec.variants[tag] : undefined
      if (!variant) {
        return fail(`${spec.name}.${spec.tag} 应为 ${Object.keys(spec.variants).join(' | ')}`)
      }
      r.variants.add(`${spec.name}.${tag as string}`)
      checkFields(`${spec.name}.${tag as string}`, variant, value, path, r, [spec.tag])
      return
    }
    case 'ref':
      check(spec.get(), value, path, r)
      return
  }
}

function checkFields(
  name: string,
  fields: Readonly<Record<string, Spec>>,
  value: Record<string, unknown>,
  path: string,
  r: ValidationResult,
  extra: readonly string[],
): void {
  const expected = new Set([...Object.keys(fields), ...extra])
  const actual = new Set(Object.keys(value))
  const missing = [...expected].filter((k) => !actual.has(k) && fields[k]?.t !== 'optional')
  const unexpected = [...actual].filter((k) => !expected.has(k))
  if (missing.length) r.errors.push(`${path}: ${name} 缺少键 ${missing.join(', ')}`)
  if (unexpected.length) r.errors.push(`${path}: ${name} 多出键 ${unexpected.join(', ')}`)
  for (const [k, spec] of Object.entries(fields)) {
    if (actual.has(k)) check(spec, value[k], `${path}.${k}`, r)
  }
}

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v)
}

function preview(v: unknown): string {
  const s = JSON.stringify(v)
  return s === undefined ? String(v) : s.length > 60 ? `${s.slice(0, 57)}...` : s
}

/** 解开 ref，取到真正的 Spec */
export function resolve(spec: Spec): Spec {
  return spec.t === 'ref' ? resolve(spec.get()) : spec
}
