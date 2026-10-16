/**
 * mock 的默认命令处理函数：覆盖全部命令，带一份内存状态（设置、风格、剪贴板、权限、快捷键……），
 * 让每个视图的空壳都能跑通；各视图的场景在此基础上逐条覆写。
 */
import { longTextAck, longTextGate } from './longText'
import type { Boot } from '../boot'
import { windowKindOf } from '../boot'
import { tKey } from '../../i18n/i18n'
import type {
  ClipItemMeta,
  ConflictReport,
  GlossaryEntry,
  HotkeyStatus,
  HotkeyTarget,
  OsKind,
  OverlayInit,
  PasteMode,
  PasteOutcome,
  PermissionSet,
  Provider,
  SettingsSnapshot,
  Style,
  UpdateStatus,
  UrlPreview,
} from '../types'
import type { MockAssets } from './assets'
import type { MockBus } from './bus'
import type { MockDesktop } from './desktop'
import { HOTKEY_ACTIONS, SAMPLE, type MockFixtures } from './fixtures'
import {
  createOfflineState,
  installOfflineDefaults,
  offlineSummary,
  type MockOffline,
} from './localModels'
import type { MockRouter } from './router'
import { mockInspect, mockTransform } from './smart'
import { textStream, type MockStreams } from './streams'
import { createSyncState, installSyncDefaults, type MockSync } from './sync'
import { mockError } from './types'
import type { MockPanelWindow } from './window'

export interface MockState {
  settings: SettingsSnapshot['settings']
  styles: Style[]
  hasKey: Record<string, boolean>
  permissions: PermissionSet
  hotkeys: HotkeyStatus[]
  clipboard: ClipItemMeta[]
  clipboardPaused: boolean
  /** 列表顶部「收藏」分组的折叠状态（本机） */
  clipboardFavoritesCollapsed: boolean
  glossary: GlossaryEntry[]
  update: UpdateStatus
  onboardingDone: boolean
  /** 同步：状态与一台假的 WebDAV 服务器（sync.ts） */
  sync: MockSync
  /** 离线模型：设置页的快照与下载进度（localModels.ts） */
  offline: MockOffline
}

export function createState(f: MockFixtures): MockState {
  return {
    settings: f.settings(),
    styles: f.styles(),
    hasKey: {},
    permissions: f.permissions(),
    hotkeys: f.hotkeyStatuses(),
    clipboard: f.clipItems(),
    clipboardPaused: false,
    clipboardFavoritesCollapsed: false,
    glossary: [],
    update: f.update(),
    onboardingDone: false,
    sync: createSyncState(f),
    offline: createOfflineState(f.os),
  }
}

export interface DefaultsDeps {
  boot: Boot
  bus: MockBus
  router: MockRouter
  streams: MockStreams
  state: MockState
  fixtures: MockFixtures
  assets: MockAssets
  panel: MockPanelWindow | null
  desktop: MockDesktop | null
}

