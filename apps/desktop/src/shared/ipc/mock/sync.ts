/**
 * mock 的同步：一台假的 WebDAV 服务器（有没有同步数据、同步密码、设备、是否属于本机、要模拟的失败）+ 同步命令的
 * 默认处理函数 + 设置窗口「同步」分组的场景（未连接 / 测试中 / 服务器无数据 / 已有数据 / 可恢复 / 已连接 / 同步中带进度 /
 * 每一种错误）。行为按 Rust 的契约近似：测试连接的传输失败放进报告；地址不合规、同步密码太短或与账号相同抛
 * `invalid_argument`，同步密码不对抛 `conflict`（message 都是 `errors.sync.*` 文案 key）；连接后在后台带进度上传 / 下载。
 * 应用密码留空（`password: null`）时用本机保存的那一份：连接后保存，断开时删除；清除本机正在同步的服务器时保留
 * （之后在原地设置新的同步密码，不必重新输入应用密码），找不到时测试连接报告 `credentials_missing`。
 */
import type {
  OsKind,
  SettingsSnapshot,
  SyncAccount,
  SyncDevice,
  SyncErrorKind,
  SyncStatus,
  SyncVaultInfo,
} from '../types'
import type { DefaultsDeps } from './defaults'
import type { MockFixtures } from './fixtures'
import type { MockHandle } from './index'
import type { MockContext, Scenario } from './types'
import { mockError } from './types'

export interface MockSyncVault {
  passphrase: string
  /** 其他设备（本机连接后自动加入） */
  devices: SyncDevice[]
  last_modified_ms: number | null
  /** false：由更新版本的 Wisp 写出 */
  supported: boolean
}

export interface MockSyncServer {
  vault: MockSyncVault | null
  /** 本机保存的同步状态属于服务器上的这份数据（重新连接即可继续） */
  restorable: boolean
  /** 测试连接与同步时模拟的失败；null = 正常 */
  fail: { kind: SyncErrorKind; detail: string | null } | null
  /** 测试连接的耗时（ms）；Infinity = 一直不返回（停在「测试中」） */
  probeMs: number
  connectMs: number
}

/** 本机保存的应用密码（Rust 存在系统凭据存储里；只对同一个地址与账号有效） */
export interface MockSyncSaved {
  server_url: string
  username: string
  password: string
}

export interface MockSync {
  status: SyncStatus
  server: MockSyncServer
  device: SyncDevice
  saved: MockSyncSaved | null
}

const MINUTE = 60_000
const DAY = 24 * 60 * MINUTE
/** 后台上传 / 下载每一步的间隔（模拟网络，不是动效时长） */
const TICK_MS = 350
const TICKS = 8

const DEVICE_NAMES: Record<OsKind, string> = {
  macos: 'MacBook Air',
  windows: 'DESKTOP-7Q2K',
  linux: 'thinkpad-x1',
}

export function createSyncState(f: MockFixtures): MockSync {
  return {
    status: f.syncOff(),
    server: { vault: null, restorable: false, fail: null, probeMs: 700, connectMs: 900 },
    device: {
      id: '3f9a0c1d2e4b5a69',
      name: DEVICE_NAMES[f.os],
      os: f.os,
      app_version: '0.2.0',
      last_seen_ms: Date.now(),
      this_device: true,
    },
    saved: null,
  }
}

/** 其他设备（场景用） */
export function otherDevices(now = Date.now()): SyncDevice[] {
  return [
    {
      id: '9c41d07be2a35f18',
      name: 'Office PC',
      os: 'windows',
      app_version: '0.2.0',
      last_seen_ms: now - 3 * DAY,
      this_device: false,
    },
    {
      id: '5be0a9f7c3d21486',
      name: 'Home Mac mini',
      os: 'macos',
      app_version: '0.1.1',
      last_seen_ms: now - 40 * MINUTE,
      this_device: false,
    },
  ]
}

