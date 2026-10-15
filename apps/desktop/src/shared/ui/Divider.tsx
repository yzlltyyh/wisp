/** Divider：1 px hairline；`inset` 时左右缩进面板内边距（列表分组）。 */
import { cx } from './cx'

export function Divider({
  orientation = 'horizontal',
  inset = false,
  className,
}: {
  orientation?: 'horizontal' | 'vertical'
  inset?: boolean
  className?: string
}) {
  return (
    <hr
      aria-orientation={orientation}
      className={cx('ui-divider', className)}
      data-orientation={orientation}
      data-inset={inset || undefined}
    />
  )
}
