/**
 * mock 的基础样例。
 *
 * - `golden(name)`：读取 `ipc/fixtures/<name>.json`（wisp-types 黄金测试生成，与 Rust 序列化逐字节一致）；
 * - 其余为手写的展示数据（视觉 QA 需要可读、稳定的内容），全部受 `types.ts` 类型约束。
 *   `settings()` 优先用黄金样例里的 `Settings`（即 Rust 的 `Settings::default()`），没有时用这里的默认值镜像。
 */
import type { Boot } from '../boot'
import BUILTIN_STYLES from '../fixtures/consts/BuiltinStyles.json'
import type {
  AppearanceState,
  AppMeta,
  ClipFilter,
  ClipHit,
  ClipItemMeta,
  ClipPage,
  ExcludedApp,
  HotkeyAction,
  HotkeyStatus,
  KeyboardAdvice,
  ModelEntry,
  OsKind,
  PanelPayload,
  PermissionSet,
  PinState,
  PlatformInfo,
  Provider,
  ProviderTemplate,
  RuntimeInfo,
  Settings,
  SourceApp,
  Style,
  UpdateStatus,
} from '../types'

const goldenFiles = import.meta.glob<unknown>('../fixtures/*.json', {
  eager: true,
  import: 'default',
})

const goldenByName = new Map<string, unknown>(
  Object.entries(goldenFiles).map(([path, value]) => [
    path.slice(path.lastIndexOf('/') + 1, -'.json'.length),
    value,
  ]),
)

export const SAMPLE = {
  source:
    'Wisp stays out of your way: it idles at near-zero CPU, wakes in milliseconds, and streams the translation right next to your cursor.',
  translation: 'Wisp 从不打扰你：空闲时几乎不占 CPU，唤出只需几毫秒，译文就在光标旁边逐字流出。',
  ocr: '性能预算\n面板唤出 p95 ≤ 100 ms\n首 token p95 ≤ 1.2 s\n空闲 CPU ≈ 0%\n常驻内存 ≤ 60 MB',
  ocrTranslation:
    'Performance budget\nPanel show p95 ≤ 100 ms\nFirst token p95 ≤ 1.2 s\nIdle CPU ≈ 0%\nResident memory ≤ 60 MB',
} as const

export const HOTKEY_ACTIONS: readonly HotkeyAction[] = [
  'translate_selection',
  'translate_input',
  'ocr',
  'ocr_translate',
  'clipboard',
]

/** 各平台的默认快捷键（mock 展示用；权威表在 wisp-types/defaults.rs） */
const DEFAULT_COMBOS: Record<'macos' | 'other', Record<HotkeyAction, [string, string]>> = {
  macos: {
    translate_selection: ['Alt+KeyD', '⌥D'],
    translate_input: ['Alt+Shift+KeyD', '⌥⇧D'],
    ocr: ['Alt+KeyS', '⌥S'],
    ocr_translate: ['Alt+Shift+KeyS', '⌥⇧S'],
    clipboard: ['Alt+KeyV', '⌥V'],
  },
  other: {
    translate_selection: ['Control+Alt+KeyD', 'Ctrl+Alt+D'],
    translate_input: ['Control+Alt+Shift+KeyD', 'Ctrl+Alt+Shift+D'],
    ocr: ['Control+Alt+KeyS', 'Ctrl+Alt+S'],
    ocr_translate: ['Control+Alt+Shift+KeyS', 'Ctrl+Alt+Shift+S'],
    clipboard: ['Control+Alt+KeyV', 'Ctrl+Alt+V'],
  },
}

