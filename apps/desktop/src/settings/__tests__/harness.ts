/**
 * 设置 / 引导测试的传输层替身：按命令名注册处理函数、记录每次调用、手动推送事件。
 * 默认快照取 wisp-types 生成的黄金样例（Rust 的 `Settings::default()` 与一份 RuntimeInfo）。
 */
import { vi } from 'vitest'
import { setTransport, WispIpcError, type Transport } from '../../shared/ipc'
import { RuntimeInfo as GoldenRuntime, Settings as GoldenSettings } from '../../shared/ipc/fixtures'
import type {
  EventMap,
  EventName,
  HotkeyAction,
  HotkeyStatus,
  IpcErrorCode,
  ModelEntry,
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

/** 安装替身传输层；`settings_get` / `settings_patch` 默认按内存快照工作 */
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
  handlers.set('ocr_history_list', () => [])
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
