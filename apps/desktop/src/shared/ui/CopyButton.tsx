/**
 * CopyButton：复制后图标换成对勾 1.2 s（presets.iconSwap，减少动效时直接切换），
 * 同时经 aria-live 播报「已复制」。复制走 `copyText`（Rust 命令）；失败时（例如超过 1 MB 被 Rust
 * 拒绝）同样停留 1.2 s 显示失败图标与「复制失败」，不会无声无息。
 */
import { Check, CircleAlert, Copy } from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useEffect, useRef, useState, type ReactNode } from 'react'
import { copyText } from '../hooks/useCopy'
import { useT } from '../i18n/useT'
import { usePreset } from '../motion/presets'
import { ms, timing } from '../motion/tokens'
import { Button, IconButton } from './Button'
import { Icon } from './Icon'

export interface CopyButtonProps {
  /** 要复制的文本，或在点击时才取值的函数 */
  text: string | (() => string)
  label?: string
  shortcut?: string | null
  /** icon：只有图标（默认）；button：带文字的按钮 */
  variant?: 'icon' | 'button'
  onCopied?: (ok: boolean) => void
  disabled?: boolean
  /** 图标按钮 tooltip 的内容（默认是名称 + 快捷键），见 IconButton 的同名属性 */
  tooltip?: ReactNode
  'data-testid'?: string
}

export function CopyButton({
  text,
  label,
  shortcut,
  variant = 'icon',
  onCopied,
  disabled,
  tooltip,
  'data-testid': testId = 'copy-button',
}: CopyButtonProps) {
  const t = useT()
  const [result, setResult] = useState<'copied' | 'failed' | null>(null)
  const copied = result === 'copied'
  const failed = result === 'failed'
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const swap = usePreset('iconSwap')
  useEffect(
    () => () => {
      if (timer.current !== null) clearTimeout(timer.current)
    },
    [],
  )
  const copy = async () => {
    const ok = await copyText(typeof text === 'function' ? text() : text)
    onCopied?.(ok)
    setResult(ok ? 'copied' : 'failed')
    if (timer.current !== null) clearTimeout(timer.current)
    timer.current = setTimeout(() => setResult(null), ms(timing.copiedHold))
  }
  const name = label ?? t('common.action.copy')
  const glyph = (
    <span className="ui-iconswap" aria-hidden="true">
      <AnimatePresence initial={false} mode="popLayout">
        <m.span
          key={result ?? 'copy'}
          className="ui-iconswap-item"
          data-copied={copied || undefined}
          data-failed={failed || undefined}
          {...swap}
        >
          <Icon
            icon={copied ? Check : failed ? CircleAlert : Copy}
            size={variant === 'icon' ? 16 : 14}
          />
        </m.span>
      </AnimatePresence>
    </span>
  )
  const live = (
    <span className="ui-sr-only" aria-live="polite">
      {copied ? t('common.state.copied') : failed ? t('common.state.copy_failed') : ''}
    </span>
  )
  if (variant === 'button') {
    return (
      <>
        <Button
          size="sm"
          variant="secondary"
          disabled={disabled}
          onClick={() => void copy()}
          data-testid={testId}
        >
          <span className="ui-copy-label">
            {glyph}
            {copied ? t('common.action.copied') : failed ? t('common.state.copy_failed') : name}
          </span>
        </Button>
        {live}
      </>
    )
  }
  return (
    <>
      <IconButton
        icon={Copy}
        glyph={glyph}
        label={name}
        shortcut={shortcut}
        tooltip={tooltip}
        disabled={disabled}
        data-testid={testId}
        onClick={() => void copy()}
      />
      {live}
    </>
  )
}
