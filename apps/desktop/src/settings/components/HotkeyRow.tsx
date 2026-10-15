/**
 * 一行全局快捷键：录制器（组件库的 HotkeyRecorder：录制期间 Rust 注销全部全局键、三级冲突提示、
 * 「仍然使用」）、注册状态（已改用备选键 / 注册失败 + 原因，写明原来的组合：「⌥D：已被其他程序占用。已改用 ⌃⌥D」）、
 * 「测试」（hotkeyTest.ts：同一时刻只有一个目标在测试，10 s 内收到 `hotkey:test_received` 显示「已收到」，超时提示可能被占用
 * 并建议换键；测试期间 Rust 不执行动作）、单项恢复默认。
 *
 * `variant="embedded"`：引导「试一试」里就地换键——只有动作名、录制器（标红显示没注册上的组合）与恢复默认，原因由外面那一行说明，
 * 不重复徽标与原因、不放「测试」。
 */
import { CircleCheck, RotateCcw, Timer, TriangleAlert } from 'lucide-react'
import { m } from 'motion/react'
import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react'
import { usePlatform } from '../../shared/hooks/usePlatform'
import { Trans } from '../../shared/i18n/Trans'
import { useT, useTKey } from '../../shared/i18n/useT'
import { api, WispIpcError } from '../../shared/ipc'
import type { ConflictReport, HotkeyStatus, HotkeyTarget } from '../../shared/ipc/types'
import { formatHotkey } from '../../shared/keys/formatHotkey'
import { usePreset } from '../../shared/motion/presets'
import { Badge, Button, HotkeyRecorder, Icon, IconButton, Kbd } from '../../shared/ui'
import { ipcErrorText } from '../lib/errors'
import { HOTKEY_DEFAULTS, preferredCombo, shownCombo } from '../lib/hotkeyDefaults'
import { notify, setHotkeys } from '../store'
import {
  cancelHotkeyTest,
  invalidateHotkeyTest,
  startHotkeyTest,
  useHotkeyTestState,
} from './hotkeyTest'

export { sameTarget } from './hotkeyTest'

/** 状态徽标（与扩展触发的徽标一样都带圆点） */
export function stateBadge(s: HotkeyStatus | undefined, t: ReturnType<typeof useT>): ReactNode {
  if (!s) return null
  switch (s.state) {
    case 'fallback':
      return (
        <Badge tone="warning" dot>
          {t('hotkeys.state.fallback')}
        </Badge>
      )
    case 'failed':
      return (
        <Badge tone="danger" dot>
          {t('hotkeys.state.failed')}
        </Badge>
      )
    // suspended：录制快捷键期间 Rust 暂停全部全局键（`hotkeys.reason.recording`），是一闪而过的状态，不打扰
    case 'unavailable':
      return (
        <Badge tone="neutral" dot>
          {t('hotkeys.state.unavailable')}
        </Badge>
      )
    default:
      return null
  }
}

/**
 * 没注册上 / 改用备选键的说明：「⌥D：已被其他程序占用。已改用 ⌃⌥D」。原因只有一行
 */
export function HotkeyIssue({ status }: { status: HotkeyStatus | undefined }) {
  const tKey = useTKey()
  const os = usePlatform()
  if (!status || (status.state !== 'fallback' && status.state !== 'failed')) return null
  const reason = status.reason_key ? tKey(status.reason_key) : null
  const preferred = preferredCombo(status, os)
  if (!reason) return null
  if (!preferred) return <>{reason}</>
  const combo = <Kbd combo={preferred} variant="inline" />
  return status.state === 'fallback' && status.effective ? (
    <Trans
      k="settings.hotkeys.fallback_note"
      params={{ reason }}
      nodes={{ combo, fallback: <Kbd combo={status.effective} variant="inline" /> }}
    />
  ) : (
    <Trans k="settings.hotkeys.failed_note" params={{ reason }} nodes={{ combo }} />
  )
}