/** 与 Rust 的账号校验同一套规则的简化版：只允许 http(s)、不带账号 / 查询串 / 片段，http 只给局域网地址 */
function checkAccount(account: SyncAccount): void {
  let u: URL
  try {
    u = new URL(account.server_url)
  } catch {
    throw mockError('invalid_argument', 'errors.sync.invalid_url')
  }
  const raw = account.server_url
  if (
    (u.protocol !== 'https:' && u.protocol !== 'http:') ||
    u.username ||
    raw.includes('?') ||
    raw.includes('#')
  )
    throw mockError('invalid_argument', 'errors.sync.invalid_url')
  const local =
    /^(localhost|127\.|10\.|192\.168\.|169\.254\.|172\.(1[6-9]|2\d|3[01])\.)/.test(u.hostname) ||
    u.hostname.endsWith('.local') ||
    !u.hostname.includes('.')
  if (u.protocol === 'http:' && !local)
    throw mockError('invalid_argument', 'errors.sync.https_required')
}

const BASE32 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567'

/** 24 个 base32 字符，每 4 个一组用「-」连接（与 Rust 生成的形状一致） */
function randomPassphrase(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(24))
  const chars = Array.from(bytes, (b) => BASE32[b % 32]).join('')
  return chars.match(/.{4}/g)?.join('-') ?? chars
}

function wait(ms: number): Promise<void> {
  if (ms === Infinity) return new Promise<void>(() => {})
  return new Promise((resolve) => setTimeout(resolve, ms))
}

