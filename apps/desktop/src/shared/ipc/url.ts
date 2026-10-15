/**
 * `wisp://` 资源 URL（Rust 侧见 `src-tauri/src/assets.rs`）。
 *
 * Rust 侧的资源路径前缀：`capture/<session>/<monitor-hex>/{half,full}`（原始 RGBA，响应头带 `x-wisp-width` /
 * `x-wisp-height`）、`crop/<capture_id>.png`、`img/<key>`、`thumb/<key>`、`icon/<key>.png`、`ocrimg/<key>`。
 * `<monitor-hex>` 是 MonitorId 的 UTF-8 字节的小写十六进制（Windows 的 `\\.\DISPLAY1` 含反斜杠，原样放进路径会被
 * Rust 的路径校验以 400 拒绝），由 Rust 的 `assets::capture_asset_path` 生成并经 `OverlayInit.image_url` / `half_url`
 * 下发——**前端不自己拼 `capture/` 路径**，只把载荷里的路径交给 `resolveAssetUrl`。
 * 前端拼 URL 一律经 `convertFileSrc(path, 'wisp')`（由传输层实现），不手写平台差异。
 */
import { getTransport } from './runtime'

/** 资源路径 → 可直接用于 `<img src>` / `fetch` 的 URL */
export function wispUrl(path: string): string {
  return getTransport().protocolUrl(path.replace(/^\/+/, ''))
}

const HAS_SCHEME = /^[a-z][a-z0-9+.-]*:/i

/**
 * 载荷里的 `*_url` 字段：已是完整 URL（`wisp:` / `http:` / `data:` / `blob:`）时原样返回，
 * 否则视为资源路径并转换。
 */
export function resolveAssetUrl(urlOrPath: string): string {
  return HAS_SCHEME.test(urlOrPath) ? urlOrPath : wispUrl(urlOrPath)
}

/** 原始 RGBA 帧（`capture/...`）的尺寸响应头 */
export const RGBA_WIDTH_HEADER = 'x-wisp-width'
export const RGBA_HEIGHT_HEADER = 'x-wisp-height'

/**
 * 拉取原始 RGBA 帧并解码为 ImageBitmap（`createImageBitmap` 不能直接解码裸 RGBA 字节）。
 * 调用方用完后必须 `bitmap.close()`。
 */
export async function fetchRgbaBitmap(url: string, signal?: AbortSignal): Promise<ImageBitmap> {
  const res = await fetch(url, { signal: signal ?? null })
  if (!res.ok) throw new Error(`wisp asset ${res.status}: ${url}`)
  const width = Number(res.headers.get(RGBA_WIDTH_HEADER))
  const height = Number(res.headers.get(RGBA_HEIGHT_HEADER))
  const buf = await res.arrayBuffer()
  if (!width || !height || buf.byteLength !== width * height * 4) {
    throw new Error(`wisp asset: bad RGBA frame ${width}x${height} (${buf.byteLength} bytes)`)
  }
  return createImageBitmap(new ImageData(new Uint8ClampedArray(buf), width, height))
}
