/**
 * 剪贴板列表的一行：类型图标（图片为缩略图、颜色为色块；多选时右上角叠选择序号）· 收藏的名称（加粗）+ 带高亮的单行
 * 预览 · 验证码的删除倒计时 · 来源应用图标 · 相对时间（按住 ⌘ 时换成 ⌘n 角标）。高 44，图片行 64；选中态由列表的
 * 滑动指示器绘制。
 *
 * - 行内只用元数据（`preview` ≤ 200 字、缩略图、名称、到期时间），不取正文、不读文件；
 * - 缩略图经 `wisp://` 加载、异步解码；还没生成时显示图片图标，生成后经 `clipboard:changed { updated }` 原地替换；
 * - 文件条目里全部文件都已不存在时标题置灰划线，行尾标「已不存在」；只有空白的文本显示占位说明；
 * - 验证码「N 分钟后删除」用视图每分钟刷新一次的时间基准（不另开计时器）；
 * - 无障碍：选项的名称按「类型 · 名称 · 内容 · 来源 · 时间（· 已收藏 · 验证码 · 已选择）」组织，⌘n 角标不进名称（放在
 *   aria-keyshortcuts）。进入动效（打开时的 rowIn、实时插入的 rowInsert）由列表包在行外面（ClipList 的 RowShell）。
 */
import { Code, File, Image as ImageIcon, Link, Star, Type } from 'lucide-react'
import { memo, type ReactNode } from 'react'
import { resolveAssetUrl } from '../../shared/ipc'
import type { ClipHit, ClipItemMeta, ClipKind, OsKind } from '../../shared/ipc/types'
import { formatBytes, formatNumber } from '../../shared/i18n/format'
import { relativeTime } from '../../shared/i18n/relativeTime'
import { useT, type TFunction } from '../../shared/i18n/useT'
import { AppIcon, Highlight, Icon, Kbd, ListRow, Thumbnail } from '../../shared/ui'
import { nthShortcut } from './actions'
import { rowTime } from './format'
import { allFilesMissing, isDraggable } from './kinds'
import { findMatches, isBlank, META_FILES, type GroupId } from './model'

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
  /** 当前可见顺序里的序号（⌘1–9 / 纯文本 ⌥⌘1–9 的 aria-keyshortcuts，组合来自操作注册表） */
  ordinal: number
  /** 行尾的 ⌘n 角标（1–9）；null 表示不画（第 10 条起，或视图刚挂载、角标还没补上） */
  badge: number | null
  selected: boolean
  /** 多选的序号（从 1 开始，按选择顺序）；没有被选择时为 null */
  mark: number | null
  now: number
  /** 收藏的删除确认进行中 */
  confirming: boolean
  /** 检索结果平铺（没有分组标题）：收藏在行尾带星标 */
  flat: boolean
  /** 检索词（收藏的名称按它高亮）；不在检索时为空 */
  query: string
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

/** 收藏的名称：加粗，后面的预览变淡（检索时名称里的命中同样高亮） */
function NameTitle({
  name,
  query,
  children,
}: {
  name: string
  query: string
  children: ReactNode
}) {
  return (
    <>
      <span className="clip-name">
        <Highlight text={name} ranges={query ? findMatches(name, query, 8) : []} />
      </span>
      <span className="clip-name-preview">{children}</span>
    </>
  )
}

export function ClipTitle({ hit, query = '' }: { hit: ClipHit; query?: string }) {
  const name = hit.item.name
  const body = <ClipBody hit={hit} />
  return name ? (
    <NameTitle name={name} query={query}>
      {body}
    </NameTitle>
  ) : (
    body
  )
}

function ClipBody({ hit }: { hit: ClipHit }) {
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

/**
 * 验证码条目的删除倒计时（「验证码 · 9 分钟后删除」）：按分钟向上取整，不到 1 分钟时「即将删除」；没有到期时间（不是
 * 验证码、已收藏）或已经到期（Rust 马上删除）时为 null
 */
export function otpLabel(item: ClipItemMeta, now: number, t: TFunction): string | null {
  const at = item.expires_at
  if (at == null || item.pinned || at <= now) return null
  const minutes = Math.ceil((at - now) / 60_000)
  return minutes <= 1
    ? t('clipboard.meta.otp_soon')
    : t('clipboard.meta.otp_expires', { n: minutes })
}

/**
 * 读屏用的选项名称：类型 · 名称 · 内容 · 来源 · 时间（· 已收藏 / 验证码倒计时 / 已不存在 / 已选择第 n 个）。时间用完整
 * 写法（「3 天前」「3 days ago」）：行尾的紧凑写法「3d」「2m」会被读成「3 d」「2 米」
 */
export function rowLabel(
  item: ClipItemMeta,
  now: number,
  t: TFunction,
  mark: number | null = null,
): string {
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
    item.name ?? null,
    content,
    item.source?.name ?? t('clipboard.a11y.unknown_app'),
    relativeTime(item.last_used_at, now, 'long'),
    item.pinned ? t('clipboard.a11y.favorite') : null,
    otpLabel(item, now, t),
    allFilesMissing(item) ? t('clipboard.meta.missing') : null,
    mark !== null ? t('clipboard.a11y.marked', { n: mark }) : null,
  ]
    .filter(Boolean)
    .join(', ')
}

function ClipRowImpl({
  hit,
  ordinal,
  badge,
  selected,
  mark,
  now,
  confirming,
  flat,
  query,
  os,
}: ClipRowProps) {
  const t = useT()
  const it = hit.item
  const source = it.source
  const missing = allFilesMissing(it)
  const otp = otpLabel(it, now, t)
  // 行尾的 ⌘n 角标：组合来自注册表的「粘贴第 n 条」
  const badgeCombo = badge === null ? null : nthShortcut(os, badge)?.combo
  return (
    <ListRow
      id={rowDomId(it.id)}
      className="clip-row"
      indicator
      selected={selected}
      size={it.kind === 'image' ? 'lg' : 'md'}
      aria-label={rowLabel(it, now, t, mark)}
      aria-keyshortcuts={nthShortcut(os, ordinal)?.aria}
      data-clip-id={it.id}
      data-kind={it.kind}
      // 文本类可以拖到别的应用里（拖拽数据由列表的事件委托在按下时取好）
      draggable={isDraggable(it.kind) || undefined}
      data-missing={missing || undefined}
      data-confirm={confirming || undefined}
      data-badge={badge !== null || undefined}
      data-marked={mark !== null || undefined}
      data-named={it.name ? true : undefined}
      data-testid="clip-row"
      leading={
        <span className="clip-lead">
          <ClipLeading hit={hit} />
          {mark !== null ? (
            <span className="clip-mark" data-testid="clip-mark">
              {mark}
            </span>
          ) : null}
        </span>
      }
      title={<ClipTitle hit={hit} query={query} />}
      trailing={
        <>
          {missing ? <span className="clip-missing-tag">{t('clipboard.meta.missing')}</span> : null}
          {otp ? (
            <span className="clip-otp-tag" data-testid="clip-otp">
              {otp}
            </span>
          ) : null}
          {flat && it.pinned ? (
            <span className="clip-fav-mark" data-testid="clip-favorite-mark">
              <Icon icon={Star} size={12} />
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
            {badgeCombo ? <Kbd className="clip-badge" combo={badgeCombo} variant="cap" /> : null}
          </span>
        </>
      }
    />
  )
}

export const ClipRow = memo(ClipRowImpl)
