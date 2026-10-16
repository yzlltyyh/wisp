/**
 * 设置 / 引导测试的传输层替身：按命令名注册处理函数、记录每次调用、手动推送事件。
 * 默认快照取 wisp-types 生成的黄金样例（Rust 的 `Settings::default()` 与一份 RuntimeInfo）。
 */
import { vi } from 'vitest'
import { setTransport, WispIpcError, type Transport } from '../../shared/ipc'
import {
  LocalModelsSnapshot as GoldenOffline,
  RuntimeInfo as GoldenRuntime,
  Settings as GoldenSettings,
} from '../../shared/ipc/fixtures'
import { resetOfflineModels } from '../../shared/offline/store'
import type {
  AppKeymap,
  EventMap,
  EventName,
  ExcludedApp,
  HotkeyAction,
  HotkeyStatus,
  IpcErrorCode,
  ModelEntry,
  OsKind,
  Provider,
  RuntimeInfo,
  Settings,
  SettingsSnapshot,
  Style,
  Vision,
} from '../../shared/ipc/types'
import { mergePatch } from '../lib/patch'

type Handler = (args: Record<string, unknown>) => unknown

export interface Harness {
  calls: Array<[string, Record<string, unknown>]>
  handle(cmd: string, fn: Handler): void
  emit<K extends EventName>(event: K, payload: EventMap[K]): void
  callsOf(cmd: string): Array<Record<string, unknown>>
  close: ReturnType<typeof vi.fn<() => Promise<void>>>
  state: { snap: SettingsSnapshot }
}

export function ipcError(code: IpcErrorCode, message: string): WispIpcError {
  return new WispIpcError('test', { code, message, llm: null })
}

const ACTIONS: HotkeyAction[] = [
  'translate_selection',
  'translate_input',
  'ocr',
  'ocr_translate',
  'clipboard',
]

export function hotkeys(): HotkeyStatus[] {
  const combos = ['Alt+KeyD', 'Alt+Shift+KeyD', 'Alt+KeyS', 'Alt+Shift+KeyS', 'Alt+KeyV']
  return ACTIONS.map((action, i) => ({
    target: { kind: 'action', action },
    binding: 'default',
    effective: combos[i] ?? null,
    display: null,
    state: 'registered',
    reason_key: null,
  }))
}

export function entry(
  id: string,
  vision: Vision = 'yes',
  opts: Partial<ModelEntry> = {},
): ModelEntry {
  return {
    id,
    label: null,
    vision,
    vision_source: null,
    reasoning: false,
    enabled: true,
    missing_upstream: false,
    deprecated: false,
    context_tokens: null,
    caps: {
      temperature: null,
      max_tokens_field: null,
      thinking_param: null,
      effort_param: null,
      stream_options: null,
      include_obfuscation: null,
      anthropic_thinking_disabled: null,
      anthropic_efforts: null,
    },
    reasoning_effort: null,
    ...opts,
  }
}

export function provider(id: string, opts: Partial<Provider> = {}): Provider {
  return {
    id,
    name: id,
    protocol: 'openai_chat',
    base_url: `https://${id}.example.com/v1`,
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
    ...opts,
  }
}

export function style(id: string, opts: Partial<Style> = {}): Style {
  return {
    id,
    name: '',
    builtin: true,
    system_template: `${id} template {{source_lang}} → {{target_lang}}`,
    tail_instruction: 'Reply with the translation only.',
    temperature: 0.3,
    output: 'plain_text',
    dictionary_mode: true,
    model_ref: null,
    hotkey: null,
    modified: false,
    ...opts,
  }
}

export function snapshot(
  settings: Partial<Settings> = {},
  runtime: Partial<RuntimeInfo> = {},
  styles: Style[] = ['natural', 'technical', 'academic', 'casual', 'learn', 'literal'].map((id) =>
    style(id),
  ),
): SettingsSnapshot {
  const base = structuredClone(GoldenSettings) as Settings
  return {
    settings: { ...base, ...settings },
    styles,
    runtime: {
      ...(structuredClone(GoldenRuntime) as RuntimeInfo),
      has_key: {},
      hotkeys: hotkeys(),
      settings_issue: null,
      orphan_keys: 0,
      keychain: 'available',
      ...runtime,
    },
  }
}

/**
 * 安装替身传输层；`settings_get` / `settings_patch`、按 id 写服务的 `provider_save` / `provider_add`、按（系统，应用 id）
 * 写排除名单与按键映射的 `excluded_app_*` / `app_keymap_*` 默认按内存快照工作（`provider_save` 找不到这个服务时与 Rust
 * 一样报 not_found）
 */
