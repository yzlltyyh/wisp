/**
 * 轻量的组合键捕获（应用内键位映射用，例如终端的粘贴键）：点击后按下一个组合即记下（global-hotkey 规范串），
 * Esc 取消。不做全局冲突检测——这不是全局快捷键，只是告诉 Wisp 在某个应用里发什么键。
 */
import { useState, type KeyboardEvent as ReactKeyboardEvent } from 'react'
import { usePlatform } from '../../shared/hooks/usePlatform'
import { useT } from '../../shared/i18n/useT'
import { comboFromEvent, isModifierEvent } from '../../shared/keys/combo'
import { cx, Kbd } from '../../shared/ui'

export function KeyCapture({
  value,
  onChange,
  label,
  className,
  'data-testid': testId,
}: {
  value: string | null
  onChange: (combo: string) => void
  label: string
  className?: string
  'data-testid'?: string
}) {
  const t = useT()
  const os = usePlatform()
  const [listening, setListening] = useState(false)
  const onKeyDown = (e: ReactKeyboardEvent<HTMLButtonElement>) => {
    if (!listening) return
    e.preventDefault()
    e.stopPropagation()
    if (e.key === 'Escape') {
      setListening(false)
      return
    }
    if (isModifierEvent(e.nativeEvent)) return
    const combo = comboFromEvent(e.nativeEvent, os)
    if (!combo) return
    setListening(false)
    onChange(combo)
  }
  return (
    <button
      type="button"
      className={cx('ui-recorder', 'st-keycap', className)}
      data-recording={listening || undefined}
      aria-label={label}
      aria-pressed={listening}
      onClick={() => setListening((v) => !v)}
      onKeyDown={onKeyDown}
      onBlur={() => setListening(false)}
      data-testid={testId}
    >
      {listening ? (
        <span className="ui-recorder-prompt">{t('hotkeys.recorder.recording')}</span>
      ) : value ? (
        <Kbd combo={value} />
      ) : (
        <span className="ui-recorder-placeholder">{t('hotkeys.recorder.none')}</span>
      )}
    </button>
  )
}