export function installSyncDefaults(d: DefaultsDeps): void {
  const { bus, router: r, state } = d
  const sync = state.sync
  const server = sync.server
  /** 后台同步的代次：断开 / 清除之后，还在走的进度不再写回 */
  let generation = 0

  const emit = (next: SyncStatus) => {
    sync.status = next
    bus.emit('sync:status', next)
  }
  /** 设置改了：按当前（可能被场景覆写的）settings_get 广播一份快照 */
  const settingsChanged = async () => {
    const snap = (await r.dispatch('settings_get', {})) as SettingsSnapshot
    bus.emit('settings:changed', snap)
  }
  const devices = (): SyncDevice[] =>
    server.vault ? [{ ...sync.device, last_seen_ms: Date.now() }, ...server.vault.devices] : []
  const vaultInfo = (): SyncVaultInfo | null =>
    server.vault
      ? {
          supported: server.vault.supported,
          device_count: server.vault.devices.length + (server.restorable ? 1 : 0),
          last_modified_ms: server.vault.last_modified_ms,
          restorable: server.restorable,
        }
      : null
  const idle = (): SyncStatus => ({
    phase: 'idle',
    last_ok_ms: Date.now(),
    pending: false,
    error: null,
    transient: false,
    error_detail: null,
    progress: null,
    device_count: devices().length,
  })
  const failed = (kind: SyncErrorKind, detail: string | null): SyncStatus => ({
    ...sync.status,
    phase: 'error',
    error: kind,
    transient: kind === 'network' || kind === 'server' || kind === 'internal',
    error_detail: detail,
    progress: null,
  })

  /** 后台上传 / 下载 `total` 条，按固定间隔推进度，最后回到空闲（或按场景失败） */
  const runInBackground = (total: number) => {
    const id = ++generation
    let done = 0
    emit({
      ...sync.status,
      phase: 'syncing',
      error: null,
      transient: false,
      error_detail: null,
      progress: total > 0 ? { done, total } : null,
    })
    const step = () => {
      if (id !== generation) return
      if (done >= total) {
        emit(server.fail ? failed(server.fail.kind, server.fail.detail) : idle())
        return
      }
      done = Math.min(total, done + Math.ceil(total / TICKS))
      emit({ ...sync.status, progress: { done, total } })
      setTimeout(step, TICK_MS)
    }
    setTimeout(step, TICK_MS)
  }

  const disconnect = async (keep: MockSyncSaved | null = null) => {
    generation += 1
    sync.saved = keep
    state.settings.sync = { ...state.settings.sync, enabled: false }
    emit(d.fixtures.syncOff())
    await settingsChanged()
  }
  /** 留空的应用密码 = 本机为这个地址与账号保存的那一份 */
  const storedPassword = (account: SyncAccount): string | null =>
    sync.saved &&
    sync.saved.server_url === account.server_url &&
    sync.saved.username === account.username
      ? sync.saved.password
      : null
  const missing = () => mockError('not_found', 'errors.sync.credentials_missing')

  r.handle('sync_status', () => sync.status)
  r.handle('sync_devices', () => (state.settings.sync.enabled ? devices() : []))
  r.handle('sync_probe', async ({ account, password }) => {
    checkAccount(account)
    if (password === null && storedPassword(account) === null)
      return { ok: false, error: 'credentials_missing', error_detail: null, vault: null }
    await wait(server.probeMs)
    if (server.fail)
      return { ok: false, error: server.fail.kind, error_detail: server.fail.detail, vault: null }
    return { ok: true, error: null, error_detail: null, vault: vaultInfo() }
  })
  r.handle('sync_connect', async ({ account, password, passphrase, content }) => {
    checkAccount(account)
    const appPassword = password ?? storedPassword(account)
    if (appPassword === null) throw missing()
    const fold = (s: string) => s.trim().toLowerCase()
    if ([...passphrase.trim()].length < 12)
      throw mockError('invalid_argument', 'errors.sync.passphrase_weak')
    const host = new URL(account.server_url).hostname
    const taken = [appPassword, account.username, host].filter((s) => s.trim() !== '')
    if (taken.some((s) => fold(s) === fold(passphrase)))
      throw mockError('invalid_argument', 'errors.sync.passphrase_reused')
    await wait(server.connectMs)
    if (server.fail) throw mockError('io', `sync: ${server.fail.kind}`)
    let outcome: 'created' | 'joined' | 'restored'
    if (!server.vault) {
      server.vault = { passphrase, devices: [], last_modified_ms: Date.now(), supported: true }
      outcome = 'created'
    } else if (server.vault.passphrase !== passphrase) {
      throw mockError('conflict', 'errors.sync.passphrase')
    } else {
      outcome = server.restorable ? 'restored' : 'joined'
    }
    server.restorable = true
    sync.saved = {
      server_url: account.server_url,
      username: account.username,
      password: appPassword,
    }
    state.settings.sync = {
      ...state.settings.sync,
      enabled: true,
      server_url: account.server_url,
      username: account.username,
      content,
    }
    sync.status = idle()
    await settingsChanged()
    // 配置类在返回前完成；剪贴板与识别历史在后台继续，状态行带进度
    if (outcome !== 'restored') runInBackground(outcome === 'joined' ? 240 : 60)
    return { outcome, status: sync.status }
  })
  r.handle('sync_now', () => {
    if (!state.settings.sync.enabled) throw mockError('not_ready', 'errors.sync.not_connected')
    if (sync.status.phase !== 'syncing') runInBackground(server.fail ? 0 : 12)
    return sync.status
  })
  r.handle('sync_disconnect', () => disconnect())
  r.handle('sync_reset_remote', async ({ account, password }) => {
    const current = state.settings.sync
    if (account) checkAccount(account)
    else if (!current.server_url || !current.username)
      throw mockError('not_ready', 'errors.sync.not_connected')
    const target = account ?? { server_url: current.server_url, username: current.username }
    const appPassword = password ?? storedPassword(target)
    if (appPassword === null) throw missing()
    await wait(server.connectMs)
    server.vault = null
    server.restorable = false
    // 清除的是本机正在同步的服务器（或本机还没连接）时本机随之断开；刚用过的应用密码在本机连着它、或本来就保存着
    // 它时留下，之后的测试连接与连接可以留空沿用
    const same = current.server_url === target.server_url && current.username === target.username
    if (!current.enabled || same) {
      const keep = same && (current.enabled || password === null)
      await disconnect(keep ? { ...target, password: appPassword } : null)
    }
  })
  r.handle('sync_generate_passphrase', () => randomPassphrase())
}

