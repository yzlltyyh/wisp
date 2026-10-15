/**
 * Skeleton：首 token 前的 2–3 条占位，1 Hz 脉冲（opacity，减少动效与隐藏时静止）。
 * 每条的高度与行距按正文行高设计，文字替换骨架时整体高度几乎不变（CLS ≈ 0）。
 */
export interface SkeletonProps {
  lines?: number
  'data-testid'?: string
}

/** 各行的宽度（最后一行短一些，像一段自然结束的文字） */
const WIDTHS = ['100%', '94%', '62%', '86%', '72%']

export function Skeleton({ lines = 3, 'data-testid': testId = 'skeleton' }: SkeletonProps) {
  const n = Math.max(1, lines)
  return (
    <div data-testid={testId} aria-hidden="true" className="ui-skeleton">
      {Array.from({ length: n }, (_, i) => (
        <span
          key={i}
          className="ui-skeleton-line"
          style={{ width: i === n - 1 ? WIDTHS[2] : WIDTHS[i % 2] }}
        />
      ))}
    </div>
  )
}