export function installDefaults(d: DefaultsDeps): void {
  const { boot, bus, router: r, streams, state, fixtures: f } = d

  const snapshot = (): SettingsSnapshot => ({
    settings: state.settings,
    styles: state.styles,
    runtime: {
      ...f.runtime(),
      has_key: state.hasKey,
      permissions: state.permissions,
      hotkeys: state.hotkeys,
      clipboard_paused: state.clipboardPaused,
      update: state.update,
      glossary_count: state.glossary.length,
      sync: state.sync.status,
      offline: offlineSummary(state.offline),
    },
  })
  const settingsChanged = () => {
    const s = snapshot()
    bus.emit('settings:changed', s)
    return s
  }
  const findClip = (id: number) => {
    const item = state.clipboard.find((c) => c.id === id)
    if (!item) throw mockError('not_found', `mock: clip ${id} not found`)
    return item
  }

  // ----- 面板 / 窗口
  r.handle('panel_ready', ({ session_id, content_height }) =>
    d.panel ? d.panel.ready(session_id, content_height) : { session_id, focused: false },
  )
  r.handle('panel_painted', ({ session_id }) => d.panel?.painted(session_id))
  r.handle('panel_resize', ({ session_id, content_height, spring }) =>
    d.panel?.resize(session_id, content_height, spring ?? 'gentle'),
  )
  r.handle('panel_set_width', ({ session_id, width }) => d.panel?.setWidth(session_id, width))
  r.handle('panel_hide', ({ session_id, reason }) => d.panel?.hide(session_id, reason))
  r.handle('panel_activate', ({ session_id }) => d.panel?.activate(session_id))
  r.handle('panel_drag_start', () => undefined)
  r.handle('panel_pin', ({ session_id }) => {
    d.panel?.hide(session_id, 'pin')
    return `mock-${session_id}`
  })
  r.handle('pinned_init', () => f.pinState())
  r.handle('pinned_ready', () => undefined)
  r.handle('pinned_set_on_top', () => undefined)
  r.handle('window_open', ({ target }) => {
    const label = target.kind
    const url = new URL(`/settings.html${location.search}`, location.origin)
    url.searchParams.set('label', label)
    url.searchParams.delete('scenario')
    // 深链：真实环境首次创建写入 Boot.route、已打开时发 window:navigate；mock 用同名标签页 + ?route= 近似
    const route = target.kind === 'settings' ? target.section : target.step
    if (route === null) url.searchParams.delete('route')
    else url.searchParams.set('route', String(route))
    const anchor = target.kind === 'settings' ? (target.anchor ?? null) : null
    if (anchor === null) url.searchParams.delete('anchor')
    else url.searchParams.set('anchor', anchor)
    if (boot.label === label) bus.emit('window:navigate', target)
    else open(url.toString(), `wisp-${label}`)
  })

  // ----- 流 / 编排
  r.handle('stream_attach', ({ request_id }, call) => {
    if (call.onEvent) streams.attach(request_id, call.onEvent)
  })
  r.handle('cancel', ({ request_id }) => streams.cancel(request_id))
  r.handle('translate_start', ({ req }, call) => {
    const long = longTextGate(req, state.settings.translate.long_text_threshold)
    if (long) return longTextAck(long, null)
    const id = streams.newRequestId()
    const text =
      req.task === 'translate'
        ? req.target_lang === 'en'
          ? SAMPLE.source
          : SAMPLE.translation
        : `${req.text}\n\n---\n- ${SAMPLE.translation}`
    streams.start(
      id,
      streams.takeQueued() ??
        textStream(text, {
          meta: { detected_source_lang: 'en', target_lang: req.target_lang ?? 'zh-CN' },
        }),
    )
    if (call.onEvent) streams.attach(id, call.onEvent)
    return { request_id: id, width: null }
  })
  r.handle('ocr_start', ({ req }, call) => {
    const id = streams.newRequestId()
    const text = req.mode === 'translate' ? SAMPLE.ocrTranslation : SAMPLE.ocr
    streams.start(id, streams.takeQueued() ?? textStream(text, { chunkChars: 3 }))
    if (call.onEvent) streams.attach(id, call.onEvent)
    return { request_id: id, width: null }
  })
  r.handle('capture_release', () => undefined)
  // 与 Rust 一致的前提：自动复制开着、调用方是面板（固定窗口里切换排版不带动剪贴板）。mock 没有系统剪贴板，
  // 视为期间没有别的复制；不记进剪贴板历史（mock 的历史只是界面数据）
  r.handle('ocr_recopy', ({ text }) => {
    if (text.length > 1024 * 1024) throw mockError('invalid_argument', 'text exceeds 1048576 bytes')
    return state.settings.ocr.auto_copy && windowKindOf(boot.label) === 'panel'
  })
  r.handle('trigger_action', ({ action, text, origin }) => {
    if (text !== null && action !== 'translate_selection' && action !== 'translate_input') {
      throw mockError(
        'invalid_argument',
        `text is only accepted for translate actions, got ${action}`,
      )
    }
    console.info(
      '[mock] trigger_action',
      action,
      origin,
      text === null ? null : `${text.length} chars`,
    )
  })
  // Windows 用真实形状的 MonitorId（含反斜杠），覆盖「id 不能原样进资源路径」这条约束
  r.handle('overlay_init', () => overlayInit(d, d.boot.os === 'windows' ? '\\\\.\\DISPLAY1' : '1'))
  r.handle('overlay_ready', () => undefined)
  r.handle('overlay_commit', ({ commit }) => ({
    fly_to: {
      x: commit.rect.x,
      y: Math.min(commit.rect.y + commit.rect.height + 12, innerHeight - 220),
      width: 440,
      height: 220,
    },
  }))
  r.handle('overlay_done', () => undefined)
  r.handle('overlay_cancel', () => undefined)
  r.handle('overlay_focus', () => undefined)

  // ----- 剪贴板
  r.handle('clipboard_recent', ({ offset, limit, filter }) =>
    f.clipPage(state.clipboard, filter, null, offset, limit),
  )
  r.handle('clipboard_search', ({ query, filter, limit }) =>
    f.clipPage(state.clipboard, filter, query, 0, limit),
  )
  r.handle('clipboard_get', ({ id }) => {
    const meta = state.clipboard.find((c) => c.id === id)
    if (!meta) return null
    return {
      meta,
      text: meta.kind === 'image' || meta.kind === 'files' ? null : meta.preview,
      has_html: meta.kind === 'rich_text',
      has_rtf: meta.kind === 'rich_text',
      image_url: meta.image?.thumb_url ?? null,
      files: meta.files,
    }
  })
  /** 粘贴之后：更新来源条目的最近使用时间、隐藏面板（与 Rust 的面板粘贴一致）；只复制时给出 copied */
  const pasted = (ids: readonly number[], mode: PasteMode): PasteOutcome => {
    for (const id of ids) {
      const item = findClip(id)
      item.last_used_at = Date.now()
      item.use_count += 1
      bus.emit('clipboard:changed', { change: 'touched', item })
    }
    if (d.panel?.currentSession != null) d.panel.hide(d.panel.currentSession, 'paste')
    return mode === 'copy_only' ? { type: 'copied', reason: 'copy_only' } : { type: 'pasted' }
  }
  r.handle('clipboard_paste', ({ id, mode }) => pasted([id], mode))
  r.handle('clipboard_paste_text', ({ text, from, mode }) => {
    if (text.length > 1024 * 1024) throw mockError('invalid_argument', 'text exceeds 1048576 bytes')
    return pasted(from === null ? [] : [from], mode)
  })
  r.handle('clipboard_paste_many', ({ ids, mode }) => {
    if (ids.length === 0) throw mockError('invalid_argument', 'ids must not be empty')
    return pasted(ids, mode)
  })
  r.handle('clipboard_inspect', ({ id }) => {
    const item = findClip(id)
    return mockInspect(
      item.kind,
      item.kind === 'image' || item.kind === 'files' ? null : item.preview,
    )
  })
  r.handle('text_inspect', ({ text }) => mockInspect('text', text))
  r.handle('text_transform', ({ text, transform }) => {
    if (text.length > 1024 * 1024) throw mockError('invalid_argument', 'text exceeds 1048576 bytes')
    return mockTransform(text, transform)
  })
  r.handle('clipboard_pin', ({ id, pinned }) => {
    const item = findClip(id)
    item.pinned = pinned
    // 与 Rust 一致：收藏去掉验证码的到期时间，取消收藏去掉名称
    if (pinned) delete item.expires_at
    else delete item.name
    return item
  })
  r.handle('clipboard_set_name', ({ id, name }) => {
    const item = findClip(id)
    const clean = name?.trim() ?? ''
    if (clean.length > 64) throw mockError('invalid_argument', 'name exceeds 64 characters')
    // 命名即收藏；清除名称不取消收藏
    if (clean) {
      item.name = clean
      item.pinned = true
      delete item.expires_at
    } else delete item.name
    bus.emit('clipboard:changed', { change: 'updated', item })
    return item
  })
  r.handle('clipboard_set_favorites_collapsed', ({ collapsed }) => {
    state.clipboardFavoritesCollapsed = collapsed
  })
  r.handle('clipboard_drag_data', ({ ids }) => {
    if (ids.length === 0) throw mockError('invalid_argument', 'ids must not be empty')
    const items = ids.map(findClip)
    const text = items
      .filter((it) => it.kind !== 'image')
      .map((it) =>
        it.kind === 'files' ? (it.files ?? []).map((file) => file.path).join('\n') : it.preview,
      )
      .join('\n')
    const one = items.length === 1 ? items[0] : undefined
    return {
      text,
      html: one?.kind === 'rich_text' ? `<p>${one.preview}</p>` : null,
      uri: one?.kind === 'link' ? one.preview : null,
    }
  })
  r.handle('clipboard_queue_start', ({ ids }) => {
    if (ids.length === 0) throw mockError('invalid_argument', 'ids must not be empty')
    const status = { active: ids.length > 1, index: 1, total: ids.length }
    pasted(ids.slice(0, 1), 'normal')
    bus.emit('clipboard:queue', status)
    return status
  })
  r.handle('clipboard_queue_cancel', () => {
    bus.emit('clipboard:queue', { active: false, index: 0, total: 0 })
  })
  r.handle('clipboard_float_open', () => {
    if (d.panel?.currentSession != null) d.panel.hide(d.panel.currentSession, 'pin')
  })
  r.handle('clipboard_float_close', () => undefined)
  r.handle('clipboard_float_ready', () => ({
    query: '',
    filter: 'all',
    selected_id: null,
    on_top: true,
    payload: {
      kind: 'clipboard',
      page: f.clipPage(state.clipboard),
      paused: state.clipboardPaused,
      hint_bar: true,
      paste_on_double_click: false,
      ...(state.clipboardFavoritesCollapsed ? { favorites_collapsed: true } : {}),
    },
  }))
  r.handle('clipboard_float_reveal', () => undefined)
  r.handle('clipboard_float_set_on_top', () => undefined)
  r.handle('clipboard_float_focus', () => undefined)
  r.handle('clipboard_float_hover', () => undefined)
  r.handle('clipboard_delete', ({ id, force }) => {
    const item = findClip(id)
    if (item.pinned && !force) throw mockError('conflict', 'pinned item requires force')
    state.clipboard = state.clipboard.filter((c) => c.id !== id)
    bus.emit('clipboard:changed', { change: 'removed', id })
  })
  r.handle('clipboard_clear', ({ keep_pinned }) => {
    const before = state.clipboard.length
    state.clipboard = keep_pinned ? state.clipboard.filter((c) => c.pinned) : []
    bus.emit('clipboard:changed', { change: 'cleared', keep_pinned })
    return before - state.clipboard.length
  })
  r.handle('clipboard_set_paused', ({ paused }) => {
    state.clipboardPaused = paused
    bus.emit('clipboard:changed', { change: 'paused', paused })
  })
  r.handle('clipboard_save_image', ({ id }) => {
    findClip(id)
    return f.os === 'windows' ? 'C:\\Users\\me\\Pictures\\wisp.png' : '~/Pictures/wisp.png'
  })
  r.handle('clipboard_reveal', ({ id }) => void findClip(id))
  r.handle('clipboard_open_link', ({ id }) => {
    const link = findClip(id).link
    if (link) open(link, '_blank', 'noopener')
  })
  r.handle('clipboard_send_to', ({ id }) => void findClip(id))
  r.handle('clipboard_copy_text', async ({ text }) => {
    if (text.length > 1024 * 1024) throw mockError('invalid_argument', 'text exceeds 1048576 bytes')
    // 浏览器 mock：尽力写系统剪贴板（失败不算错误，真实环境由 Rust 写入）
    await navigator.clipboard?.writeText(text).catch(() => undefined)
  })
  r.handle('ocr_history_list', () => [])
  r.handle('ocr_history_delete', () => undefined)
  r.handle('ocr_history_clear', () => undefined)
  r.handle('apps_running', async () => {
    // 真实环境在阻塞线程上枚举进程（几十毫秒）：给一点延迟，让选择器的加载态有机会出现
    await sleep(120)
    return f.runningApps()
  })

  // ----- 设置 / 风格 / 术语表
  r.handle('settings_get', snapshot)
  r.handle('settings_patch', ({ patch }) => {
    state.settings = mergePatch(state.settings, patch) as SettingsSnapshot['settings']
    return settingsChanged()
  })
  // 排除名单 / 按键映射逐项写入：与 Rust 同一规则（按系统与应用 id；已有的原位替换，没有就追加；移除不存在的也算成功）
  r.handle('excluded_app_upsert', ({ entry }) => {
    const app_id = entry.app_id.trim()
    if (!app_id) throw mockError('invalid_argument', 'mock: entry.app_id must not be empty')
    state.settings.clipboard.excluded_apps = upsertByApp(state.settings.clipboard.excluded_apps, {
      ...entry,
      app_id,
    })
    return settingsChanged()
  })
  r.handle('excluded_app_remove', ({ os, app_id }) => {
    state.settings.clipboard.excluded_apps = state.settings.clipboard.excluded_apps.filter(
      (a) => !(a.os === os && a.app_id === app_id.trim()),
    )
    return settingsChanged()
  })
  r.handle('app_keymap_upsert', ({ entry }) => {
    const app_id = entry.app_id.trim()
    if (!app_id) throw mockError('invalid_argument', 'mock: entry.app_id must not be empty')
    state.settings.hotkeys.app_keymaps = upsertByApp(state.settings.hotkeys.app_keymaps, {
      ...entry,
      app_id,
    })
    return settingsChanged()
  })
  r.handle('app_keymap_remove', ({ os, app_id }) => {
    state.settings.hotkeys.app_keymaps = state.settings.hotkeys.app_keymaps.filter(
      (k) => !(k.os === os && k.app_id === app_id.trim()),
    )
    return settingsChanged()
  })
  r.handle('styles_list', () => state.styles)
  r.handle('style_save', ({ style }) => {
    const i = state.styles.findIndex((s) => s.id === style.id)
    const saved = { ...style, modified: style.builtin }
    if (i >= 0) state.styles[i] = saved
    else state.styles.push(saved)
    return state.styles
  })
  r.handle('style_delete', ({ id }) => {
    state.styles = state.styles.filter((s) => s.builtin || s.id !== id)
    return state.styles
  })
  r.handle('style_reset', ({ id }) => {
    const original = f.styles().find((s) => s.id === id)
    if (original) state.styles = state.styles.map((s) => (s.id === id ? original : s))
    return state.styles
  })
  r.handle('styles_reorder', ({ ids }) => {
    const byId = new Map(state.styles.map((s) => [s.id, s]))
    const ordered = ids.flatMap((id) => {
      const s = byId.get(id)
      byId.delete(id)
      return s ? [s] : []
    })
    state.styles = [...ordered, ...byId.values()]
    return state.styles
  })
  r.handle('style_preview', ({ style, sample }) => ({
    system: style.system_template,
    user: `${sample}${style.tail_instruction ? `\n\n${style.tail_instruction}` : ''}`,
  }))
  r.handle('glossary_get', () => state.glossary)
  r.handle('glossary_set', ({ entries }) => {
    state.glossary = entries
  })
  // 逐条写入：与 Rust 同一规则（改了原文时占原条目的位置；同原文合并为一条；都没有就追加），并广播 glossary:changed
  r.handle('glossary_upsert', ({ entry, previous_source }) => {
    const source = entry.source.trim()
    if (!source) throw mockError('invalid_argument', 'mock: entry.source must not be empty')
    const next: GlossaryEntry = {
      source,
      target: entry.target.trim(),
      note: entry.note?.trim() || null,
    }
    const previous = previous_source?.trim() || null
    const out: GlossaryEntry[] = []
    let placed = false
    for (const e of state.glossary) {
      if (e.source === source || e.source === previous) {
        if (!placed) out.push(next)
        placed = true
        continue
      }
      out.push(e)
    }
    if (!placed) out.push(next)
    state.glossary = out
    bus.emit('glossary:changed', null)
  })
  r.handle('glossary_remove', ({ source }) => {
    const before = state.glossary.length
    state.glossary = state.glossary.filter((e) => e.source !== source.trim())
    if (state.glossary.length !== before) bus.emit('glossary:changed', null)
  })
  r.handle('glossary_import', () => 0)
  r.handle('glossary_export', () => null)

  // ----- 模型服务
  r.handle('provider_templates', () => f.providerTemplates())
  r.handle('provider_new', ({ template_id }) => {
    const template = f.providerTemplates().find((t) => t.id === template_id)
    if (!template) throw mockError('not_found', `mock: template ${template_id}`)
    const suffix = Math.random().toString(16).slice(2, 6)
    return { ...template.provider, id: `${template_id}-${suffix}`, template_id }
  })
  r.handle('provider_delete', ({ provider_id }) => {
    state.settings.providers = state.settings.providers.filter((p) => p.id !== provider_id)
    delete state.hasKey[provider_id]
    return settingsChanged()
  })
  // 按 id 写入单个服务：已被删除时 not_found（设置页提示「已在别处删除」），不把它加回来
  r.handle('provider_save', ({ provider }) => {
    if (!state.settings.providers.some((p) => p.id === provider.id))
      throw mockError('not_found', 'mock: provider')
    state.settings.providers = state.settings.providers.map((p) =>
      p.id === provider.id ? provider : p,
    )
    return settingsChanged()
  })
  r.handle('provider_add', ({ provider }) => {
    const exists = state.settings.providers.some((p) => p.id === provider.id)
    state.settings.providers = exists
      ? state.settings.providers.map((p) => (p.id === provider.id ? provider : p))
      : [...state.settings.providers, provider]
    return settingsChanged()
  })
  r.handle('providers_reorder', ({ ids }) => {
    const rest = [...state.settings.providers]
    const ordered = ids.flatMap((id) => {
      const i = rest.findIndex((p) => p.id === id)
      return i < 0 ? [] : rest.splice(i, 1)
    })
    state.settings.providers = [...ordered, ...rest]
    return settingsChanged()
  })
  r.handle('provider_test', async ({ provider }) => {
    await sleep(650)
    const url = previewUrl(provider, null)
    const probe = { ok: true, status: 200, elapsed_ms: 412, url: url.url, error: null }
    return {
      models: { ...probe, elapsed_ms: 236 },
      stream: probe,
      overall: 'ok',
      model_used: f.models()[0]?.id ?? null,
      // 与 provider_models 同一套合并（Rust 的 merge_models）：已有条目在前、保留用户设置
      fetched_models: [
        ...provider.models,
        ...f.models().filter((m) => !provider.models.some((x) => x.id === m.id)),
      ],
    }
  })
  r.handle('provider_models', async ({ provider }) => {
    await sleep(380)
    const known = new Set(provider.models.map((m) => m.id))
    return [...provider.models, ...f.models().filter((m) => !known.has(m.id))]
  })
  r.handle('llm_preview_url', ({ protocol, base_url, model_id }) =>
    previewUrl({ protocol, base_url }, model_id),
  )
  r.handle('secret_set', ({ provider_id }) => {
    state.hasKey[provider_id] = true
  })
  r.handle('secret_delete', ({ provider_id }) => {
    delete state.hasKey[provider_id]
  })
  r.handle('model_caps_reset', () => undefined)

  // ----- 应用
  r.handle('permissions_status', () => state.permissions)
  r.handle('permissions_request', ({ kind }) => {
    const key = kind
    state.permissions = { ...state.permissions, [key]: 'granted' }
    bus.emit('permissions:changed', state.permissions)
    return state.permissions
  })
  r.handle('permissions_open', () => undefined)
  r.handle('permissions_watch', () => undefined)
  r.handle('app_info', () => f.appMeta())
  r.handle('onboarding_complete', () => {
    state.onboardingDone = true
  })
  r.handle('appearance_get', () => f.appearance())
  r.handle('open_path', () => undefined)
  r.handle('open_url', ({ url }) => void open(url, '_blank', 'noopener'))
  r.handle('app_relaunch', () => location.reload())
  r.handle('app_quit', () => console.info('[mock] app_quit'))
  r.handle('diagnostics_export', () =>
    f.os === 'windows'
      ? 'C:\\Users\\me\\Desktop\\wisp-diagnostics.zip'
      : '~/Desktop/wisp-diagnostics.zip',
  )
  r.handle('update_check', () => {
    state.update = { ...state.update, state: 'checking' }
    setTimeout(() => {
      state.update = {
        state: 'up_to_date',
        version: null,
        notes: null,
        progress: null,
        error: null,
      }
      bus.emit('update:status', state.update)
    }, 900)
    return state.update
  })
  r.handle('update_install', () => undefined)
  r.handle('update_status', () => state.update)

  // ----- 快捷键
  const setHotkeys = (next: HotkeyStatus[]) => {
    state.hotkeys = next
    bus.emit('hotkeys:changed', next)
    return next
  }
  r.handle('hotkey_status', () => state.hotkeys)
  r.handle('hotkey_record_begin', () => undefined)
  r.handle('hotkey_record_end', () => undefined)
  // 与 Rust（hotkeys/conflicts.rs）一致：不与目标自己比较；self_duplicate 带上占用者的显示名 `{action}`（hotkeys/names.rs：
  // 动作取 hotkeys.action.<id>，风格取用户起的名字、内置风格取 styles.<id>）
  r.handle('hotkey_check', ({ target, combo }): ConflictReport => {
    const display = formatCombo(combo, f.os)
    const self = target ? targetKey(target) : null
    const holder = state.hotkeys.find((h) => h.effective === combo && targetKey(h.target) !== self)
    return {
      combo,
      display,
      conflicts: holder
        ? [
            {
              level: 'forbidden',
              source: 'self_duplicate',
              message_key: 'hotkeys.conflict.self_duplicate',
              params: { action: targetName(holder.target, state.styles) },
            },
          ]
        : [],
      acceptable: !holder,
    }
  })
  r.handle('hotkey_set', ({ target, binding }) => {
    const key = targetKey(target)
    const effective =
      binding === 'none' ? null : binding === 'default' ? defaultCombo(f, target) : binding
    const status: HotkeyStatus = {
      target,
      binding,
      effective,
      display: effective ? formatCombo(effective, f.os) : null,
      state: effective ? 'registered' : 'unbound',
      reason_key: null,
    }
    const others = state.hotkeys.filter((h) => targetKey(h.target) !== key)
    return setHotkeys([...others, status].sort(byAction))
  })
  r.handle('hotkey_reset_defaults', () => setHotkeys(f.hotkeyStatuses()))
  r.handle('hotkey_test_begin', ({ target }) => {
    setTimeout(() => bus.emit('hotkey:test_received', { target }), 1200)
  })
  r.handle('hotkey_test_end', () => undefined)
  r.handle('neighbor_apps', () => [])
  r.handle('keyboard_advice', () => f.keyboardAdvice())

  // ----- 同步
  installSyncDefaults(d)

  // ----- 离线模型
  installOfflineDefaults(d)
}

