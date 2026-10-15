/**
 * 设置快照的窗口内缓存：风格列表（chip 菜单、⌘1–9、输出格式）、服务与模型（切换模型、错误卡片里的服务名）、
 * 翻译设置（主语言 / 第二语言、默认风格、长文本阈值）。
 *
 * - 第一次被视图用到时才拉取（`settings_get`），之后随 `settings:changed` 原地更新——面板窗口常驻，下一次唤出时
 *   直接可用，不在 prepare 的同步路径上等 IPC；
 * - 只有一份快照（几 KB），不随会话增长；拉取失败时保持 null，下一次使用时重试；
 * - 视图在快照到达之前按内置默认值工作（`FALLBACK_STYLE_ORDER` 等），快照到达后只触发一次重渲染。
 */
import { useEffect, useSyncExternalStore } from 'react'
import { api, on } from '../../shared/ipc'
import type {
  Lang,
  ModelRef,
  OutputFormat,
  Provider,
  SettingsSnapshot,
  Style,
  TranslateSettings,
} from '../../shared/ipc/types'

/** 内置风格的固定顺序（与 Rust `BUILTIN_STYLE_IDS` 一致） */
export const BUILTIN_STYLE_IDS = [
  'natural',
  'technical',
  'academic',
  'casual',
  'learn',
  'literal',
] as const
export type BuiltinStyleId = (typeof BUILTIN_STYLE_IDS)[number]

/** 快照到达之前的内置风格输出格式与词典式开关（与 Rust `builtin_styles` 一致） */
const BUILTIN_OUTPUT: Record<BuiltinStyleId, { output: OutputFormat; dict: boolean }> = {
  natural: { output: 'plain_text', dict: true },
  technical: { output: 'markdown_lite', dict: true },
  academic: { output: 'plain_text', dict: true },
  casual: { output: 'plain_text', dict: true },
  learn: { output: 'markdown_lite', dict: true },
  literal: { output: 'plain_text', dict: false },
}

export function isBuiltinStyle(id: string): id is BuiltinStyleId {
  return (BUILTIN_STYLE_IDS as readonly string[]).includes(id)
}

/** 风格的精简视图（菜单与排版只需要这些） */
export interface StyleInfo {
  id: string
  builtin: boolean
  /** 自定义风格的名字；内置风格为空串，显示时取 `styles.<id>` */
  name: string
  output: OutputFormat
  dictionary: boolean
  hotkey: string | null
}

export interface ModelChoice {
  ref: ModelRef
  label: string
  providerName: string
}

let snapshot: SettingsSnapshot | null = null
let loading = false
let subscribed = false
const listeners = new Set<() => void>()

function set(next: SettingsSnapshot | null | undefined): void {
  // 测试里的占位传输层会返回 undefined：当作没有快照
  if (!next || typeof next !== 'object' || !('settings' in next)) return
  snapshot = next
  for (const l of listeners) l()
}

/** 确保已拉取并订阅变化（多次调用只做一次；失败后下一次调用重试） */
export function ensureCatalog(): void {
  if (!subscribed) {
    subscribed = true
    void on('settings:changed', set).catch(() => {
      subscribed = false
    })
  }
  if (snapshot || loading) return
  loading = true
  void api
    .settingsGet()
    .then(set, () => undefined)
    .finally(() => {
      loading = false
    })
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

function current(): SettingsSnapshot | null {
  return snapshot
}

/**
 * 当前设置快照（响应式；挂载后触发拉取——不在渲染里、也不在面板 prepare 的同步路径上发 IPC）。`fetch = false`：
 * 只读缓存、不拉取（翻译视图的 JIT 预热渲染）
 */
export function useCatalog(fetch = true): SettingsSnapshot | null {
  useEffect(() => {
    if (fetch && !snapshot) queueMicrotask(ensureCatalog)
  }, [fetch])
  return useSyncExternalStore(subscribe, current, current)
}

/** 测试用：清空缓存（换传输层之后） */
export function resetCatalog(next: SettingsSnapshot | null = null): void {
  snapshot = next
  loading = false
  subscribed = false
  for (const l of listeners) l()
}

/** 有序的风格列表：内置在前（固定顺序），自定义按设置里的顺序在后 */
export function styleList(s: SettingsSnapshot | null): StyleInfo[] {
  const styles: Style[] = s?.styles ?? []
  if (styles.length === 0) {
    return BUILTIN_STYLE_IDS.map((id) => ({
      id,
      builtin: true,
      name: '',
      output: BUILTIN_OUTPUT[id].output,
      dictionary: BUILTIN_OUTPUT[id].dict,
      hotkey: null,
    }))
  }
  const info = (st: Style): StyleInfo => ({
    id: st.id,
    builtin: st.builtin,
    name: st.name,
    output: st.output,
    dictionary: st.dictionary_mode,
    hotkey: st.hotkey,
  })
  const builtin = BUILTIN_STYLE_IDS.flatMap((id) => {
    const st = styles.find((x) => x.id === id)
    return st ? [info(st)] : []
  })
  const custom = styles.filter((st) => !isBuiltinStyle(st.id)).map(info)
  return [...builtin, ...custom]
}

/** 按 id 找风格；找不到（例如自定义风格已被删除）时按 natural 处理 */
export function styleOf(list: readonly StyleInfo[], id: string): StyleInfo {
  return (
    list.find((s) => s.id === id) ?? {
      id,
      builtin: isBuiltinStyle(id),
      name: '',
      output: isBuiltinStyle(id) ? BUILTIN_OUTPUT[id].output : 'plain_text',
      dictionary: isBuiltinStyle(id) ? BUILTIN_OUTPUT[id].dict : false,
      hotkey: null,
    }
  )
}

/** 翻译设置（快照还没到时用 v1 默认值：主语言中文、第二语言英文） */
export function translateSettings(
  s: SettingsSnapshot | null,
): Pick<
  TranslateSettings,
  'primary_lang' | 'secondary_lang' | 'default_style' | 'long_text_threshold'
> {
  const t = s?.settings.translate
  return {
    primary_lang: t?.primary_lang ?? ('zh-CN' satisfies Lang),
    secondary_lang: t?.secondary_lang ?? ('en' satisfies Lang),
    default_style: t?.default_style ?? 'natural',
    long_text_threshold: t?.long_text_threshold ?? 3000,
  }
}

/** 可选的模型（启用的模型，按服务分组）；模型 label 为空时显示 id */
export function modelChoices(s: SettingsSnapshot | null): ModelChoice[] {
  const providers: Provider[] = s?.settings.providers ?? []
  return providers.flatMap((p) =>
    p.models
      .filter((m) => m.enabled)
      .map((m) => ({
        ref: { provider_id: p.id, model_id: m.id },
        label: m.label || m.id,
        providerName: p.name || p.id,
      })),
  )
}

export function providerOf(s: SettingsSnapshot | null, id: string | null): Provider | null {
  if (!id) return null
  return s?.settings.providers.find((p) => p.id === id) ?? null
}

/** 官方端点的主机名：其余都视为兼容端点，错误卡片优先显示服务端原文 */
const OFFICIAL_HOSTS = [
  'api.openai.com',
  'api.anthropic.com',
  'generativelanguage.googleapis.com',
] as const

export function isCompatibleEndpoint(p: Provider | null): boolean {
  if (!p) return false
  const raw = p.base_url.replace(/#$/, '').trim()
  try {
    const host = new URL(raw.includes('://') ? raw : `https://${raw}`).hostname
    return !(OFFICIAL_HOSTS as readonly string[]).includes(host)
  } catch {
    return true
  }
}
