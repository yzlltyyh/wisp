/**
 * mock 的 `wisp://` 资源表（Rust 侧 assets.rs 的浏览器替身）。
 *
 * - `register(path, body, headers)`：Blob / 字符串 → object URL，`protocolUrl(path)` 返回它；
 * - `rgbaFrame(path, w, h)`：惰性生成的原始 RGBA 帧（Overlay 用），URL 形如 `http://wisp.localhost/<path>`，
 *   只能经 `fetch` 读取（mock 拦截 fetch，返回带 `x-wisp-width` / `x-wisp-height` 头的响应，与 Rust 一致）；
 * - `rgbaRegions(base, fullPath)`：`<base>/<x>/<y>/<w>/<h>[/<max_px>]` 的小块从对应的整屏帧里裁出（macOS 原生冻结帧的
 *   放大镜、低分辨率底图与飞入；面积超过 `max_px`（缺省约 4 MP）时与 Rust 一样等比降采样）；
 * - 未注册的路径返回一张占位图（SVG data URL）。
 */
import { RGBA_HEIGHT_HEADER, RGBA_WIDTH_HEADER } from '../url'

interface Entry {
  url: string
  headers: Record<string, string>
  blob?: Blob
  produce?: () => Promise<Blob>
}

const LAZY_ORIGIN = 'http://wisp.localhost/'

/** 与 Rust `assets::REGION_MAX_PX` 相同：更大的小块等比降采样 */
const REGION_MAX_PX = 4_000_000

export class MockAssets {
  private readonly byPath = new Map<string, Entry>()
  private readonly byUrl = new Map<string, Entry>()
  /** `region` 前缀 → 整屏帧的路径 */
  private readonly regionBases = new Map<string, string>()
  /** 整屏帧解码出的字节（每个前缀只读一次 Blob；真实应用里小块由 Rust 在页面主线程之外裁出，mock 也不该在每次请求时整帧拷贝） */
  private readonly regionPixels = new Map<string, Promise<Uint8Array>>()

  constructor() {
    const realFetch = window.fetch.bind(window)
    window.fetch = async (input, init) => {
      const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url
      const region = this.region(url)
      if (region) return region
      const entry = this.byUrl.get(url)
      if (!entry) return realFetch(input, init)
      const blob = entry.blob ?? (entry.blob = await entry.produce?.())
      return new Response(blob ?? null, {
        status: blob ? 200 : 404,
        headers: { 'cache-control': 'no-store', ...entry.headers },
      })
    }
  }

  register(path: string, body: Blob | string, headers: Record<string, string> = {}): string {
    const key = normalize(path)
    const blob = typeof body === 'string' ? new Blob([body], { type: guessMime(key) }) : body
    const old = this.byPath.get(key)
    if (old?.url.startsWith('blob:')) URL.revokeObjectURL(old.url)
    const entry: Entry = { url: URL.createObjectURL(blob), headers, blob }
    this.byPath.set(key, entry)
    this.byUrl.set(entry.url, entry)
    return entry.url
  }

  /** 惰性生成的 RGBA 帧（合成的「屏幕」：渐变 + 两块窗口 + 几行文字） */
  rgbaFrame(path: string, width: number, height: number): string {
    const key = normalize(path)
    const existing = this.byPath.get(key)
    if (existing) return existing.url
    const entry: Entry = {
      url: LAZY_ORIGIN + encodeURIComponent(key),
      headers: { [RGBA_WIDTH_HEADER]: String(width), [RGBA_HEIGHT_HEADER]: String(height) },
      produce: async () => new Blob([synthesizeScreen(width, height)]),
    }
    this.byPath.set(key, entry)
    this.byUrl.set(entry.url, entry)
    return entry.url
  }

  /** `<base>/<x>/<y>/<w>/<h>[/<max_px>]` 从 `fullPath` 的整屏帧里裁（见文件头）；返回 base */
  rgbaRegions(base: string, fullPath: string): string {
    this.regionBases.set(normalize(base), normalize(fullPath))
    return base
  }

  url(path: string): string {
    const key = normalize(path)
    const hit = this.byPath.get(key)?.url
    if (hit) return hit
    // 小块请求：交给 fetch 拦截按需裁出（见 rgbaRegions）
    const slash = key.replace(/(\/\d+){4,5}$/, '')
    if (slash !== key && this.regionBases.has(slash)) return LAZY_ORIGIN + encodeURIComponent(key)
    return placeholder(key)
  }