// ---------------------------------------------------------------------------
// 场景（设置窗口的「同步」分组；由 scenarios/settings.ts 收进它的场景表，id 以 `settings.sync` 开头）

type PageFn = (
  id: string,
  label: string,
  section: string,
  extra?: (ctx: MockContext, mock: MockHandle) => void,
) => Scenario

type ClickFn = (ctx: MockContext, selector: string, then?: () => void) => void

const NUTSTORE = 'https://dav.jianguoyun.com/dav/'
const ACCOUNT = 'me@example.com'
const APP_PASSWORD = 'app-password-0000'

/** 给受控输入框填值：走原生 setter + input 事件，React 的 onChange 才会收到 */
function fill(selector: string, value: string): void {
  const el = document.querySelector<HTMLInputElement>(selector)
  if (!el) return
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')?.set
  setter?.call(el, value)
  el.dispatchEvent(new Event('input', { bubbles: true }))
}

/** 停在连接子视图：预填地址与账号（断开后保留的那一份），挂载后填应用密码，按需再点「测试连接」 */
function openConnect(
  ctx: MockContext,
  mock: MockHandle,
  click: ClickFn,
  opts: { url?: string; test?: boolean; after?: () => void } = {},
): void {
  mock.state.settings.sync = {
    ...mock.state.settings.sync,
    server_url: opts.url ?? NUTSTORE,
    username: ACCOUNT,
  }
  // 要截到测试结果的场景：测试立即返回（停在「测试中」的场景自己把耗时设成 Infinity）
  if (mock.state.sync.server.probeMs !== Infinity) mock.state.sync.server.probeMs = 0
  click(ctx, '[data-testid="sync-open"]', () => {
    click(ctx, '[data-testid="sync-password"]', () => {
      fill('[data-testid="sync-password"]', APP_PASSWORD)
      if (opts.test !== false) click(ctx, '[data-testid="sync-test"]', opts.after)
    })
  })
}

/** 已连接：地址、账号、保存的应用密码与状态 */
function connected(mock: MockHandle, status: Partial<SyncStatus> = {}): void {
  const { state } = mock
  const now = Date.now()
  state.settings.sync = {
    ...state.settings.sync,
    enabled: true,
    server_url: NUTSTORE,
    username: ACCOUNT,
  }
  state.sync.saved = { server_url: NUTSTORE, username: ACCOUNT, password: APP_PASSWORD }
  state.sync.server.vault = {
    passphrase: 'correct-horse-battery',
    devices: otherDevices(now),
    last_modified_ms: now - 2 * MINUTE,
    supported: true,
  }
  state.sync.server.restorable = true
  state.sync.status = {
    phase: 'idle',
    last_ok_ms: now - 2 * MINUTE,
    pending: false,
    error: null,
    transient: false,
    error_detail: null,
    progress: null,
    device_count: 3,
    ...status,
  }
}

const ERROR_DETAILS: Partial<Record<SyncErrorKind, string>> = {
  network: 'timeout',
  auth: 'http 401',
  forbidden: 'http 403',
  not_found: 'http 404',
  quota: 'http 507',
  server: 'http 503',
  https_required: 'redirect',
  keychain: 'denied',
}

const ERROR_KINDS: readonly SyncErrorKind[] = [
  'network',
  'auth',
  'forbidden',
  'not_found',
  'quota',
  'server',
  'https_required',
  'passphrase',
  'vault_reset',
  'format_too_new',
  'credentials_missing',
  'keychain',
  'corrupt',
  'internal',
]

