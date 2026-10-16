/**
 * ProgressBar：4 px 细进度条（离线模型的下载、校验）。确定进度只改条的 `transform: scaleX()`，由 CSS 以
 * `--dur-base` / `--ease-standard` 过渡（Rust 约每 250 ms 推一次进度，过渡正好把相邻两次连起来）；没有确定进度
 * （排队、校验中）时是一段来回滑动的短条——只动 transform，减少动效时静止在中间（base.css 统一停掉 CSS 动画）。
 * 暂停时条变成中性色（进度保留，不再是强调色）。
 */
import { cx } from './cx'

export interface ProgressBarProps {
  /** 0–1；null 表示不确定进度 */
  value: number | null
  /** 已暂停：条换成中性色 */
  paused?: boolean
  /** 无障碍名称（例如「正在下载 PP-OCRv6 Small」） */
  label: string
  /** 读屏念的当前值（例如「42%」）；缺省时按百分比 */
  valueText?: string
  className?: string
  'data-testid'?: string
}

const PERCENT = 100

export function ProgressBar({
  value,
  paused = false,
  label,
  valueText,
  className,
  'data-testid': testId,
}: ProgressBarProps) {
  const ratio = value === null ? null : Math.max(0, Math.min(1, value))
  return (
    <div
      // 原生 <progress> 的条无法只用 transform 过渡（各引擎各画各的）：这里是自绘的条，语义按 WAI-ARIA progressbar 补齐
      // oxlint-disable-next-line jsx-a11y/prefer-tag-over-role
      role="progressbar"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={PERCENT}
      aria-valuenow={ratio === null ? undefined : Math.round(ratio * PERCENT)}
      aria-valuetext={valueText}
      className={cx('ui-progress', className)}
      data-indeterminate={ratio === null || undefined}
      data-paused={paused || undefined}
      data-testid={testId}
    >
      <span
        className="ui-progress-bar"
        style={ratio === null ? undefined : { transform: `scaleX(${ratio})` }}
      />
    </div>
  )
}