  /** 小块请求：裁出那一块（越界 404，与 Rust 一致）；不是小块请求时为 null */
  private region(url: string): Promise<Response> | null {
    if (!url.startsWith(LAZY_ORIGIN)) return null
    const path = normalize(url.slice(LAZY_ORIGIN.length))
    const m = /^(.*\/region)\/(\d+)\/(\d+)\/(\d+)\/(\d+)(?:\/(\d+))?$/.exec(path)
    const fullPath = m?.[1] ? this.regionBases.get(m[1]) : undefined
    const full = fullPath ? this.byPath.get(fullPath) : undefined
    if (!m || !full) return null
    const [x, y, w, h] = m.slice(2, 6).map(Number) as [number, number, number, number]
    const cap = m[6] === undefined ? REGION_MAX_PX : Number(m[6])
    return (async () => {
      const fw = Number(full.headers[RGBA_WIDTH_HEADER])
      const fh = Number(full.headers[RGBA_HEIGHT_HEADER])
      if (!w || !h || x + w > fw || y + h > fh || !(cap > 0 && cap <= REGION_MAX_PX))
        return new Response(null, { status: 404 })
      const key = m[1] as string
      let pixels = this.regionPixels.get(key)
      if (!pixels) {
        pixels = (async () => {
          const blob = full.blob ?? (full.blob = await full.produce?.())
          return new Uint8Array(blob ? await blob.arrayBuffer() : new ArrayBuffer(fw * fh * 4))
        })()
        this.regionPixels.set(key, pixels)
      }
      const src = await pixels
      const k = Math.min(1, Math.sqrt(cap / (w * h)))
      const ow = Math.max(1, Math.floor(w * k))
      const oh = Math.max(1, Math.floor(h * k))
      const out = new Uint8Array(ow * oh * 4)
      for (let oy = 0; oy < oh; oy++) {
        const sy = y + Math.floor((oy * h) / oh)
        if (ow === w) {
          // 不缩放：整行拷贝
          const start = (sy * fw + x) * 4
          out.set(src.subarray(start, start + w * 4), oy * ow * 4)
          continue
        }
        for (let ox = 0; ox < ow; ox++) {
          const s4 = (sy * fw + x + Math.floor((ox * w) / ow)) * 4
          out.set(src.subarray(s4, s4 + 4), (oy * ow + ox) * 4)
        }
      }
      return new Response(out, {
        headers: {
          'cache-control': 'no-store',
          [RGBA_WIDTH_HEADER]: String(ow),
          [RGBA_HEIGHT_HEADER]: String(oh),
        },
      })
    })()
  }
}

function normalize(path: string): string {
  return decodeURIComponent(path).replace(/^\/+/, '')
}

function guessMime(path: string): string {
  if (path.endsWith('.png')) return 'image/png'
  if (path.endsWith('.jpg') || path.endsWith('.jpeg')) return 'image/jpeg'
  if (path.endsWith('.svg')) return 'image/svg+xml'
  return 'application/octet-stream'
}

/** 未注册资源的占位图：按路径哈希取色相 */
function placeholder(path: string): string {
  let hash = 0
  for (const ch of path) hash = (hash * 31 + ch.charCodeAt(0)) | 0
  const hue = Math.abs(hash) % 360
  const icon = path.startsWith('icon/')
  const size = icon ? 64 : 320
  const h = icon ? 64 : 200
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="${size}" height="${h}" viewBox="0 0 ${size} ${h}"><defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="hsl(${hue} 70% 86%)"/><stop offset="1" stop-color="hsl(${(hue + 50) % 360} 65% 66%)"/></linearGradient></defs><rect width="${size}" height="${h}" rx="${icon ? 14 : 0}" fill="url(#g)"/></svg>`
  return `data:image/svg+xml,${encodeURIComponent(svg)}`
}

/** 用 2D canvas 画一张合成的屏幕并取出 RGBA 字节（与 Rust 送来的帧格式一致） */
function synthesizeScreen(width: number, height: number): ArrayBuffer {
  const canvas = new OffscreenCanvas(width, height)
  const ctx = canvas.getContext('2d')
  if (!ctx) return new ArrayBuffer(width * height * 4)
  const bg = ctx.createLinearGradient(0, 0, width, height)
  bg.addColorStop(0, '#e9e4ff')
  bg.addColorStop(1, '#cfe3ff')
  ctx.fillStyle = bg
  ctx.fillRect(0, 0, width, height)
  const win = (x: number, y: number, w: number, h: number) => {
    ctx.fillStyle = 'rgba(0,0,0,0.12)'
    ctx.fillRect(x + 4, y + 10, w, h)
    ctx.fillStyle = '#ffffff'
    ctx.fillRect(x, y, w, h)
    ctx.fillStyle = '#f2f2f4'
    ctx.fillRect(x, y, w, Math.round(h * 0.07))
    ctx.fillStyle = '#1d1d1f'
    const line = Math.round(h * 0.045)
    ctx.font = `${line}px sans-serif`
    const rows = ['性能预算', '面板唤出 p95 ≤ 100 ms', '首 token p95 ≤ 1.2 s', '空闲 CPU ≈ 0%']
    rows.forEach((text, i) => ctx.fillText(text, x + w * 0.06, y + h * 0.2 + i * line * 1.8))
  }
  win(width * 0.07, height * 0.09, width * 0.52, height * 0.58)
  win(width * 0.5, height * 0.4, width * 0.4, height * 0.45)
  return ctx.getImageData(0, 0, width, height).data.buffer as ArrayBuffer
}
