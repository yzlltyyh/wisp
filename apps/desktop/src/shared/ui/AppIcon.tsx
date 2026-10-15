/**
 * AppIcon：来源应用图标（剪贴板行尾、排除列表）。取不到或加载失败时显示通用应用图标。
 * `src` 应已经过 `resolveAssetUrl`（wisp:// 资源）。
 */
import { AppWindow } from 'lucide-react'
import { useState } from 'react'
import { cx } from './cx'
import { Icon } from './Icon'

export interface AppIconProps {
  src?: string | null
  /** 应用名（alt / tooltip） */
  name: string
  size?: 16 | 20 | 24 | 32
  className?: string
}

export function AppIcon({ src, name, size = 16, className }: AppIconProps) {
  const [failed, setFailed] = useState(false)
  return (
    <span
      className={cx('ui-appicon', className)}
      style={{ width: size, height: size }}
      title={name}
      data-fallback={!src || failed || undefined}
    >
      {src && !failed ? (
        <img
          src={src}
          alt={name}
          width={size}
          height={size}
          draggable={false}
          decoding="async"
          onError={() => setFailed(true)}
        />
      ) : (
        <Icon icon={AppWindow} size={Math.round(size * 0.75)} label={name} />
      )}
    </span>
  )
}
