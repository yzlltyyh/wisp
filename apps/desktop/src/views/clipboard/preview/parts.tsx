/**
 * 预览的公共部件：详情加载、可释放的图片（缩略图打底、原图淡入）、文件列表、颜色值、元信息。
 */
import {
  AppWindow,
  File,
  FileArchive,
  FileCode,
  FileImage,
  FileJson,
  FileMusic,
  FilePlay,
  FileSpreadsheet,
  FileText,
  FileType,
  FileX,
  Folder,
  Presentation,
  type LucideIcon,
} from 'lucide-react'
import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import { resolveAssetUrl } from '../../../shared/ipc'
import type { ClipItemDetail, ClipItemMeta, FileRef } from '../../../shared/ipc/types'
import { formatBytes, formatChars, formatNumber } from '../../../shared/i18n/format'
import type { TFunction } from '../../../shared/i18n/useT'
import { CopyButton, Icon } from '../../../shared/ui'
import { formatHex, formatHsl, formatRgb, META_FILES, parseColor, type Rgba } from '../model'
import type { ClipActions } from '../useClipboard'

/** 详情：undefined = 加载中，null = 取不到（已删除 / 出错） */
export function useDetail(
  actions: ClipActions,
  id: number,
  needed: boolean,
): ClipItemDetail | null | undefined {
  const [state, setState] = useState<{ id: number; detail: ClipItemDetail | null | undefined }>(
    () => ({ id, detail: needed ? actions.cachedDetail(id) : undefined }),
  )
  // 换了条目：先用缓存（有的话），没有就回到加载中
  if (state.id !== id) setState({ id, detail: needed ? actions.cachedDetail(id) : undefined })
  useEffect(() => {
    if (!needed) return
    let live = true
    void actions.detail(id).then((detail) => {
      if (live) setState((cur) => (cur.id === id ? { id, detail } : cur))
    })
    return () => {
      live = false
    }
  }, [actions, id, needed])
  return state.id === id ? state.detail : undefined
}

/** 悬停卡片用的正文开头：undefined = 加载中，null = 取不到 */
export function useHead(
  actions: ClipActions,
  id: number,
  needed: boolean,
): string | null | undefined {
  const [state, setState] = useState<{ id: number; head: string | null | undefined }>(() => ({
    id,
    head: needed ? actions.cachedHead(id) : undefined,
  }))
  if (state.id !== id) setState({ id, head: needed ? actions.cachedHead(id) : undefined })
  useEffect(() => {
    if (!needed) return
    let live = true
    void actions.textHead(id).then((head) => {
      if (live) setState((cur) => (cur.id === id ? { id, head } : cur))
    })
    return () => {
      live = false
    }
  }, [actions, id, needed])
  return state.id === id ? state.head : undefined
}

/**
 * 可释放的 `<img>`：异步解码，加载完成后淡入；卸载时清空 src，让 WebView 尽早丢掉解码后的位图
 * （原图可达 20 MB，退出预览后就要释放）
 */
export function ReleasingImage({
  src,
  className,
  onLoad,
}: {
  src: string
  className?: string
  onLoad?: () => void
}) {
  const ref = useRef<HTMLImageElement>(null)
  const [loaded, setLoaded] = useState<string | null>(null)
  useLayoutEffect(() => {
    const el = ref.current
    // StrictMode 的「卸载 → 再挂载」会先走一遍下面的清理：再挂载时把 src 补回来
    if (el && el.getAttribute('src') !== src) el.setAttribute('src', src)
    return () => {
      // 去掉 src（而不是设成空串）：不会触发对页面地址的请求
      el?.removeAttribute('src')
    }
  }, [src])
  return (
    <img
      ref={ref}
      className={className}
      src={src}
      alt=""
      draggable={false}
      decoding="async"
      data-loaded={loaded === src || undefined}
      data-releasable=""
      onLoad={() => {
        setLoaded(src)
        onLoad?.()
      }}
    />
  )
}