const EXCLUDED_APPS: ExcludedApp[] = [
  { os: 'macos', app_id: 'com.1password.1password', name: '1Password' },
  { os: 'macos', app_id: 'com.agilebits.onepassword7', name: '1Password 7' },
  { os: 'macos', app_id: 'com.bitwarden.desktop', name: 'Bitwarden' },
  { os: 'macos', app_id: 'org.keepassxc.keepassxc', name: 'KeePassXC' },
  { os: 'macos', app_id: 'com.apple.keychainaccess', name: 'Keychain Access' },
  { os: 'windows', app_id: '1password.exe', name: '1Password' },
  { os: 'windows', app_id: 'bitwarden.exe', name: 'Bitwarden' },
  { os: 'windows', app_id: 'keepassxc.exe', name: 'KeePassXC' },
  { os: 'windows', app_id: 'keepass.exe', name: 'KeePass' },
  { os: 'linux', app_id: '1password', name: '1Password' },
  { os: 'linux', app_id: 'bitwarden', name: 'Bitwarden' },
  { os: 'linux', app_id: 'keepassxc', name: 'KeePassXC' },
]

export type MockFixtures = ReturnType<typeof createFixtures>

export function createFixtures(boot: Boot) {
  const os = boot.os
  const mod = os === 'macos' ? '⌘' : 'Ctrl+'
  const now = Date.now()
  const minutes = (n: number) => now - n * 60_000

  function golden<T>(name: string): T | undefined {
    const value = goldenByName.get(name)
    return value === undefined ? undefined : (structuredClone(value) as T)
  }

  function platform(): PlatformInfo {
    const ok = { available: true, reason_key: null }
    return {
      os,
      os_version: os === 'macos' ? '26.4' : os === 'windows' ? '10.0.26100' : '6.8.0',
      arch: os === 'macos' ? 'aarch64' : 'x86_64',
      session: os === 'linux' ? 'x11' : 'native',
      capabilities: {
        global_hotkeys: ok,
        selection: ok,
        simulated_copy: ok,
        paste_inject: ok,
        capture_overlay: ok,
        clipboard_watch: ok,
        input_hooks: ok,
      },
    }
  }

  function defaultSettings(): Settings {
    return {
      schema_version: 1,
      general: {
        launch_at_login: true,
        start_hidden: true,
        ui_language: 'system',
        update_policy: 'notify',
        developer_mode: boot.dev,
      },
      hotkeys: {
        bindings: {},
        bobber: { enabled: false },
        double_tap: { enabled: false, key: null, interval_ms: 300 },
        mouse: {
          enabled: false,
          long_press: false,
          middle: { enabled: true, action: 'translate_selection' },
          back: { enabled: false, action: 'clipboard' },
          forward: { enabled: false, action: 'ocr' },
        },
        app_keymaps: [],
        copy_wait_ms: 150,
      },
      providers: [],
      defaults: { translate: null, ocr: null },
      translate: {
        primary_lang: 'zh-CN',
        secondary_lang: 'en',
        default_style: 'natural',
        long_text_threshold: 3000,
        close_on_click_outside: true,
        close_on_mouse_leave: false,
        mouse_leave_px: 160,
        anchor: 'selection',
        input_anchor: 'cursor',
        cache_enabled: true,
        show_source_by_default: false,
      },
      ocr: {
        default_mode: 'lines',
        max_edge_px: 2048,
        auto_copy: true,
        history: false,
        history_limit: 50,
        window_detect: true,
        fly_animation: true,
        direct_translate_transcribe: false,
        image_detail: 'high',
      },
      clipboard: {
        enabled: true,
        record_text: true,
        record_rich_text: true,
        record_images: true,
        record_files: true,
        max_items: 1000,
        max_age_days: null,
        max_text_bytes: 1_048_576,
        max_image_bytes: 20_971_520,
        excluded_apps: EXCLUDED_APPS.map((a) => ({ ...a })),
        secret_detection: true,
        secret_tray_flash: true,
        panel_position: 'center',
        paste_on: 'single_click',
        show_hint_bar: true,
      },
      appearance: {
        theme: 'system',
        accent: 'system',
        font_size: 14,
        translate_min_width: 360,
        translate_max_width: 520,
        clipboard_width: 420,
        max_height_ratio: 0.6,
        material: 'system',
        reduce_motion: 'system',
      },
      advanced: {
        log_level: 'info',
        webview_low_memory_when_hidden: true,
        unload_panel_after_idle_min: null,
      },
    }
  }

  function settings(): Settings {
    return golden<Settings>('Settings') ?? defaultSettings()
  }

  function styles(): Style[] {
    // 6 个内置风格由 Rust 生成（wisp-prompts tests/fixtures_consts.rs，`node scripts/gen-fixtures.mjs`）：模板正文、
    // temperature、输出格式、词典式开关都与 builtin_styles() 一致。每次返回新对象，调用方可以随意改
    return (BUILTIN_STYLES as Style[]).map((s) => ({ ...s }))
  }

  function hotkeyStatuses(): HotkeyStatus[] {
    const table = DEFAULT_COMBOS[os === 'macos' ? 'macos' : 'other']
    return HOTKEY_ACTIONS.map((action) => {
      const [combo, display] = table[action]
      return {
        target: { kind: 'action', action },
        binding: 'default',
        effective: combo,
        display,
        state: 'registered',
        reason_key: null,
      }
    })
  }

  function permissions(): PermissionSet {
    const mac = os === 'macos'
    return {
      accessibility: mac ? 'granted' : 'not_applicable',
      screen_recording: mac ? 'granted' : 'not_applicable',
      input_monitoring: mac ? 'not_determined' : 'not_applicable',
      lost_after_update: false,
    }
  }

  function update(): UpdateStatus {
    return { state: 'idle', version: null, notes: null, progress: null, error: null }
  }

  function runtime(): RuntimeInfo {
    return {
      version: '0.1.0',
      platform: platform(),
      has_key: {},
      keychain: 'available',
      hotkeys: hotkeyStatuses(),
      ext_triggers: { bobber: 'off', double_tap: 'off', mouse: 'off', reason_key: null },
      permissions: permissions(),
      clipboard_paused: false,
      update: update(),
      glossary_count: 0,
      settings_issue: null,
      orphan_keys: 0,
    }
  }

  function appearance(): AppearanceState {
    return {
      theme: boot.theme,
      accent: boot.accent || '#3B82F6',
      reduce_motion: boot.reduce_motion,
      reduce_transparency: boot.reduce_transparency,
      material: boot.material,
      font_size: boot.font_size,
      os,
      ui_language: boot.ui_language,
    }
  }

  function appMeta(): AppMeta {
    const home = os === 'windows' ? 'C:\\Users\\me' : os === 'macos' ? '/Users/me' : '/home/me'
    return {
      version: '0.1.0',
      platform: platform(),
      config_dir:
        os === 'macos'
          ? `${home}/Library/Application Support/wisp`
          : os === 'windows'
            ? `${home}\\AppData\\Roaming\\wisp`
            : `${home}/.config/wisp`,
      data_dir:
        os === 'macos'
          ? `${home}/Library/Application Support/wisp`
          : os === 'windows'
            ? `${home}\\AppData\\Local\\wisp`
            : `${home}/.local/share/wisp`,
      log_dir:
        os === 'macos'
          ? `${home}/Library/Logs/wisp`
          : os === 'windows'
            ? `${home}\\AppData\\Local\\wisp\\logs`
            : `${home}/.local/state/wisp/logs`,
      dev_mode: boot.dev,
    }
  }

  function keyboardAdvice(): KeyboardAdvice {
    return { has_altgr: false, multi_input_lang: false, recommend_family: null, qq_running: false }
  }

  function models(): ModelEntry[] {
    const caps = {
      temperature: null,
      max_tokens_field: null,
      thinking_param: null,
      effort_param: null,
      stream_options: null,
      include_obfuscation: null,
      anthropic_thinking_disabled: null,
      anthropic_efforts: null,
    }
    const entry = (
      id: string,
      vision: ModelEntry['vision'],
      reasoning: boolean,
      context: number,
    ): ModelEntry => ({
      id,
      label: null,
      vision,
      vision_source: vision === 'unknown' ? null : 'capability',
      reasoning,
      enabled: true,
      missing_upstream: false,
      deprecated: false,
      context_tokens: context,
      caps: { ...caps },
      reasoning_effort: null,
    })
    return [
      entry('gpt-5.4-mini', 'yes', false, 400_000),
      entry('gpt-5.4', 'yes', true, 400_000),
      entry('claude-haiku-4-5', 'yes', false, 200_000),
      entry('deepseek-chat', 'no', false, 128_000),
      entry('qwen3-vl-plus', 'yes', false, 262_144),
    ]
  }

  function provider(
    id: string,
    name: string,
    protocol: Provider['protocol'],
    base_url: string,
  ): Provider {
    return {
      id,
      name,
      protocol,
      base_url,
      headers: {},
      quirks: {
        thinking_off: 'none',
        merge_system_into_user: false,
        strip_think_tags: true,
        max_tokens_field: null,
        dual_auth_header: null,
        gemini_key_in_query: false,
      },
      models: [],
      timeouts: { connect_ms: 10_000, first_token_ms: 30_000, idle_ms: 60_000 },
      template_id: id,
    }
  }

  function providerTemplates(): ProviderTemplate[] {
    const t = (
      id: string,
      name: string,
      group: ProviderTemplate['group'],
      protocol: Provider['protocol'],
      base: string,
      key_required = true,
    ): ProviderTemplate => ({
      id,
      name_key: `settings.providers.template.${id}`,
      group,
      provider: provider(id, name, protocol, base),
      key_required,
      region_variants: [],
    })
    return [
      t('openai', 'OpenAI', 'openai', 'openai_chat', 'https://api.openai.com/v1'),
      t(
        'openai-responses',
        'OpenAI Responses',
        'openai',
        'openai_responses',
        'https://api.openai.com/v1',
      ),
      t('anthropic', 'Anthropic', 'anthropic', 'anthropic_messages', 'https://api.anthropic.com'),
      t('gemini', 'Google Gemini', 'gemini', 'gemini', 'https://generativelanguage.googleapis.com'),
      t('deepseek', 'DeepSeek', 'other', 'openai_chat', 'https://api.deepseek.com'),
      t('openrouter', 'OpenRouter', 'other', 'openai_chat', 'https://openrouter.ai/api/v1'),
      t('ollama', 'Ollama', 'local', 'openai_chat', 'http://localhost:11434/v1', false),
    ]
  }

  // ----- 剪贴板

  const app = (id: string, name: string, hue: number) => ({
    id,
    name,
    icon_url: appIcon(name, hue),
  })
  const apps = {
    wechat: app(os === 'macos' ? 'com.tencent.xinWeChat' : 'wechat.exe', '微信', 140),
    safari: app(
      os === 'macos' ? 'com.apple.Safari' : 'msedge.exe',
      os === 'macos' ? 'Safari' : 'Edge',
      205,
    ),
    code: app(os === 'macos' ? 'com.microsoft.VSCode' : 'code.exe', 'VS Code', 210),
    figma: app(os === 'macos' ? 'com.figma.Desktop' : 'figma.exe', 'Figma', 12),
    notes: app(
      os === 'macos' ? 'com.apple.Notes' : 'notepad.exe',
      os === 'macos' ? '备忘录' : 'Notepad',
      45,
    ),
    finder: app(
      os === 'macos' ? 'com.apple.finder' : 'explorer.exe',
      os === 'macos' ? 'Finder' : 'Explorer',
      215,
    ),
    slack: app(os === 'macos' ? 'com.tinyspeck.slackmacgap' : 'slack.exe', 'Slack', 300),
    mail: app(
      os === 'macos' ? 'com.apple.mail' : 'outlook.exe',
      os === 'macos' ? 'Mail' : 'Outlook',
      195,
    ),
    shot: app(
      os === 'macos' ? 'com.apple.screenshot.launcher' : 'snippingtool.exe',
      os === 'macos' ? '截屏' : 'Snipping Tool',
      280,
    ),
  }
  const docs =
    os === 'windows'
      ? 'C:\\Users\\me\\Documents\\'
      : os === 'macos'
        ? '/Users/me/Documents/'
        : '/home/me/Documents/'

  let nextId = 1
  const item = (
    partial: Partial<ClipItemMeta> & Pick<ClipItemMeta, 'kind' | 'preview'>,
  ): ClipItemMeta => {
    const id = nextId++
    const preview = partial.preview
    return {
      id,
      created_at: minutes(id * 7),
      last_used_at: minutes(id * 5),
      use_count: 1,
      pinned: false,
      source: null,
      char_count: preview.length,
      line_count: preview.split('\n').length,
      truncated: false,
      image: null,
      files: null,
      link: null,
      color: null,
      ...partial,
    }
  }

  function clipItems(): ClipItemMeta[] {
    nextId = 1
    return [
      item({
        kind: 'text',
        preview: '收货地址：上海市徐汇区漕溪北路 88 号 12 层 1203 室',
        pinned: true,
        use_count: 14,
        source: apps.wechat,
      }),
      item({
        kind: 'link',
        preview: 'https://github.com/yzlltyyh/wisp',
        link: 'https://github.com/yzlltyyh/wisp',
        pinned: true,
        use_count: 6,
        source: apps.safari,
      }),
      item({
        kind: 'code',
        preview: "const unlisten = await on('panel:prepare', handlePrepare)",
        source: apps.code,
      }),
      item({
        kind: 'text',
        preview: 'Latency budget: prepare ≤ 40 ms · first token p95 ≤ 1.2 s · idle CPU ≈ 0%',
        source: apps.notes,
      }),
      item({ kind: 'color', preview: '#3B82F6', color: '#3B82F6', source: apps.figma }),
      item({
        kind: 'image',
        preview: '',
        char_count: null,
        line_count: null,
        image: {
          width: 2560,
          height: 1440,
          bytes: 1_843_200,
          thumb_url: thumbnail(2560, 1440, 220),
        },
        source: apps.shot,
      }),
      item({
        kind: 'files',
        preview: '季度报告.pdf, design-v3.fig',
        char_count: null,
        line_count: null,
        files: [
          { path: `${docs}季度报告.pdf`, name: '季度报告.pdf', exists: true, size: 2_418_112 },
          { path: `${docs}design-v3.fig`, name: 'design-v3.fig', exists: true, size: 18_874_368 },
        ],
        source: apps.finder,
      }),
      item({
        kind: 'rich_text',
        preview: 'Wisp 让翻译跟手出现：选中、按键、译文就在光标旁。',
        source: apps.notes,
      }),
      item({
        kind: 'text',
        preview:
          'Design principle: every trigger gets immediate feedback, streaming output renders as it arrives, and motion serves perception rather than decoration. The panel never steals focus from the app you are working in…',
        char_count: 5320,
        line_count: 48,
        truncated: true,
        source: apps.safari,
      }),
      item({
        kind: 'link',
        preview: 'https://developer.apple.com/documentation/screencapturekit',
        link: 'https://developer.apple.com/documentation/screencapturekit',
        source: apps.safari,
      }),
      item({
        kind: 'text',
        preview: "Thanks for the quick turnaround — let's ship it on Friday.",
        source: apps.slack,
      }),
      item({
        kind: 'image',
        preview: '',
        char_count: null,
        line_count: null,
        image: {
          width: 4032,
          height: 3024,
          bytes: 3_604_480,
          thumb_url: thumbnail(4032, 3024, 32),
        },
        source: apps.wechat,
      }),
      item({
        kind: 'code',
        preview: 'SELECT id, preview FROM clips WHERE pinned = 1 ORDER BY last_used_at DESC;',
        source: apps.code,
      }),
      item({
        kind: 'text',
        preview: 'ありがとうございます。明日またご連絡いたします。',
        source: apps.mail,
      }),
    ]
  }

  function clipPage(
    items: readonly ClipItemMeta[],
    filter: ClipFilter = 'all',
    query: string | null = null,
    offset = 0,
    limit = 50,
  ): ClipPage {
    const matchesFilter = (it: ClipItemMeta) =>
      filter === 'all' ||
      it.kind === filter ||
      (filter === 'text' && (it.kind === 'rich_text' || it.kind === 'text'))
    const q = query?.trim().toLowerCase() ?? ''
    const hits: ClipHit[] = []
    for (const it of items) {
      if (!matchesFilter(it)) continue
      if (!q) {
        hits.push({ item: it, highlights: [] })
        continue
      }
      const hay = it.preview.toLowerCase()
      const at = hay.indexOf(q)
      if (at >= 0) hits.push({ item: it, highlights: [{ start: at, end: at + q.length }] })
    }
    // 置顶在前，其余按最近使用
    hits.sort(
      (a, b) =>
        Number(b.item.pinned) - Number(a.item.pinned) || b.item.last_used_at - a.item.last_used_at,
    )
    const page = hits.slice(offset, offset + limit)
    return {
      items: page,
      pinned_count: hits.filter((h) => h.item.pinned).length,
      next_offset: offset + limit < hits.length ? offset + limit : null,
      query: q ? query : null,
      filter,
    }
  }

  // ----- 面板载荷

  const payload = {
    translate(requestId: string | null = 'r-translate'): PanelPayload {
      return {
        kind: 'translate',
        source_text: SAMPLE.source,
        request_id: requestId,
        task: 'translate',
        style_id: 'natural',
        detected_lang: 'en',
        target_lang: 'zh-CN',
        dict_mode: false,
        long_text: null,
        not_ready: null,
        selection_method: os === 'windows' ? 'uia' : os === 'macos' ? 'ax' : 'primary',
      }
    },
    ocr(requestId: string | null = 'r-ocr'): PanelPayload {
      return {
        kind: 'ocr',
        capture_id: 'cap-1',
        request_id: requestId,
        mode: 'lines',
        thumb_url: thumbnail(1280, 480, 250),
        image_size: { width: 1280, height: 480 },
        target_lang: 'zh-CN',
        not_ready: null,
      }
    },
    clipboard(items: readonly ClipItemMeta[] = clipItems()): PanelPayload {
      return {
        kind: 'clipboard',
        page: clipPage(items),
        paused: false,
        hint_bar: true,
        paste_on_double_click: false,
      }
    },
    input(): PanelPayload {
      return { kind: 'input', initial_text: '', target_lang: null, hint: null }
    },
    toast(): PanelPayload {
      return { kind: 'toast', reason: 'copy_only', paste_key: `${mod}V` }
    },
  }

  /**
   * `apps_running`：正在运行的应用——与 Rust 同样按名称排序、按 id 去重、不含 Wisp；
   * 一部分和剪贴板条目的来源重合（选择器合并最近来源时去重），另有两个没有图标的应用（icon_url 为 null 的显示）
   */
  function runningApps(): SourceApp[] {
    const extra: SourceApp[] = [
      {
        id: os === 'macos' ? 'com.1password.1password' : '1password.exe',
        name: '1Password',
        icon_url: appIcon('1Password', 220),
      },
      {
        id: os === 'macos' ? 'com.apple.Terminal' : 'windowsterminal.exe',
        name: os === 'macos' ? '终端' : 'Terminal',
        icon_url: null,
      },
      { id: os === 'macos' ? 'md.obsidian' : 'obsidian.exe', name: 'Obsidian', icon_url: null },
    ]
    return [apps.safari, apps.code, apps.slack, apps.wechat, apps.notes, ...extra].sort((a, b) =>
      a.name.localeCompare(b.name),
    )
  }

  function pinState(): PinState {
    return {
      kind: 'translate',
      source_text: SAMPLE.source,
      result_text: SAMPLE.translation,
      task: 'translate',
      style_id: 'natural',
      target_lang: 'zh-CN',
      detected_lang: 'en',
      request_id: null,
      capture_id: null,
      ocr_text: null,
      ocr_mode: null,
      show_source: false,
      status_line: null,
    }
  }

  return {
    os,
    golden,
    goldenNames: () => [...goldenByName.keys()],
    platform,
    defaultSettings,
    settings,
    styles,
    hotkeyStatuses,
    permissions,
    update,
    runtime,
    appearance,
    appMeta,
    keyboardAdvice,
    models,
    providerTemplates,
    clipItems,
    clipPage,
    payload,
    pinState,
    runningApps,
  }
}