// ---------------------------------------------------------------------------

/**
 * 冻结帧路径，与 Rust `assets::capture_asset_path` 同形：`capture/<session>/<MonitorId 的 UTF-8 小写十六进制>/<variant>`。
 * 只有 mock 需要自己拼（真实路径由 Rust 经 `OverlayInit.image_url` / `half_url` 下发）
 */
function captureAssetPath(sessionId: number, monitorId: string, variant: 'half' | 'full'): string {
  const hex = Array.from(new TextEncoder().encode(monitorId), (b) =>
    b.toString(16).padStart(2, '0'),
  ).join('')
  return `capture/${sessionId}/${hex}/${variant}`
}

function overlayInit(d: DefaultsDeps, monitorId: string): OverlayInit {
  const mac = d.boot.os === 'macos'
  const w = innerWidth
  const h = innerHeight
  const scale = devicePixelRatio || 1
  const imageSize = { width: Math.round(w * scale), height: Math.round(h * scale) }
  const full = d.assets.rgbaFrame(
    captureAssetPath(1, monitorId, 'full'),
    imageSize.width,
    imageSize.height,
  )
  const half = mac
    ? null
    : d.assets.rgbaFrame(
        captureAssetPath(1, monitorId, 'half'),
        Math.round(imageSize.width / 2),
        Math.round(imageSize.height / 2),
      )
  return {
    session_id: 1,
    monitor: {
      id: monitorId,
      name: mac ? 'Built-in Retina Display' : 'DISPLAY1',
      physical_origin: { x: 0, y: 0 },
      physical_size: imageSize,
      scale_factor: scale,
      frame: { x: 0, y: 0, width: w, height: h },
      work_area: d.desktop?.workArea() ?? { x: 0, y: 0, width: w, height: h },
      is_primary: true,
    },
    image_url: full,
    image_size: imageSize,
    windows: d.desktop?.windowRects() ?? [],
    mode: 'ocr',
    window_detect: d.state.settings.ocr.window_detect,
    fly_animation: d.state.settings.ocr.fly_animation && !d.boot.reduce_motion,
    reduced_motion: d.boot.reduce_motion,
    frozen_native: mac,
    half_url: half,
    // 原生冻结帧：放大镜与飞入按需拉小块（与 Rust `assets::capture_region_base` 同形）
    region_url: mac
      ? d.assets.rgbaRegions(
          captureAssetPath(1, monitorId, 'full').replace(/\/full$/, '/region'),
          captureAssetPath(1, monitorId, 'full'),
        )
      : null,
  }
}

