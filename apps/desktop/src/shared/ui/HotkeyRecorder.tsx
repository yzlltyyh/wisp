/**
 * HotkeyRecorder。
 *
 * 状态：
 * - 静止：显示当前组合的键帽（或「未设置」），点击 / Enter / Space 进入录制；
 * - 录制中：强调色描边 + 1 Hz 脉冲（减少动效时静态强调色），实时回显已按下的修饰键，提示「Esc 取消 · ⌫ 清除」；
 *   进入录制时调用 `hotkey_record_begin`（Rust 注销全部全局键，录制中按 ⌥D 不会弹出面板），离开录制时——包括录制中
 *   组件被卸载（深链切换设置分组、列表重建时浏览器不一定派发 blur）——一定调用 `hotkey_record_end`；
 * - 按下主键时记下候选组合并整串回显（至少一个修饰键，F13–F24 可裸用），**抬起主键即完成**。
 *   macOS 按住 ⌘ 时 WebKit / Chromium 都不派发主键的 keyup，所以抬起任一修饰键也视为完成（组合在 keydown 时已定）；
 *   完成后 `hotkey_check` 做冲突检测，按三级提示：红（forbidden，不能用）/ 黄（warning，可「仍然使用」）/ 灰（info）；
 *   检测本身失败时显示通用错误，不静默丢弃；
 * - 录制中 Esc 取消、⌫ 清除（onChange(null)）、失焦取消。
 * 键盘事件在路由的 overlay 层以模态方式截获，录制期间视图与全局快捷键都不响应。
 */
