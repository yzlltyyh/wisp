/**
 * 剪贴板列表的一行：类型图标（图片为缩略图、颜色为色块）· 带高亮的单行预览 ·
 * 来源应用图标 · 相对时间（按住 ⌘ 时换成 ⌘n 角标）。高 44，图片行 64；选中态由列表的滑动指示器绘制。
 *
 * - 行内只用元数据（`preview` ≤ 200 字、缩略图），不取正文、不读文件；
 * - 缩略图经 `wisp://` 加载、异步解码；还没生成时显示图片图标，生成后经 `clipboard:changed { updated }` 原地替换；
 * - 文件条目里全部文件都已不存在时标题置灰划线，行尾标「已不存在」；只有空白的文本显示占位说明；
 * - 无障碍：选项的名称按「类型 · 内容 · 来源 · 时间（· 已置顶）」组织，⌘n 角标不进名称（放在 aria-keyshortcuts）。
 *   进入动效（打开时的 rowIn、实时插入的 rowInsert）由列表包在行外面（ClipList 的 RowShell）。
 */
import { Code, File, Image as ImageIcon, Link, Pin, Type } from 'lucide-react'
import { memo } from 'react'
import { resolveAssetUrl } from '../../shared/ipc'
import type { ClipHit, ClipItemMeta, ClipKind, OsKind } from '../../shared/ipc/types'
import { formatBytes, formatNumber } from '../../shared/i18n/format'
import { relativeTime } from '../../shared/i18n/relativeTime'
import { useT, type TFunction } from '../../shared/i18n/useT'
import { AppIcon, Highlight, Icon, Kbd, ListRow, Thumbnail } from '../../shared/ui'
import { rowTime } from './format'
import { isBlank, META_FILES, type GroupId } from './model'

const KIND_ICONS = {
  text: Type,
  rich_text: Type,
  link: Link,
  code: Code,
  files: File,
  image: ImageIcon,
  color: Type,
} as const satisfies Record<ClipKind, unknown>

export function rowDomId(id: number): string {
  return `clip-row-${id}`
}

export interface ClipRowProps {
  hit: ClipHit
  group: GroupId
  /** 当前可见顺序里的序号（⌘1–9 / 纯文本 ⌥⌘1–9 的 aria-keyshortcuts，见 keys.ts 的 `plainNthCombo`） */
  ordinal: number
  /** 行尾的 ⌘n 角标（1–9）；null 表示不画（第 10 条起，或视图刚挂载、角标还没补上） */
  badge: number | null
  selected: boolean
  now: number
  /** 置顶项的删除确认进行中 */
  confirming: boolean
  /** 检索结果平铺（没有分组标题）：置顶项在行尾带图钉 */
  flat: boolean
  os: OsKind
}

/** 行内缩略图的最大尺寸（design.* 视觉稿：56 × 44） */
const ROW_THUMB = { width: 56, height: 44 } as const

/** 行首：类型图标 / 缩略图 / 色块。`thumb` 是缩略图的最大尺寸（预览头部更小） */
export function ClipLeading({
  hit,
  thumb = ROW_THUMB,
}: {
  hit: ClipHit
  thumb?: { readonly width: number; readonly height: number }
}) {
  const it = hit.item
  if (it.kind === 'image' && it.image?.thumb_url) {
    return (
      <Thumbnail
        src={resolveAssetUrl(it.image.thumb_url)}
        alt=""
        naturalSize={it.image}
        maxHeight={thumb.height}
        maxWidth={thumb.width}
        checkerboard
      />
    )
  }
  if (it.kind === 'color' && it.color) {
    return (
      <span className="clip-kind">
        <span className="clip-swatch" style={{ background: it.color }} />
      </span>
    )
  }
  return (
    <span className="clip-kind">
      <Icon icon={KIND_ICONS[it.kind]} size={14} />
    </span>
  )
}

/** 只有空白的正文：「（3 个空白字符）」（字数未知时「（空白）」） */
export function blankLabel(item: ClipItemMeta, t: TFunction): string {
  const n = item.char_count
  return n !== null && n > 0
    ? t('clipboard.meta.blank', { count: n, n: formatNumber(n) })
    : t('clipboard.meta.blank_unknown')
}

