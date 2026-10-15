/**
 * 同步页的纯逻辑：服务预设、服务器地址的本地校验（与 Rust 的账号校验同一套规则，只用来就地提示，结论以 Rust 为准）、
 * 同步密码规则、状态行的归类、错误种类对应的操作，以及测试连接报告的分步归属。没有副作用，单独测试。
 */
import type {
  SyncErrorKind,
  SyncProbe,
  SyncProgress,
  SyncStatus,
  SyncVaultInfo,
} from '../../shared/ipc/types'

// ---------------------------------------------------------------------------
// 服务预设：只用来预填地址、提示应用密码在哪里申请；不影响连接本身

export type SyncPresetId = 'nutstore' | 'pan123' | 'nextcloud' | 'synology' | 'custom'

export interface SyncPreset {
  id: SyncPresetId
  /** 固定的地址（各家公开的 WebDAV 入口）；null = 因人而异，只给占位提示 */
  url: string | null
  /** 识别已填地址属于哪个预设 */
  match: (u: URL) => boolean
}

export const SYNC_PRESETS: readonly SyncPreset[] = [
  {
    id: 'nutstore',
    url: 'https://dav.jianguoyun.com/dav/',
    match: (u) => u.hostname === 'dav.jianguoyun.com',
  },
  {
    id: 'pan123',
    url: 'https://webdav.123pan.cn/webdav',
    match: (u) => u.hostname === 'webdav.123pan.cn',
  },
  { id: 'nextcloud', url: null, match: (u) => u.pathname.includes('/remote.php/') },
  // DSM 的 WebDAV Server 套件默认端口：http 5005 / https 5006
  { id: 'synology', url: null, match: (u) => u.port === '5005' || u.port === '5006' },
  { id: 'custom', url: null, match: () => false },
]

/** 群晖套件中心里 WebDAV 服务的名字（第三方界面上的原文，不翻译） */
export const SYNOLOGY_PACKAGE = 'WebDAV Server' // i18n-ignore

export function presetById(id: SyncPresetId): SyncPreset {
  return SYNC_PRESETS.find((p) => p.id === id) ?? (SYNC_PRESETS.at(-1) as SyncPreset)
}

/** 已填的地址属于哪个预设（打开表单时用）；还没填时不预选，认不出来就是「自定义」 */
export function presetFor(url: string): SyncPresetId | null {
  if (!url.trim()) return null
  const u = parseUrl(url)
  if (!u) return 'custom'
  return SYNC_PRESETS.find((p) => p.match(u))?.id ?? 'custom'
}

/**
 * 换预设后的地址：固定地址的预设直接填上；换到没有固定地址的预设时，原来是另一个预设的固定地址就清空
 * （用户自己填的地址保留，不替他删）
 */
export function urlForPreset(id: SyncPresetId, current: string): string {
  const preset = presetById(id)
  if (preset.url) return preset.url
  const fixed = SYNC_PRESETS.some((p) => p.url !== null && p.url === current.trim())
  return fixed ? '' : current
}

// ---------------------------------------------------------------------------
// 服务器地址

/** 与 wisp-types 的 `SYNC_MAX_URL_BYTES` 一致 */
export const SYNC_MAX_URL_BYTES = 1024

export type UrlCheck = 'empty' | 'ok' | 'invalid' | 'https_required'

function parseUrl(raw: string): URL | null {
  try {
    return new URL(raw.trim())
  } catch {
    return null
  }
}

const utf8 = new TextEncoder()

/**
 * 本地检查服务器地址：只允许 http / https；不能带账号、查询串与片段（`?` / `#` 哪怕为空也不行，与 Rust 的 url crate
 * 解析结果一致）；http 只允许局域网地址——其他情况下应用密码会以明文经过网络
 */
export function checkServerUrl(raw: string): UrlCheck {
  const s = raw.trim()
  if (!s) return 'empty'
  if (utf8.encode(s).length > SYNC_MAX_URL_BYTES) return 'invalid'
  const u = parseUrl(s)
  if (!u || (u.protocol !== 'https:' && u.protocol !== 'http:') || !u.hostname) return 'invalid'
  if (u.username || u.password || s.includes('?') || s.includes('#')) return 'invalid'
  if (u.protocol === 'http:' && !isLocalHost(u.hostname)) return 'https_required'
  return 'ok'
}

/**
 * 允许走 http 的主机：回环、私有与链路本地的 IP 字面量，`.local`，以及单标签主机名（局域网里的 `nas`、`localhost`）。
 * `hostname` 是 WHATWG URL 规范化之后的值（IPv4 已展开为点分十进制，IPv6 带方括号）
 */
export function isLocalHost(hostname: string): boolean {
  const host = hostname.toLowerCase()
  if (host.startsWith('[')) return isLocalIpv6(host.slice(1, -1))
  const v4 = /^(\d+)\.(\d+)\.(\d+)\.(\d+)$/.exec(host)
  if (v4) {
    const a = Number(v4[1])
    const b = Number(v4[2])
    return (
      a === 127 ||
      a === 10 ||
      (a === 172 && b >= 16 && b <= 31) ||
      (a === 192 && b === 168) ||
      (a === 169 && b === 254)
    )
  }
  if (host.endsWith('.local')) return true
  return !host.includes('.')
}