/**
 * URL 预览的简化规则（权威实现在 wisp-llm/url.rs）：末尾 # 表示强制使用原样 URL；PaddleOCR 的地址就是完整的接口地址，
 * 原样使用
 */
function previewUrl(
  p: Pick<Provider, 'protocol' | 'base_url'>,
  modelId: string | null,
): UrlPreview {
  const raw = p.base_url.trim()
  if (p.protocol === 'paddle_ocr') {
    return { method: 'POST', url: raw.replace(/\/+$/, ''), version_appended: false, forced: false }
  }
  if (raw.endsWith('#')) {
    return { method: 'POST', url: raw.slice(0, -1), version_appended: false, forced: true }
  }
  const base = raw.replace(/\/+$/, '')
  const hasVersion = /\/v\d+(beta\d*)?$/.test(base)
  const model = modelId ?? '{model}'
  const path: Record<Exclude<Provider['protocol'], 'paddle_ocr'>, [string, string]> = {
    openai_chat: ['/v1', '/chat/completions'],
    openai_responses: ['/v1', '/responses'],
    anthropic_messages: ['/v1', '/messages'],
    gemini: ['/v1beta', `/models/${model}:streamGenerateContent?alt=sse`],
  }
  const [version, suffix] = path[p.protocol]
  return {
    method: 'POST',
    url: `${base}${hasVersion ? '' : version}${suffix}`,
    version_appended: !hasVersion,
    forced: false,
  }
}