/** 图片的尺寸与大小（屏幕上用间距隔开，读屏时用「，」隔开） */
function imageText(item: ClipItemMeta, t: TFunction): string {
  const image = item.image
  if (!image) return ''
  return `${t('clipboard.meta.dimensions', { width: image.width, height: image.height })}, ${formatBytes(image.bytes)}`
}

export function ClipTitle({ hit }: { hit: ClipHit }) {
  const t = useT()
  const it = hit.item
  if (it.kind === 'image' && it.image) {
    return (
      <>
        {t('clipboard.meta.dimensions', { width: it.image.width, height: it.image.height })}
        <span className="clip-title-meta">{formatBytes(it.image.bytes)}</span>
      </>
    )
  }
  if (it.kind !== 'files' && isBlank(it.preview)) {
    return <span className="clip-blank">{blankLabel(it, t)}</span>
  }
  const text = <Highlight text={it.preview} ranges={hit.highlights} />
  return it.kind === 'code' ? <span className="clip-code">{text}</span> : text
}

function allMissing(item: ClipItemMeta): boolean {
  const files = item.files
  return item.kind === 'files' && !!files && files.length > 0 && files.every((f) => !f.exists)
}

/**
 * 读屏用的选项名称：类型 · 内容 · 来源 · 时间（· 已置顶 / 已不存在）。时间用完整写法（「3 天前」「3 days ago」）：
 * 行尾的紧凑写法「3d」「2m」会被读成「3 d」「2 米」
 */
export function rowLabel(item: ClipItemMeta, now: number, t: TFunction): string {
  let content: string
  if (item.kind === 'image') content = imageText(item, t)
  else if (item.kind === 'files') {
    const files = item.files ?? []
    const n = files.length
    content = [
      files.map((f) => f.name).join(', '),
      n >= META_FILES
        ? t('clipboard.meta.files_at_least', { n: formatNumber(n) })
        : t('clipboard.meta.files', { count: n, n: formatNumber(n) }),
    ].join(', ')
  } else if (isBlank(item.preview)) content = blankLabel(item, t)
  else content = item.preview
  return [
    t(`clipboard.kind.${item.kind}`),
    content,
    item.source?.name ?? t('clipboard.a11y.unknown_app'),
    relativeTime(item.last_used_at, now, 'long'),
    item.pinned ? t('clipboard.a11y.pinned') : null,
    allMissing(item) ? t('clipboard.meta.missing') : null,
  ]
    .filter(Boolean)
    .join(', ')
}

function ClipRowImpl({ hit, ordinal, badge, selected, now, confirming, flat, os }: ClipRowProps) {
  const t = useT()
  const it = hit.item
  const source = it.source
  const missing = allMissing(it)
  return (
    <ListRow
      id={rowDomId(it.id)}
      className="clip-row"
      indicator
      selected={selected}
      size={it.kind === 'image' ? 'lg' : 'md'}
      aria-label={rowLabel(it, now, t)}
      aria-keyshortcuts={
        ordinal <= 9
          ? os === 'macos'
            ? `Meta+${ordinal} Alt+Meta+${ordinal}`
            : `Control+${ordinal} Control+Shift+${ordinal}`
          : undefined
      }
      data-clip-id={it.id}
      data-kind={it.kind}
      data-missing={missing || undefined}
      data-confirm={confirming || undefined}
      data-badge={badge !== null || undefined}
      data-testid="clip-row"
      leading={<ClipLeading hit={hit} />}
      title={<ClipTitle hit={hit} />}
      trailing={
        <>
          {missing ? <span className="clip-missing-tag">{t('clipboard.meta.missing')}</span> : null}
          {flat && it.pinned ? (
            <span className="clip-pin-mark" data-testid="clip-pin-mark">
              <Icon icon={Pin} size={12} />
            </span>
          ) : null}
          <span className="clip-source">
            <AppIcon
              src={source?.icon_url ? resolveAssetUrl(source.icon_url) : null}
              name={source?.name ?? t('clipboard.a11y.unknown_app')}
            />
          </span>
          <span className="clip-when" data-badge={badge !== null || undefined}>
            <span className="clip-time">{rowTime(it.last_used_at, now)}</span>
            {badge !== null ? (
              <Kbd className="clip-badge" combo={`Mod+Digit${badge}`} variant="cap" />
            ) : null}
          </span>
        </>
      }
    />
  )
}

export const ClipRow = memo(ClipRowImpl)