function isLocalIpv6(addr: string): boolean {
  if (addr === '::1') return true
  // 第一组为空（`::…`）时是 0：不是唯一本地或链路本地地址
  const first = Number.parseInt(addr.split(':')[0] || '0', 16)
  if (Number.isNaN(first)) return false
  // fc00::/7 唯一本地地址；fe80::/10 链路本地地址
  return (first & 0xfe00) === 0xfc00 || (first & 0xffc0) === 0xfe80
}

/** 地址里的主机（含端口），用于「主机 · 账号」一行；解析不了时原样返回 */
export function hostOf(url: string): string {
  return parseUrl(url)?.host ?? url.trim()
}

// ---------------------------------------------------------------------------
// 同步密码

/** 与 wisp-types 的 `SYNC_PASSPHRASE_MIN_CHARS` 一致 */
export const SYNC_PASSPHRASE_MIN_CHARS = 12

/** 交给 Rust 之前统一做 NFC 规范化：同一个密码在不同系统的输入法下可能是不同的码点序列 */
export function normalizePassphrase(s: string): string {
  return s.normalize('NFC')
}

/**
 * 字符数按 Unicode 码点计（去掉首尾空白、规范化之后），与 Rust 的检查一致。只用于检查：交给 Rust 的同步密码
 * 保持原样（密钥由原文派生，首尾的空白同样算数）
 */
export function passphraseChars(s: string): number {
  return [...normalizePassphrase(s.trim())].length
}

export type PassphraseIssue = 'short' | 'reused'

/**
 * 新建同步密码的规则：至少 12 个字符；不能与应用密码、账号、服务器主机名相同（忽略大小写与首尾空白）。
 * 与 Rust 的检查同一套比较方式，免得这里放行、连接时才被 Rust 拒绝
 */
export function passphraseIssue(
  pass: string,
  ctx: { password: string; username: string; url: string },
): PassphraseIssue | null {
  if (passphraseChars(pass) < SYNC_PASSPHRASE_MIN_CHARS) return 'short'
  const fold = (s: string) => normalizePassphrase(s.trim()).toLowerCase()
  const p = fold(pass)
  const host = parseUrl(ctx.url)?.hostname ?? ''
  const taken = [ctx.password, ctx.username, host].filter((s) => s.trim() !== '').map(fold)
  return taken.includes(p) ? 'reused' : null
}

// ---------------------------------------------------------------------------
// 测试连接报告

export type PassMode = 'create' | 'join' | 'restore' | 'unsupported'

/** 同步密码一节按服务器状态变形：没有数据 → 设置；已有 → 输入并合并；本机就是这份数据 → 重新连接 */
export function passModeOf(vault: SyncVaultInfo | null): PassMode {
  if (!vault) return 'create'
  if (!vault.supported) return 'unsupported'
  return vault.restorable ? 'restore' : 'join'
}

/** 失败落在哪一步：0 连接与登录、1 读写权限、2 服务器上的数据 */
export function failedStep(kind: SyncErrorKind | null): 0 | 1 | 2 {
  if (kind === 'forbidden' || kind === 'quota') return 1
  if (kind === 'format_too_new' || kind === 'corrupt' || kind === 'passphrase') return 2
  return 0
}

/** 报告里的数据一步能不能算通过（服务器数据由更新版本写出时不能继续） */
export function probeUsable(probe: SyncProbe): boolean {
  return probe.ok && (probe.vault === null || probe.vault.supported)
}

// ---------------------------------------------------------------------------
// 状态行

export type StatusView =
  | { kind: 'syncing'; progress: SyncProgress | null }
  | { kind: 'synced'; at: number }
  | { kind: 'pending' }
  | { kind: 'never' }
  /** 网络、服务器之类的暂时性失败：不标红，稍后由下一次活动自动重试 */
  | { kind: 'offline' }
  | { kind: 'problem'; error: SyncErrorKind; transient: boolean }

export function statusView(s: SyncStatus): StatusView {
  if (s.phase === 'syncing') return { kind: 'syncing', progress: s.progress }
  if (s.phase === 'error') {
    const error = s.error ?? 'internal'
    if (s.transient && (error === 'network' || error === 'server')) return { kind: 'offline' }
    return { kind: 'problem', error, transient: s.transient }
  }
  if (s.pending) return { kind: 'pending' }
  if (s.last_ok_ms !== null) return { kind: 'synced', at: s.last_ok_ms }
  return { kind: 'never' }
}

/** 进度条的比例（0–1）；总数未知时为 null（只显示转圈与文字） */
export function progressRatio(p: SyncProgress | null): number | null {
  if (!p || p.total <= 0) return null
  return Math.max(0, Math.min(1, p.done / p.total))
}

/** 需要用户处理的错误对应的操作：重新输入密码，或重新连接（服务器上的数据被清除后） */
export function errorAction(kind: SyncErrorKind): 'update' | 'reconnect' | null {
  if (kind === 'auth' || kind === 'passphrase' || kind === 'credentials_missing') return 'update'
  if (kind === 'vault_reset') return 'reconnect'
  return null
}