/** RFC 7396 JSON Merge Patch（数组整体替换） */
export function mergePatch(target: unknown, patch: unknown): unknown {
  if (!isPlainObject(patch)) return structuredClone(patch)
  const out: Record<string, unknown> = isPlainObject(target) ? { ...target } : {}
  for (const [key, value] of Object.entries(patch)) {
    if (value === null) delete out[key]
    else out[key] = mergePatch(out[key], value)
  }
  return out
}

function isPlainObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v)
}

const MAC_KEYS: Record<string, string> = { Control: '⌃', Alt: '⌥', Shift: '⇧', Super: '⌘' }
const PC_KEYS: Record<string, string> = {
  Control: 'Ctrl',
  Alt: 'Alt',
  Shift: 'Shift',
  Super: 'Win',
}

/** global-hotkey 规范串 → 显示串（权威实现在 shared/keys/formatHotkey.ts 与 Rust hotkeys::display） */
function formatCombo(combo: string, os: string): string {
  const parts = combo.split('+')
  const key = (parts.pop() ?? '').replace(/^Key|^Digit/, '')
  if (os === 'macos') return parts.map((m) => MAC_KEYS[m] ?? m).join('') + key
  return [...parts.map((m) => PC_KEYS[m] ?? m), key].join('+')
}

