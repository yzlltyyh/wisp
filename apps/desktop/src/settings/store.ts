/**
 * 设置窗口与引导窗口共用的状态。
 *
 * - 唯一数据源是 Rust 的 `SettingsSnapshot`：启动时 `settings_get`，之后随 `settings:changed`（含外部手改 settings.json 的
 *   热加载）整体替换；`hotkeys:changed` / `permissions:changed` / `update:status` 只更新对应的运行时字段。
 * - 写入一律经 `patchSettings`：先在本地乐观合并（界面即时响应，没有「保存」按钮），再发 `settings_patch`；
 *   最后一个在途补丁返回时以 Rust 的快照为准（夹紧、校验后的值），失败时回到服务器状态并提示。
 *   在途期间到达的 `settings:changed` 只取运行时与风格，不覆盖乐观值，避免输入框回跳。
 * - 不保存流式文本与大列表。
 */
import { create } from 'zustand'
import { api, onMany, WispIpcError } from '../shared/ipc'
import type {
  HotkeyStatus,
  PermissionSet,
  Settings,
  SettingsSnapshot,
  Style,
  UpdateStatus,
} from '../shared/ipc/types'
import { timing, ms } from '../shared/motion/tokens'
import { mergePatch, subtractPatch, type Patch } from './lib/patch'
import { closeWindowSafely, installFlushTriggers, registerFlusher, trackWrite } from './lib/writes'

export type NoticeTone = 'info' | 'success' | 'danger'

export interface Notice {
  id: number
  tone: NoticeTone
  /** 已本地化的文案 */
  text: string
}

export interface SettingsState {
  snap: SettingsSnapshot | null
  /** settings_get 失败时的错误（显示重试） */
  loadError: WispIpcError | null
  /** 在途的 settings_patch 个数 */
  pending: number
  /** 窗口底部的轻提示（保存失败、已导出……） */
  notice: Notice | null
  /** 本次会话里关掉的配置文件问题（`settings_issue.at_ms`）：切换分组不会让横幅重新出现，问题变化时才再出现 */
  dismissedIssueAt: number | null
}

const initial: SettingsState = {
  snap: null,
  loadError: null,
  pending: 0,
  notice: null,
  dismissedIssueAt: null,
}

export const useSettingsStore = create<SettingsState>()(() => initial)

const set = useSettingsStore.setState
const get = useSettingsStore.getState

let noticeSeq = 0
let noticeTimer: ReturnType<typeof setTimeout> | null = null
/** 自动消失的轻提示：剩余停留时间与这一段开始计时的时刻（悬停 / 聚焦时暂停，离开后继续） */
let noticeLeft = 0
let noticeSince = 0
let noticePaused = false

export interface NotifyOptions {
  /**
   * 不自动消失，直到用户关掉或被新的提示替换：错误（需要读完原因）与带路径的结果（「已导出到 …」）。
   * 默认：错误不自动消失，其余停留 `timing.noticeHold`（一句完整的说明，比面板的「已复制」长）
   */
  sticky?: boolean
}

function clearNoticeTimer(): void {
  if (noticeTimer !== null) clearTimeout(noticeTimer)
  noticeTimer = null
}

function runNoticeTimer(): void {
  clearNoticeTimer()
  if (noticePaused || noticeLeft <= 0) return
  noticeSince = Date.now()
  noticeTimer = setTimeout(() => {
    noticeTimer = null
    set({ notice: null })
  }, noticeLeft)
}

/** 显示一条轻提示（同一时刻只有一条，新的替换旧的） */
export function notify(text: string, tone: NoticeTone = 'info', options: NotifyOptions = {}): void {
  const sticky = options.sticky ?? tone === 'danger'
  noticeSeq += 1
  set({ notice: { id: noticeSeq, tone, text } })
  // 停留时长来自动效令牌（tokens.ts：停留时长不得写字面量）
  noticeLeft = sticky ? 0 : ms(timing.noticeHold)
  // 新提示是另一个元素：上一条的悬停状态不延续（否则指针没再移动时它永远不会消失）
  noticePaused = false
  runNoticeTimer()
}

