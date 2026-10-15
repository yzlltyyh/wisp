/**
 * 快捷键显示：组合串 → 逐个键帽，格式一律来自 `formatHotkey`（三端规则见 keys/formatHotkey.ts）。
 *
 * - `variant="keys"`（默认）：每个键一个键帽；
 * - `variant="cap"`：整串放进一个键帽（提示条等空间紧张处；Windows 的文字键名较长）；
 * - `variant="inline"`：不画键帽，整串文本（菜单的快捷键列、tooltip 内）；
 * - `tone="inverted"`：放在深色 tooltip 上。
 */
import { formatHotkey, hotkeyParts } from '../keys/formatHotkey'
import { usePlatform } from '../hooks/usePlatform'
import { cx } from './cx'

export interface KbdProps {
  /** 组合串，例如 `Mod+KeyC`、`Alt+KeyD`、`Escape` */
  combo: string
  variant?: 'keys' | 'cap' | 'inline'
  tone?: 'default' | 'inverted'
  className?: string
  'data-testid'?: string
}

export function Kbd({
  combo,
  variant = 'keys',
  tone = 'default',
  className,
  'data-testid': testId,
}: KbdProps) {
  const os = usePlatform()
  const label = formatHotkey(combo, os)
  if (variant === 'cap') {
    return (
      <kbd className={cx('ui-kbd', className)} data-tone={tone} data-testid={testId}>
        <span className="ui-kbd-key">{label}</span>
      </kbd>
    )
  }
  if (variant === 'inline') {
    return (
      <kbd className={cx('ui-kbd-inline', className)} data-tone={tone} data-testid={testId}>
        {label}
      </kbd>
    )
  }
  return (
    <kbd
      className={cx('ui-kbd', className)}
      data-tone={tone}
      aria-label={label}
      data-testid={testId}
    >
      {hotkeyParts(combo, os).map((part) => (
        <span key={part} className="ui-kbd-key" aria-hidden="true">
          {part}
        </span>
      ))}
    </kbd>
  )
}