function targetKey(t: HotkeyTarget): string {
  return t.kind === 'action' ? `action:${t.action}` : `style:${t.style_id}`
}

/** 冲突提示里的目标名（同 Rust hotkeys/names.rs 的 target_name） */
function targetName(t: HotkeyTarget, styles: readonly Style[]): string {
  if (t.kind === 'action') return tKey(`hotkeys.action.${t.action}`) ?? t.action
  const named = styles.find((s) => s.id === t.style_id)?.name.trim()
  return named || (tKey(`styles.${t.style_id}`) ?? t.style_id)
}

function defaultCombo(f: MockFixtures, target: HotkeyTarget): string | null {
  if (target.kind !== 'action') return null
  return (
    f.hotkeyStatuses().find((h) => targetKey(h.target) === targetKey(target))?.effective ?? null
  )
}

function byAction(a: HotkeyStatus, b: HotkeyStatus): number {
  const rank = (h: HotkeyStatus) =>
    h.target.kind === 'action' ? HOTKEY_ACTIONS.indexOf(h.target.action) : HOTKEY_ACTIONS.length
  return rank(a) - rank(b)
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms))
}

/** 按（系统，应用 id）写入一项：已有的原位替换，没有就追加 */
function upsertByApp<T extends { os: OsKind; app_id: string }>(list: readonly T[], next: T): T[] {
  return list.some((x) => x.os === next.os && x.app_id === next.app_id)
    ? list.map((x) => (x.os === next.os && x.app_id === next.app_id ? next : x))
    : [...list, next]
}

export { windowKindOf }
