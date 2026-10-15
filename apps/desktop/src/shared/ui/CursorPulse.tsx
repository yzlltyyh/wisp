/**
 * CursorPulse：首 token 前的光标「▍」，1 Hz 闪烁；减少动效时静止。
 * 流式输出过程中的末尾光标由 StreamView 用 CSS ::after 绘制，样式与这里一致。
 */
import { cx } from './cx'

export function CursorPulse({
  className,
  'data-testid': testId = 'cursor-pulse',
}: {
  className?: string
  'data-testid'?: string
}) {
  return (
    <span className={cx('ui-caret', className)} aria-hidden="true" data-testid={testId}>
      ▍
    </span>
  )
}