/** 应用图标占位：圆角方块 + 按名称首字选的几何标记（SVG data URL；只在 mock 中使用） */
export function appIcon(name: string, hue: number): string {
  // 只用几何图形、不放 <text>：浏览器第一次排版带文字的 SVG 图片文档要 13–14 ms（发生在插入 <img> 的任务里），
  // 会让流畅度场景出现与真实应用无关的长任务（真实应用的图标是 Rust 送来的 PNG）
  const code = Array.from(name)[0]?.codePointAt(0) ?? 0
  const marks = [
    '<circle cx="32" cy="32" r="12" fill="white"/>',
    '<rect x="21" y="21" width="22" height="22" rx="6" fill="white"/>',
    '<path d="M32 18 46 32 32 46 18 32Z" fill="white"/>',
    '<circle cx="32" cy="32" r="12" fill="none" stroke="white" stroke-width="6"/>',
  ]
  const mark = marks[code % marks.length]
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64" viewBox="0 0 64 64"><defs><linearGradient id="g" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="hsl(${hue} 80% 62%)"/><stop offset="1" stop-color="hsl(${hue} 72% 46%)"/></linearGradient></defs><rect x="4" y="4" width="56" height="56" rx="14" fill="url(#g)"/>${mark}</svg>`
  return `data:image/svg+xml,${encodeURIComponent(svg)}`
}

/** 缩略图占位：渐变 + 几块抽象内容（SVG data URL） */
export function thumbnail(width: number, height: number, hue: number): string {
  const w = 320
  const h = Math.round((w * height) / width)
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="${w}" height="${h}" viewBox="0 0 ${w} ${h}"><defs><linearGradient id="b" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="hsl(${hue} 70% 88%)"/><stop offset="1" stop-color="hsl(${(hue + 40) % 360} 70% 70%)"/></linearGradient></defs><rect width="${w}" height="${h}" fill="url(#b)"/><rect x="${w * 0.08}" y="${h * 0.14}" width="${w * 0.5}" height="${h * 0.1}" rx="4" fill="white" fill-opacity=".85"/><rect x="${w * 0.08}" y="${h * 0.32}" width="${w * 0.78}" height="${h * 0.06}" rx="3" fill="white" fill-opacity=".6"/><rect x="${w * 0.08}" y="${h * 0.44}" width="${w * 0.66}" height="${h * 0.06}" rx="3" fill="white" fill-opacity=".6"/><circle cx="${w * 0.8}" cy="${h * 0.72}" r="${Math.min(w, h) * 0.14}" fill="white" fill-opacity=".45"/></svg>`
  return `data:image/svg+xml,${encodeURIComponent(svg)}`
}

export function isMacOs(os: OsKind): boolean {
  return os === 'macos'
}