export function installHarness(initial: SettingsSnapshot = snapshot()): Harness {
  const handlers = new Map<string, Handler>()
  const listeners = new Map<string, Set<(p: unknown) => void>>()
  const calls: Array<[string, Record<string, unknown>]> = []
  const state = { snap: initial }
  const close = vi.fn<() => Promise<void>>(async () => {})
  handlers.set('settings_get', () => structuredClone(state.snap))
  handlers.set('settings_patch', ({ patch }) => {
    state.snap = { ...state.snap, settings: mergePatch(state.snap.settings, patch) }
    return structuredClone(state.snap)
  })
  handlers.set('provider_save', ({ provider: saved }) => {
    const p = saved as Provider
    const providers = state.snap.settings.providers
    if (!providers.some((x) => x.id === p.id))
      throw { code: 'not_found', message: 'provider', llm: null }
    state.snap = {
      ...state.snap,
      settings: {
        ...state.snap.settings,
        providers: providers.map((x) => (x.id === p.id ? p : x)),
      },
    }
    return structuredClone(state.snap)
  })
  handlers.set('provider_add', ({ provider: added }) => {
    const p = added as Provider
    const providers = state.snap.settings.providers
    state.snap = {
      ...state.snap,
      settings: {
        ...state.snap.settings,
        providers: providers.some((x) => x.id === p.id)
          ? providers.map((x) => (x.id === p.id ? p : x))
          : [...providers, p],
      },
    }
    return structuredClone(state.snap)
  })
  type AppEntry = { os: OsKind; app_id: string }
  const sameApp = (a: AppEntry, os: unknown, appId: unknown) => a.os === os && a.app_id === appId
  const upsertApp = <T extends AppEntry>(list: T[], next: T): T[] =>
    list.some((x) => sameApp(x, next.os, next.app_id))
      ? list.map((x) => (sameApp(x, next.os, next.app_id) ? next : x))
      : [...list, next]
  const writeLists = (change: (s: Settings) => Settings) => {
    state.snap = { ...state.snap, settings: change(state.snap.settings) }
    return structuredClone(state.snap)
  }
  handlers.set('excluded_app_upsert', ({ entry: app }) =>
    writeLists((s) => ({
      ...s,
      clipboard: {
        ...s.clipboard,
        excluded_apps: upsertApp(s.clipboard.excluded_apps, app as ExcludedApp),
      },
    })),
  )
  handlers.set('excluded_app_remove', ({ os, app_id }) =>
    writeLists((s) => ({
      ...s,
      clipboard: {
        ...s.clipboard,
        excluded_apps: s.clipboard.excluded_apps.filter((a) => !sameApp(a, os, app_id)),
      },
    })),
  )
  handlers.set('app_keymap_upsert', ({ entry: keymap }) =>
    writeLists((s) => ({
      ...s,
      hotkeys: { ...s.hotkeys, app_keymaps: upsertApp(s.hotkeys.app_keymaps, keymap as AppKeymap) },
    })),
  )
  handlers.set('app_keymap_remove', ({ os, app_id }) =>
    writeLists((s) => ({
      ...s,
      hotkeys: {
        ...s.hotkeys,
        app_keymaps: s.hotkeys.app_keymaps.filter((k) => !sameApp(k, os, app_id)),
      },
    })),
  )
  handlers.set('ocr_history_list', () => [])
  // 离线模型：默认取黄金样例（下载中 / 已安装各有一项）；快照在模块级缓存，每次安装替身时清空
  resetOfflineModels()
  handlers.set('offline_models_get', () => structuredClone(GoldenOffline))
  const transport: Transport = {
    kind: 'mock',
    invoke: async <T>(cmd: string, args?: Record<string, unknown>) => {
      calls.push([cmd, structuredClone(args ?? {})])
      const h = handlers.get(cmd)
      if (!h) return undefined as T
      try {
        return (await h(args ?? {})) as T
      } catch (error) {
        throw WispIpcError.from(cmd, error)
      }
    },
    openStream: () => Promise.reject(new Error('unused')),
    listen: (event, cb) => {
      const set = listeners.get(event) ?? new Set()
      set.add(cb as (p: unknown) => void)
      listeners.set(event, set)
      return Promise.resolve(() => set.delete(cb as (p: unknown) => void))
    },
    window: {
      label: 'settings',
      startDragging: async () => {},
      close,
    },
    protocolUrl: (p) => p,
  }
  setTransport(transport)
  return {
    calls,
    state,
    close,
    handle: (cmd, fn) => handlers.set(cmd, fn),
    emit: (event, payload) => {
      for (const cb of listeners.get(event) ?? []) cb(payload)
    },
    callsOf: (cmd) => calls.filter(([c]) => c === cmd).map(([, a]) => a),
  }
}