export function syncScenarios(page: PageFn, click: ClickFn): Scenario[] {
  const existing = (mock: MockHandle, restorable = false) => {
    mock.state.sync.server.vault = {
      passphrase: 'correct-horse-battery',
      devices: otherDevices(),
      last_modified_ms: Date.now() - 3 * DAY,
      supported: true,
    }
    mock.state.sync.server.restorable = restorable
  }
  return [
    page('sync', '设置 · 同步（未连接）', 'sync'),
    page('sync-connect', '设置 · 同步 · 连接（坚果云，未测试）', 'sync', (ctx, mock) =>
      openConnect(ctx, mock, click, { test: false }),
    ),
    page('sync-probing', '设置 · 同步 · 连接（测试中）', 'sync', (ctx, mock) => {
      mock.state.sync.server.probeMs = Infinity
      openConnect(ctx, mock, click)
    }),
    page(
      'sync-empty-server',
      '设置 · 同步 · 连接（服务器无数据：设置同步密码）',
      'sync',
      (ctx, mock) => openConnect(ctx, mock, click),
    ),
    page(
      'sync-existing',
      '设置 · 同步 · 连接（已有 2 台设备的数据：输入同步密码）',
      'sync',
      (ctx, mock) => {
        existing(mock)
        openConnect(ctx, mock, click)
      },
    ),
    page('sync-wrong-passphrase', '设置 · 同步 · 连接（同步密码不对）', 'sync', (ctx, mock) => {
      existing(mock)
      mock.state.sync.server.connectMs = 0
      openConnect(ctx, mock, click, {
        after: () =>
          click(ctx, '[data-testid="sync-pass"]', () => {
            fill('[data-testid="sync-pass"]', 'not-the-right-one')
            click(ctx, '[data-testid="sync-connect"]')
          }),
      })
    }),
    page('sync-restorable', '设置 · 同步 · 连接（可恢复：重新连接）', 'sync', (ctx, mock) => {
      existing(mock, true)
      openConnect(ctx, mock, click)
    }),
    page(
      'sync-probe-failed',
      '设置 · 同步 · 连接（测试失败：应用密码不对）',
      'sync',
      (ctx, mock) => {
        mock.state.sync.server.fail = { kind: 'auth', detail: 'http 401' }
        openConnect(ctx, mock, click)
      },
    ),
    page('sync-http', '设置 · 同步 · 连接（http 公网地址：就地提示）', 'sync', (ctx, mock) =>
      openConnect(ctx, mock, click, { url: 'http://dav.example.com/dav/', test: false }),
    ),
    page('sync-connected', '设置 · 同步（已连接，已同步）', 'sync', (_ctx, mock) =>
      connected(mock),
    ),
    page('sync-syncing', '设置 · 同步（同步中 · 120 / 1000）', 'sync', (_ctx, mock) =>
      connected(mock, { phase: 'syncing', pending: true, progress: { done: 120, total: 1000 } }),
    ),
    page('sync-pending', '设置 · 同步（有改动等待同步、自动同步关闭）', 'sync', (_ctx, mock) => {
      connected(mock, { pending: true })
      mock.state.settings.sync.auto = false
    }),
    page('sync-update', '设置 · 同步 · 更新密码（地址与账号只读）', 'sync', (ctx, mock) => {
      connected(mock)
      click(ctx, '[data-testid="sync-update"]')
    }),
    ...ERROR_KINDS.map((kind) =>
      page(`sync-error-${kind}`, `设置 · 同步（错误：${kind}）`, 'sync', (_ctx, mock) => {
        const transient = kind === 'network' || kind === 'server' || kind === 'internal'
        connected(mock, {
          phase: 'error',
          error: kind,
          transient,
          pending: true,
          error_detail: ERROR_DETAILS[kind] ?? null,
        })
        mock.state.sync.server.fail = { kind, detail: ERROR_DETAILS[kind] ?? null }
      }),
    ),
  ]
}