export function HotkeyRow({
  target,
  label,
  status,
  disabled = false,
  description,
  variant = 'full',
}: {
  target: HotkeyTarget
  label: string
  status: HotkeyStatus | undefined
  disabled?: boolean
  description?: ReactNode
  variant?: 'full' | 'embedded'
}) {
  const t = useT()
  const tKey = useTKey()
  const os = usePlatform()
  const fade = usePreset('footerIn')
  const combo = shownCombo(status, os)
  // 测试结果绑定测试时的组合：重新录制 / 恢复默认 / 改用备选键之后，旧结果不再显示
  const test = useHotkeyTestState(target, combo)
  const failed = status?.state === 'failed'
  const embedded = variant === 'embedded'
  const testId =
    target.kind === 'action' ? `hotkey-${target.action}` : `hotkey-style-${target.style_id}`

  // 离开页面时结束这一行的测试（否则 Rust 会一直处于「测试中、不执行动作」，直到它自己 15 s 后结束）
  const targetRef = useRef(target)
  useLayoutEffect(() => {
    targetRef.current = target
  })
  useEffect(() => () => cancelHotkeyTest(targetRef.current), [])
  useEffect(() => invalidateHotkeyTest(targetRef.current, combo), [combo])

  /** Rust 拒绝时重新检测得到的完整冲突报告：交给录制器在原位显示 */
  const [rejected, setRejected] = useState<ConflictReport | null>(null)
  const set = async (next: string | null, force: boolean) => {
    try {
      setHotkeys(await api.hotkeySet(target, next ?? 'none', force))
      setRejected(null)
    } catch (error) {
      // 冲突被拒绝（录制期间系统里刚出现的占用等）：重新 hotkey_check，在录制器里显示完整的报告与「仍然使用」
      if (next && error instanceof WispIpcError && error.code === 'conflict') {
        const report = await api.hotkeyCheck(target, next).catch(() => null)
        if (report && report.conflicts.length > 0) {
          setRejected(report)
          return
        }
      }
      // 其余情况：Rust 下发 `hotkeys.conflict.*` 的 key；带参数的文案在这里补上组合与动作名
      notify(
        ipcErrorText(error, 'hotkey_set', {
          combo: next ? formatHotkey(next, os) : '',
          action: label,
        }),
        'danger',
      )
    }
  }

  const start = async () => {
    try {
      await startHotkeyTest(target, combo)
    } catch (error) {
      notify(ipcErrorText(error, 'hotkey_test_begin'), 'danger')
    }
  }

  // 测试超时的是默认首选键：可以一键改用这个动作的备选键（⌥D → ⌃⌥D）
  const fallback =
    test.phase === 'timeout' &&
    target.kind === 'action' &&
    status?.binding === 'default' &&
    combo === HOTKEY_DEFAULTS[os][target.action].preferred
      ? HOTKEY_DEFAULTS[os][target.action].fallback
      : null

  const unavailableReason =
    status?.state === 'unavailable' && status.reason_key ? tKey(status.reason_key) : null
  const issue =
    status?.state === 'fallback' || status?.state === 'failed' ? (
      <HotkeyIssue status={status} />
    ) : null
  const note =
    test.phase === 'waiting' ? (
      <span className="st-test-note" data-tone="accent">
        <Icon icon={Timer} size={13} />
        {t('settings.hotkeys.test_press')}
        {combo ? <Kbd combo={combo} variant="inline" /> : null}
        <span className="st-tabular">{t('settings.hotkeys.test_left', { n: test.left })}</span>
      </span>
    ) : test.phase === 'received' ? (
      <span className="st-test-note" data-tone="success">
        <Icon icon={CircleCheck} size={13} />
        {t('hotkeys.recorder.test_received')}
      </span>
    ) : test.phase === 'timeout' ? (
      <span className="st-test-note" data-tone="warning">
        <Icon icon={TriangleAlert} size={13} />
        {fallback
          ? t('settings.hotkeys.test_timeout_fallback')
          : t('settings.hotkeys.test_timeout')}
        {fallback ? (
          // 超时建议改用备选——默认绑定直接给出这个动作的备选键，一键换上
          <Button
            size="sm"
            variant="ghost"
            className="st-test-fallback"
            onClick={() => void set(fallback, false)}
            data-testid={`${testId}-use-fallback`}
          >
            {t('settings.hotkeys.use_fallback', { combo: formatHotkey(fallback, os) })}
          </Button>
        ) : null}
      </span>
    ) : !embedded && !disabled && issue ? (
      // 原因是一句带键帽的文字：普通行内排版（不能用 inline-flex，否则每段文字各占一列）
      <span className="st-issue-note" data-tone={failed ? 'danger' : 'warning'}>
        {issue}
      </span>
    ) : !embedded && !disabled && unavailableReason ? (
      <span className="st-test-note">{unavailableReason}</span>
    ) : (
      description
    )

  return (
    <div
      className="st-hotkey-row"
      data-testid={testId}
      data-state={status?.state}
      data-variant={variant}
    >
      <div className="st-hotkey-text">
        <span className="st-hotkey-label">
          {label}
          {embedded ? null : stateBadge(status, t)}
        </span>
        {note ? (
          <m.span key={test.phase} className="st-hotkey-desc" {...fade}>
            {note}
          </m.span>
        ) : null}
      </div>
      <div className="st-hotkey-controls">
        <HotkeyRecorder
          target={target}
          value={combo}
          label={t('hotkeys.recorder.label', { action: label })}
          disabled={disabled}
          onChange={(next, force) => void set(next, force)}
          report={rejected ?? undefined}
          onReportChange={(r) => {
            if (r === null) setRejected(null)
          }}
          className={failed ? 'st-recorder-failed' : undefined}
          data-testid={`${testId}-recorder`}
        />
        {embedded ? null : (
          <Button
            size="sm"
            variant="ghost"
            className="st-swap-btn"
            disabled={test.phase !== 'waiting' && (disabled || !combo || failed)}
            onClick={() => (test.phase === 'waiting' ? cancelHotkeyTest(target) : void start())}
            data-testid={`${testId}-test`}
          >
            {/* 两种文案叠放、按较宽的那个占位：「测试」⇄「取消」切换时按钮宽度不变，录制器不左右跳 */}
            <span className="st-swap" data-active={test.phase === 'waiting' ? 'b' : 'a'}>
              <span data-slot="a" aria-hidden={test.phase === 'waiting' || undefined}>
                {t('hotkeys.recorder.test')}
              </span>
              <span data-slot="b" aria-hidden={test.phase !== 'waiting' || undefined}>
                {t('common.action.cancel')}
              </span>
            </span>
          </Button>
        )}
        {target.kind === 'action' && !(embedded && status?.binding === 'default') ? (
          <IconButton
            icon={RotateCcw}
            label={t('hotkeys.recorder.restore')}
            size="sm"
            disabled={disabled || status?.binding === 'default'}
            onClick={() => void set('default', false)}
            data-testid={`${testId}-restore`}
          />
        ) : null}
      </div>
    </div>
  )
}
