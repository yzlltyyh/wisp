/**
 * ListRow：列表项——前置（类型图标 / 缩略图）、标题、副标题、尾部（来源图标、相对时间、角标）。
 * 高 44（普通）/ 64（图片行），圆角 --r-row；悬停淡底、按下更深；选中有两种画法：
 * - `selected` 且列表没有滑动指示器：行自己画 --fill-selected；
 * - 列表用 SelectionIndicator（VirtualList）时传 `indicator`，行不画底色，只改文字状态（避免两层叠加）。
 */
import type { HTMLAttributes, ReactNode, Ref } from 'react'
import { cx } from './cx'

export interface ListRowProps extends Omit<HTMLAttributes<HTMLDivElement>, 'title'> {
  leading?: ReactNode
  title: ReactNode
  subtitle?: ReactNode
  trailing?: ReactNode
  size?: 'md' | 'lg'
  selected?: boolean
  /** 选中态由外部滑动指示器绘制 */
  indicator?: boolean
  disabled?: boolean
  ref?: Ref<HTMLDivElement>
  'data-testid'?: string
  'data-force'?: string
}

export function ListRow({
  leading,
  title,
  subtitle,
  trailing,
  size = 'md',
  selected = false,
  indicator = false,
  disabled = false,
  className,
  ...rest
}: ListRowProps) {
  return (
    <div
      // 列表行是自绘的 listbox 选项（含图标、高亮、尾部信息），原生 <option> 无法承载
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role
      role="option"
      aria-selected={selected}
      aria-disabled={disabled || undefined}
      {...rest}
      className={cx('ui-row', className)}
      data-size={size}
      data-selected={selected || undefined}
      data-indicator={indicator || undefined}
    >
      {leading ? <span className="ui-row-leading">{leading}</span> : null}
      <span className="ui-row-main">
        <span className="ui-row-title">{title}</span>
        {subtitle ? <span className="ui-row-subtitle">{subtitle}</span> : null}
      </span>
      {trailing ? <span className="ui-row-trailing">{trailing}</span> : null}
    </div>
  )
}

/** 搜索命中高亮：按 [start, end) 区间（UTF-16 下标）把文本切成普通段与 <mark> 段 */
export function Highlight({
  text,
  ranges,
}: {
  text: string
  ranges: readonly { start: number; end: number }[]
}) {
  if (ranges.length === 0) return <>{text}</>
  const sorted = [...ranges].sort((a, b) => a.start - b.start)
  const out: ReactNode[] = []
  let at = 0
  for (const r of sorted) {
    const s = Math.max(at, Math.min(text.length, r.start))
    const e = Math.max(s, Math.min(text.length, r.end))
    if (s > at) out.push(text.slice(at, s))
    if (e > s)
      out.push(
        <mark key={s} className="ui-mark">
          {text.slice(s, e)}
        </mark>,
      )
    at = e
  }
  if (at < text.length) out.push(text.slice(at))
  return <>{out}</>
}