/** 图片：缩略图打底（立即可见），原图到达后淡入覆盖；按给定的框等比缩放 */
export function ImageView({
  item,
  detail,
  maxWidth,
  maxHeight,
}: {
  item: ClipItemMeta
  detail: ClipItemDetail | null | undefined
  maxWidth: number
  maxHeight: number
}) {
  const image = item.image
  const w = image?.width ?? 1
  const h = image?.height ?? 1
  const scale = Math.min(1, maxWidth / w, maxHeight / h)
  const width = Math.max(1, Math.round(w * scale))
  const height = Math.max(1, Math.round(h * scale))
  const thumb = image?.thumb_url ? resolveAssetUrl(image.thumb_url) : null
  const full = detail?.image_url ? resolveAssetUrl(detail.image_url) : null
  return (
    <span className="clip-image" style={{ width, height }} data-testid="clip-image">
      {thumb ? <ReleasingImage className="clip-image-thumb" src={thumb} /> : null}
      {full ? <ReleasingImage className="clip-image-full" src={full} /> : null}
    </span>
  )
}

/** 扩展名 → 文件类型图标（不读文件、不取系统图标：只看名字；Rust 的图标通道只按应用取图标，不覆盖任意文件） */
const FILE_KINDS: ReadonlyArray<readonly [LucideIcon, readonly string[]]> = [
  [
    FileImage,
    [
      'png',
      'jpg',
      'jpeg',
      'gif',
      'webp',
      'heic',
      'heif',
      'bmp',
      'tif',
      'tiff',
      'svg',
      'avif',
      'ico',
      'psd',
      'fig',
      'sketch',
      'xd',
      'ai',
    ],
  ],
  [FilePlay, ['mp4', 'mov', 'm4v', 'mkv', 'avi', 'webm', 'wmv', 'flv']],
  [FileMusic, ['mp3', 'wav', 'flac', 'aac', 'm4a', 'ogg', 'opus', 'aif', 'aiff']],
  [
    FileArchive,
    [
      'zip',
      'rar',
      '7z',
      'tar',
      'gz',
      'tgz',
      'bz2',
      'xz',
      'zst',
      'dmg',
      'iso',
      'pkg',
      'msi',
      'deb',
      'rpm',
    ],
  ],
  [FileJson, ['json', 'jsonc', 'json5']],
  [
    FileCode,
    [
      'js',
      'mjs',
      'cjs',
      'ts',
      'tsx',
      'jsx',
      'rs',
      'py',
      'go',
      'java',
      'kt',
      'swift',
      'c',
      'cc',
      'cpp',
      'h',
      'hpp',
      'cs',
      'rb',
      'php',
      'sh',
      'zsh',
      'bash',
      'ps1',
      'css',
      'scss',
      'html',
      'htm',
      'vue',
      'svelte',
      'sql',
      'lua',
      'dart',
      'toml',
      'yaml',
      'yml',
      'xml',
      'ini',
    ],
  ],
  [FileSpreadsheet, ['xls', 'xlsx', 'csv', 'tsv', 'numbers', 'ods']],
  [Presentation, ['ppt', 'pptx', 'key', 'odp']],
  [FileText, ['txt', 'md', 'markdown', 'rtf', 'doc', 'docx', 'pages', 'odt', 'pdf', 'epub', 'log']],
  [FileType, ['ttf', 'otf', 'woff', 'woff2']],
]

/** 文件的图标：不存在 → 划掉的文件；没有大小（目录 / 包）→ 文件夹或应用；其余按扩展名 */
export function fileIcon(f: Pick<FileRef, 'name' | 'exists' | 'size'>): LucideIcon {
  if (!f.exists) return FileX
  const dot = f.name.lastIndexOf('.')
  const ext = dot > 0 ? f.name.slice(dot + 1).toLowerCase() : ''
  if (f.size === null) return ext === 'app' || ext === 'exe' ? AppWindow : Folder
  for (const [icon, exts] of FILE_KINDS) if (exts.includes(ext)) return icon
  return File
}