/** 指针悬停或焦点在提示上时暂停计时，离开后继续剩下的时间 */
export function holdNotice(paused: boolean): void {
  if (paused === noticePaused) return
  noticePaused = paused
  if (paused) {
    if (noticeTimer !== null) noticeLeft = Math.max(1, noticeLeft - (Date.now() - noticeSince))
    clearNoticeTimer()
  } else {
    runNoticeTimer()
  }
}

export function dismissNotice(): void {
  clearNoticeTimer()
  noticeLeft = 0
  noticePaused = false
  set({ notice: null })
}

/** 关掉配置文件问题的横幅（本次会话内） */
export function dismissIssue(atMs: number): void {
  set({ dismissedIssueAt: atMs })
}

/** 第一次加载（或重试） */
export async function loadSettings(): Promise<void> {
  try {
    const snap = await api.settingsGet()
    set({ snap, loadError: null })
  } catch (error) {
    set({ loadError: WispIpcError.from('settings_get', error) })
  }
}

/** 直接替换快照（命令返回了新快照时，例如 provider_delete） */
export function applySnapshot(snap: SettingsSnapshot): void {
  if (get().pending > 0) {
    const current = get().snap
    set({ snap: current ? { ...snap, settings: current.settings } : snap })
  } else {
    set({ snap })
  }
}

/** 本地改写设置的某一部分（不发命令；测试与乐观更新用） */
function applyLocal(patch: Patch): void {
  const snap = get().snap
  if (!snap) return
  set({ snap: { ...snap, settings: mergePatch<Settings>(snap.settings, patch) } })
}

/** 保存失败的回调（由界面层提供本地化文案；store 不依赖 i18n） */
let onSaveError: (error: WispIpcError) => void = () => {}

export function setSaveErrorHandler(handler: (error: WispIpcError) => void): void {
  onSaveError = handler
}

/**
 * 写入设置（JSON Merge Patch）。乐观更新 → `settings_patch` → 以最后一个返回的快照为准。
 * 返回是否成功（调用方通常不需要关心，失败已经提示并回滚）。
 */
export async function patchSettings(patch: Patch): Promise<boolean> {
  dropOverlapping(patch)
  applyLocal(patch)
  set((s) => ({ pending: s.pending + 1 }))
  try {
    const snap = await trackWrite(api.settingsPatch(patch))
    const last = get().pending === 1
    set((s) => ({ pending: s.pending - 1, ...(last ? { snap } : null) }))
    return true
  } catch (error) {
    set((s) => ({ pending: s.pending - 1 }))
    onSaveError(WispIpcError.from('settings_patch', error))
    // 回到服务器状态（只在没有其他在途补丁时；否则等最后一个补丁的结果）
    if (get().pending === 0) await loadSettings()
    return false
  }
}

/**
 * 合并写入的队列：同一个 key 的补丁在停顿后只发最后一个（例如连续编辑同一个 Provider）。
 * `build`：发出时才计算补丁（整体替换的数组按发出那一刻的本地快照计算，期间被删除的条目不会被写回）；返回 null 表示不用发了
 */
interface Queued {
  local: Patch
  build: (() => Patch | null) | null
  timer: ReturnType<typeof setTimeout>
}
const queued = new Map<string, Queued>()

/** 合并写入的停顿（不是动效时长） */
export const QUEUE_DELAY_MS = 300
let queueDelay = QUEUE_DELAY_MS

/** 测试用：改合并写入的停顿（null 恢复默认），让断言不依赖墙钟时间 */
export function setQueueDelay(delayMs: number | null): void {
  queueDelay = delayMs ?? QUEUE_DELAY_MS
}

/**
 * 立即乐观更新（`local`），停顿 `delayMs` 后再发 `settings_patch`；同一 key 只发最后一个。排队期间计入 pending，
 * 期间到达的 `settings:changed` 不会把界面改回旧值。
 */
export function queuePatch(
  key: string,
  local: Patch,
  build: (() => Patch | null) | null = null,
  delayMs = queueDelay,
): void {
  applyLocal(local)
  const prev = queued.get(key)
  if (prev) clearTimeout(prev.timer)
  else set((s) => ({ pending: s.pending + 1 }))
  const timer = setTimeout(() => sendQueued(key), delayMs)
  queued.set(key, { local, build, timer })
}

function sendQueued(key: string): void {
  const item = queued.get(key)
  if (!item) return
  clearTimeout(item.timer)
  queued.delete(key)
  set((s) => ({ pending: s.pending - 1 }))
  const patch = item.build ? item.build() : item.local
  if (patch) void patchSettings(patch)
}

