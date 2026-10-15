/** 转圈指示（按钮加载态、小范围等待）。减少动效时静止（base.css 统一处理）。 */
import { LoaderCircle } from 'lucide-react'
import { useT } from '../i18n/useT'
import { cx } from './cx'
import { Icon } from './Icon'

export function Spinner({ size = 16, className }: { size?: number; className?: string }) {
  const t = useT()
  return (
    <output className={cx('ui-spinner', className)} aria-label={t('common.a11y.loading')}>
      <Icon icon={LoaderCircle} size={size} />
    </output>
  )
}