export function FileList({
  files,
  total,
  t,
  showPath = false,
}: {
  files: readonly FileRef[]
  /** 文件总数；undefined 表示未知（列表元数据最多只带前 5 个，悬停卡片里不知道总数，不显示「还有 N 个」） */
  total?: number
  t: TFunction
  showPath?: boolean
}) {
  const more = total === undefined ? 0 : total - files.length
  return (
    <ul className="clip-files" data-testid="clip-files">
      {files.map((f) => (
        <li key={f.path} className="clip-file" data-missing={!f.exists || undefined}>
          {showPath ? (
            // 全文预览：类型图标放进 28 px 的圆角底（与路径两行对齐）；悬停卡片里是 14 px 的行内图标
            <span className="clip-file-tile" aria-hidden="true">
              <Icon icon={fileIcon(f)} size={16} />
            </span>
          ) : (
            <Icon icon={fileIcon(f)} size={14} className="clip-file-icon" />
          )}
          <span className="clip-file-main">
            <span className="clip-file-name">{f.name}</span>
            {showPath ? <span className="clip-file-path">{parentOf(f.path)}</span> : null}
          </span>
          <span className="clip-file-meta">
            {f.exists ? (f.size !== null ? formatBytes(f.size) : '') : t('clipboard.meta.missing')}
          </span>
        </li>
      ))}
      {more > 0 ? (
        <li className="clip-file clip-file-more">
          {t('clipboard.meta.more_files', { count: more, n: formatNumber(more) })}
        </li>
      ) : null}
    </ul>
  )
}

function parentOf(path: string): string {
  const i = Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'))
  return i > 0 ? path.slice(0, i) : path
}

/** 任意 CSS 颜色 → sRGB（常见写法直接解析；oklch 等交给 canvas 换算） */
export function toRgba(value: string): Rgba | null {
  const parsed = parseColor(value)
  if (parsed) return parsed
  try {
    const canvas = document.createElement('canvas')
    canvas.width = 1
    canvas.height = 1
    const ctx = canvas.getContext('2d', { willReadFrequently: true })
    if (!ctx) return null
    ctx.fillStyle = '#000'
    ctx.fillStyle = value
    ctx.fillRect(0, 0, 1, 1)
    const d = ctx.getImageData(0, 0, 1, 1).data
    return { r: d[0] ?? 0, g: d[1] ?? 0, b: d[2] ?? 0, a: (d[3] ?? 255) / 255 }
  } catch {
    return null
  }
}

export function ColorInfo({
  value,
  t,
  copy = false,
}: {
  value: string
  t: TFunction
  /** 每一行带复制按钮（全文预览） */
  copy?: boolean
}) {
  const [rgba] = useState(() => toRgba(value))
  const rows = rgba
    ? [
        { id: 'hex', label: 'HEX', value: formatHex(rgba) },
        { id: 'rgb', label: 'RGB', value: formatRgb(rgba) },
        { id: 'hsl', label: 'HSL', value: formatHsl(rgba) },
      ]
    : [{ id: 'css', label: 'CSS', value }]
  return (
    <div className="clip-color" data-testid="clip-color">
      <span className="clip-color-swatch" style={{ background: value }} />
      <dl className="clip-color-values">
        {rows.map((r) => (
          <div key={r.id} className="clip-color-row">
            <dt>{r.label}</dt>
            <dd>{r.value}</dd>
            {copy ? (
              <CopyButton
                text={r.value}
                label={`${t('clipboard.action.copy_value')} ${r.label}`}
                data-testid={`clip-copy-${r.id}`}
              />
            ) : null}
          </div>
        ))}
      </dl>
    </div>
  )
}

/** 条目的元信息片段：文本类的字数 / 行数、图片的尺寸 / 大小、文件数 */
export function metaParts(item: ClipItemMeta, t: TFunction): string[] {
  const out: string[] = []
  switch (item.kind) {
    case 'image':
      if (item.image) {
        out.push(
          t('clipboard.meta.dimensions', { width: item.image.width, height: item.image.height }),
          formatBytes(item.image.bytes),
        )
      }
      break
    case 'files': {
      // 元数据最多带前 5 个文件：等于 5 时实际可能更多，写「5+ 个文件」（精确数目只在全文预览里有）
      const n = item.files?.length ?? 0
      if (n >= META_FILES) out.push(t('clipboard.meta.files_at_least', { n: formatNumber(n) }))
      else if (n > 0) out.push(t('clipboard.meta.files', { count: n, n: formatNumber(n) }))
      break
    }
    case 'link':
    case 'color':
      // 链接 / 颜色的字数没有意义
      break
    default:
      if (item.char_count !== null) out.push(formatChars(item.char_count))
      if (item.line_count !== null && item.line_count > 1) {
        out.push(
          t('clipboard.meta.lines', { count: item.line_count, n: formatNumber(item.line_count) }),
        )
      }
  }
  return out
}