/**
 * 直接写入时，去掉排队补丁里同一路径的旧值（例如拖完自定义强调色 300 ms 内又点了预设色块：排队的自定义色不能在之后
 * 发出、把预设盖回去）。`build` 类的补丁在发出时按当时的本地快照计算，已经包含这次写入，不用处理
 */
function dropOverlapping(patch: Patch): void {
  for (const [key, item] of queued) {
    if (item.build) continue
    const rest = subtractPatch(item.local, patch)
    if (rest) queued.set(key, { ...item, local: rest })
    else dropQueued(key)
  }
}

/** 丢掉某个 key 还没发出的补丁（例如删除这个 Provider 之前：不能再把它写回来） */
export function dropQueued(key: string): void {
  const item = queued.get(key)
  if (!item) return
  clearTimeout(item.timer)
  queued.delete(key)
  set((s) => ({ pending: s.pending - 1 }))
}

/** 立即发出所有排队的补丁（窗口关闭前、测试） */
export function flushQueuedPatches(): void {
  // Map 在遍历中删除当前条目是安全的（已删除的不会再被访问）
  for (const key of queued.keys()) sendQueued(key)
}

// 关窗前的统一提交（writes.ts）：队列最后执行——文本草稿提交时可能再排进队列
registerFlusher(flushQueuedPatches, { final: true })

/** 用当前快照里的设置构造补丁（例如替换整个 providers 数组） */
export function currentSettings(): Settings | null {
  return get().snap?.settings ?? null
}

export function setStyles(styles: Style[]): void {
  const snap = get().snap
  if (snap) set({ snap: { ...snap, styles } })
}

export function setHotkeys(hotkeys: HotkeyStatus[]): void {
  const snap = get().snap
  if (snap) set({ snap: { ...snap, runtime: { ...snap.runtime, hotkeys } } })
}

export function setPermissions(permissions: PermissionSet): void {
  const snap = get().snap
  if (snap) set({ snap: { ...snap, runtime: { ...snap.runtime, permissions } } })
}

export function setUpdate(update: UpdateStatus): void {
  const snap = get().snap
  if (snap) set({ snap: { ...snap, runtime: { ...snap.runtime, update } } })
}

/** 本地记一下某个 Provider 是否已有 Key（secret_set / delete 成功后，等不及下一次 settings:changed） */
export function setHasKey(providerId: string, has: boolean): void {
  const snap = get().snap
  if (!snap) return
  set({
    snap: {
      ...snap,
      runtime: { ...snap.runtime, has_key: { ...snap.runtime.has_key, [providerId]: has } },
    },
  })
}

/**
 * 订阅 Rust 推送（窗口生命周期内一次）。返回取消函数，可在订阅完成前调用。
 */
export function startSettingsSync(): () => void {
  const uninstall = installFlushTriggers()
  const stop = onMany({
    'settings:changed': (snap) => applySnapshot(snap),
    'hotkeys:changed': (hotkeys) => setHotkeys(hotkeys),
    'permissions:changed': (p) => setPermissions(p),
    'update:status': (u) => setUpdate(u),
    // 原生关闭按钮（交通灯 / 标题栏 ×）：与 ⌘W 同一条收尾——提交延迟写入、等在途命令返回、放弃草稿 Key，再关窗
    // （Rust 已拦下这次关闭，1.5 s 内没关会照样关闭）
    'window:close_requested': () => void closeWindowSafely().catch(() => undefined),
  })
  return () => {
    uninstall()
    flushQueuedPatches()
    stop()
  }
}

/** 测试用：回到初始状态 */
export function resetSettingsStore(): void {
  clearNoticeTimer()
  noticeLeft = 0
  noticePaused = false
  for (const item of queued.values()) clearTimeout(item.timer)
  queued.clear()
  set(initial, true)
}

// ---- 选择器 ----

export function useSettings(): Settings | null {
  return useSettingsStore((s) => s.snap?.settings ?? null)
}

export function useRuntime() {
  return useSettingsStore((s) => s.snap?.runtime ?? null)
}

export function useStyles(): Style[] {
  return useSettingsStore((s) => s.snap?.styles ?? EMPTY_STYLES)
}

const EMPTY_STYLES: Style[] = []
