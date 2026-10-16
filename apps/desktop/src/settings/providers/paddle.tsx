/**
 * PaddleOCR 服务的编辑器部件：按服务地址判断是哪一种接口，并在地址下方说明（与 `wisp-local` 的
 * `ocr::cloud::detect_api` 同一规则）：
 * - 路径以 `/api/v2/ocr/jobs` 结尾：AI Studio 的任务接口（提交后排队，处理完再取结果）；
 * - 以 `/ocr` 或 `/layout-parsing` 结尾：部署的服务（AI Studio 上部署的，或自托管的 PaddleX 服务），直接返回结果；
 * - 其他：无法识别，就地提示应该填哪种地址。
 * 公网地址写了 http 时优先提示要用 https（Rust 会拒绝：访问令牌与截图不能明文传输），不必等到测试连接才知道。
 */
import { CircleAlert, Server, ShieldAlert, Workflow } from 'lucide-react'
import { useT } from '../../shared/i18n/useT'
import { Icon } from '../../shared/ui'

export type PaddleApi = 'jobs' | 'serving' | 'unknown' | 'empty'

/** 没写协议时按 https 解析（与 Rust 一致）；解析不了时返回 null */
function parse(raw: string): URL | null {
  try {
    return new URL(/^[a-z][a-z0-9+.-]*:\/\//i.test(raw) ? raw : `https://${raw}`)
  } catch {
    return null
  }
}

export function paddleApiOf(url: string): PaddleApi {
  const raw = url.trim()
  if (!raw) return 'empty'
  const parsed = parse(raw)
  if (!parsed) return 'unknown'
  const p = parsed.pathname.replace(/\/+$/, '').toLowerCase()
  if (p.endsWith('/api/v2/ocr/jobs')) return 'jobs'
  if (p.endsWith('/ocr') || p.endsWith('/layout-parsing')) return 'serving'
  return 'unknown'
}

const IPV4 = /^(\d+)\.(\d+)\.(\d+)\.(\d+)$/
const LOCAL_SUFFIXES = ['.localhost', '.local', '.home.arpa', '.lan', '.internal']

/**
 * 本机或局域网的主机（与 `wisp-local` 的 `ocr::cloud::is_local_host` 同一规则）：回环、私有网段、链路本地、`localhost`、
 * `.local` 等家庭网络后缀，以及不带点的单段主机名。这类地址允许 http（自托管的 PaddleX 通常没有证书）
 */
export function isLocalHost(hostname: string): boolean {
  const v4 = IPV4.exec(hostname)
  if (v4) {
    const [a, b] = [Number(v4[1]), Number(v4[2])]
    return (
      a === 127 ||
      a === 10 ||
      (a === 172 && b >= 16 && b <= 31) ||
      (a === 192 && b === 168) ||
      (a === 169 && b === 254)
    )
  }
  if (hostname.startsWith('[')) {
    const ip = hostname.slice(1, -1)
    if (ip === '::1') return true
    // 第一段：`::` 开头时为 0；fc00::/7（唯一本地）与 fe80::/10（链路本地）
    const first = ip.startsWith('::') ? 0 : Number.parseInt(ip.split(':')[0] ?? '', 16)
    return (first & 0xfe00) === 0xfc00 || (first & 0xffc0) === 0xfe80
  }
  const d = hostname.replace(/\.+$/, '').toLowerCase()
  return d !== '' && (!d.includes('.') || LOCAL_SUFFIXES.some((s) => d.endsWith(s)))
}

/** 地址明确写了 http、主机又不在本机 / 局域网：Rust 会拒绝 */
export function paddleUrlInsecure(url: string): boolean {
  const parsed = parse(url.trim())
  return parsed !== null && parsed.protocol === 'http:' && !isLocalHost(parsed.hostname)
}

/** 服务地址下方的一行：识别出的接口类型，或无法识别 / 需要 https 时的提示 */
export function PaddleUrlNote({ url }: { url: string }) {
  const t = useT()
  const api = paddleApiOf(url)
  if (api === 'empty') return null
  const kind = paddleUrlInsecure(url) ? 'insecure' : api
  const icon =
    kind === 'jobs'
      ? Workflow
      : kind === 'serving'
        ? Server
        : kind === 'insecure'
          ? ShieldAlert
          : CircleAlert
  return (
    <p
      className="st-field-hint"
      data-tone={kind === 'unknown' || kind === 'insecure' ? 'warning' : undefined}
      data-testid="paddle-url-kind"
      data-kind={kind}
    >
      <Icon icon={icon} size={12} />
      <span>{t(`settings.providers.paddle.kind.${kind}`)}</span>
    </p>
  )
}