import { m } from 'motion/react'
import { useEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from 'react'
import { usePlatform } from '../hooks/usePlatform'
import { Trans } from '../i18n/Trans'
import { useT, useTKey } from '../i18n/useT'
import { hotkeyCheck, hotkeyRecordBegin, hotkeyRecordEnd } from '../ipc/commands'
import type { Conflict, ConflictLevel, ConflictReport, HotkeyTarget } from '../ipc/types'
import { codeOf, comboFromEvent, isModifierEvent, modifiersOf } from '../keys/combo'
import { formatModifiers, hotkeySpeech } from '../keys/formatHotkey'
import { installKeyRouter, registerScope } from '../keys/keyRouter'
import { usePreset } from '../motion/presets'
import { Button } from './Button'
import { cx } from './cx'
import { Kbd } from './Kbd'

export interface HotkeyRecorderProps {
  target: HotkeyTarget
  /** 当前生效的组合（global-hotkey 规范串）；null 表示未设置 */
  value: string | null
  /** 确认新组合（force = 用户选择了「仍然使用」）；null 表示清除 */
  onChange: (combo: string | null, force: boolean) => void
  /** 冲突检测（默认调用 Rust 的 hotkey_check） */
  check?: (target: HotkeyTarget, combo: string) => Promise<ConflictReport>
  /** 无障碍名称（例如「划词翻译的快捷键」） */
  label: string
  disabled?: boolean
  /** 展示页：冻结在录制状态 */
  forceRecording?: boolean
  /** 展示页：直接显示一份冲突报告 */
  initialReport?: ConflictReport | null
  /**
   * 受控的冲突报告：调用方在 `hotkey_set` 被拒绝后重新 `hotkey_check` 拿到的完整报告交给录制器显示
   * （与录制时自己检测到的报告同一个位置、同一种样式）；新一轮录制开始时录制器调用 `onReportChange(null)`
   */
  report?: ConflictReport | null
  onReportChange?: (report: ConflictReport | null) => void
  className?: string
  'data-testid'?: string
}

/** 按下主键时记下的候选组合（抬起时提交） */
interface Candidate {
  combo: string
  code: string
}

const LEVEL_ORDER: Record<ConflictLevel, number> = { forbidden: 0, warning: 1, info: 2 }
const BARE_KEYS = /^F(1[3-9]|2[0-4])$/

function worst(conflicts: readonly Conflict[]): Conflict | null {
  return [...conflicts].sort((a, b) => LEVEL_ORDER[a.level] - LEVEL_ORDER[b.level])[0] ?? null
}

export function HotkeyRecorder({
  target,
  value,
  onChange,
  check = hotkeyCheck,
  label,
  disabled = false,
  forceRecording = false,
  initialReport = null,
  report: controlledReport,
  onReportChange,
  className,
  'data-testid': testId = 'hotkey-recorder',
}: HotkeyRecorderProps) {
  const t = useT()
  const tKey = useTKey()
  const os = usePlatform()
  const pulse = usePreset('statusPulse')
  const button = useRef<HTMLButtonElement>(null)
  const [recording, setRecording] = useState(forceRecording)
  const [held, setHeld] = useState<string[]>([])
  const [candidate, setCandidate] = useState<Candidate | null>(null)
  const [ownReport, setOwnReport] = useState<ConflictReport | null>(initialReport)
  const report = controlledReport !== undefined ? controlledReport : ownReport
  const setReport = (next: ConflictReport | null) => {
    setOwnReport(next)
    onReportChange?.(next)
  }
  const [error, setError] = useState<string | null>(null)
  /** 键盘事件处理里读最新的候选（同一帧内连续的 keydown / keyup 不等重渲染） */
  const pending = useRef<Candidate | null>(null)
  /** 检测请求的轮次：新一轮录制开始后，上一轮迟到的检测结果作废 */
  const round = useRef(0)

  const stop = () => {
    pending.current = null
    setRecording(false)
    setHeld([])
    setCandidate(null)
  }
  const start = () => {
    if (disabled) return
    round.current += 1
    pending.current = null
    setReport(null)
    setError(null)
    setCandidate(null)
    setRecording(true)
  }

  // 录制中：通知 Rust 暂停全局键，并在 overlay 层模态截获全部按键（视图 / 全局快捷键都不响应）。
  // 结束录制与录制中卸载都走这里的清理，hotkey_record_end 不会漏掉
  useEffect(() => {
    if (!recording || forceRecording) return
    void hotkeyRecordBegin().catch(() => {})
    const uninstall = installKeyRouter()
    const unregister = registerScope('hotkey-recorder', [], { layer: 'overlay', modal: true })
    return () => {
      unregister()
      uninstall()
      void hotkeyRecordEnd().catch(() => {})
    }
  }, [recording, forceRecording])

  const commit = async (next: Candidate) => {
    stop()
    const mine = round.current
    try {
      const r = await check(target, next.combo)
      if (mine !== round.current) return
      setReport(r)
      const w = worst(r.conflicts)
      if (r.acceptable && (!w || w.level === 'info')) onChange(next.combo, false)
    } catch {
      if (mine !== round.current) return
      setReport(null)
      setError(t('hotkeys.recorder.check_failed'))
    }
  }

  const onKeyDown = (e: ReactKeyboardEvent<HTMLButtonElement>) => {
    if (!recording) {
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault()
        start()
      }
      return
    }
    e.preventDefault()
    e.stopPropagation()
    const native = e.nativeEvent
    if (native.isComposing || native.repeat) return
    const mods = modifiersOf(native)
    const anyMod = mods.ctrl || mods.alt || mods.shift || mods.meta
    if (isModifierEvent(native)) {
      if (!pending.current) setHeld(formatModifiers(mods, os))
      return
    }
    const code = codeOf(native)
    if (!anyMod && code === 'Escape') {
      stop()
      return
    }
    if (!anyMod && code === 'Backspace') {
      stop()
      onChange(null, false)
      return
    }
    if (!anyMod && !BARE_KEYS.test(code)) {
      setError(t('hotkeys.recorder.modifier_required'))
      return
    }
    const combo = comboFromEvent(native, os)
    if (!combo) return
    // 候选组合整串回显，抬起主键（或修饰键）时提交
    const next = { combo, code }
    pending.current = next
    setCandidate(next)
    setError(null)
  }

  const onKeyUp = (e: ReactKeyboardEvent<HTMLButtonElement>) => {
    if (!recording) return
    e.preventDefault()
    e.stopPropagation()
    const native = e.nativeEvent
    const next = pending.current
    if (next && (codeOf(native) === next.code || isModifierEvent(native))) {
      void commit(next)
      return
    }
    if (!next) setHeld(formatModifiers(modifiersOf(native), os))
  }

  const w = report ? worst(report.conflicts) : null
  const message = w ? tKey(w.message_key, w.params, t(`hotkeys.level.${w.level}`)) : error
  const tone = w?.level ?? (error ? 'warning' : null)
  return (
    <div className={cx('ui-recorder-wrap', className)} data-testid={testId}>
      <button
        ref={button}
        type="button"
        className="ui-recorder"
        data-recording={recording || undefined}
        data-tone={tone ?? undefined}
        // 无障碍名称带上当前组合的朗读文本：键帽在按钮里，但 aria-label 会覆盖它们（VoiceOver 只读得到动作名）
        aria-label={t('hotkeys.recorder.label_value', {
          label,
          combo: value ? hotkeySpeech(value, os) : t('hotkeys.recorder.none'),
        })}
        aria-pressed={recording}
        disabled={disabled}
        onClick={() => (recording ? stop() : start())}
        onKeyDown={onKeyDown}
        onKeyUp={onKeyUp}
        onBlur={() => {
          if (recording && !forceRecording) stop()
        }}
      >
        {recording ? (
          <m.span className="ui-recorder-live" {...pulse}>
            {candidate ? (
              <Kbd combo={candidate.combo} />
            ) : held.length > 0 ? (
              <span className="ui-kbd">
                {held.map((k) => (
                  <span key={k} className="ui-kbd-key">
                    {k}
                  </span>
                ))}
              </span>
            ) : (
              <span className="ui-recorder-prompt">{t('hotkeys.recorder.recording')}</span>
            )}
          </m.span>
        ) : value ? (
          <Kbd combo={value} />
        ) : (
          <span className="ui-recorder-placeholder">{t('hotkeys.recorder.none')}</span>
        )}
      </button>
      {recording ? (
        <span className="ui-recorder-hint">
          <Trans
            k="hotkeys.recorder.hint"
            nodes={{
              // i18n-ignore：两个节点参数之间的键名不是文案（检查器按「标签之后的单词」判断）
              esc: <Kbd combo="Escape" />,
              backspace: <Kbd combo="Backspace" />,
            }}
          />
        </span>
      ) : message ? (
        <output className="ui-recorder-message" data-tone={tone ?? undefined}>
          <span className="ui-recorder-level">{w ? t(`hotkeys.level.${w.level}`) : null}</span>
          {message}
          {report && w?.level === 'warning' ? (
            <Button
              size="sm"
              variant="ghost"
              onClick={() => {
                onChange(report.combo, true)
                setReport(null)
              }}
            >
              {t('hotkeys.recorder.force')}
            </Button>
          ) : null}
        </output>
      ) : null}
    </div>
  )
}
