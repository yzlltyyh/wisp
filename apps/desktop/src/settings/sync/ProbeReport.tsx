/**
 * 「测试连接」的分步报告：连接与登录 · 读写权限 · 服务器上的数据（尚无同步数据 / 已有 N 台设备的同步数据 · 最近更新于…）。
 * 与模型服务的测试报告同一套样式（`.st-test` / `.st-step`）：运行中三步都转圈，结果回来只换图标与第二行；
 * 失败的那一步写出「原因 + 建议」，技术细节（状态码、超时等固定词汇）收进可折叠的详情；地址、账号、文件名都不出现。
 * 连接信息改过之后旧结果变淡并提示重新测试。
 */
import { Check, Minus, RotateCw, X } from 'lucide-react'
import { m } from 'motion/react'
import { useT } from '../../shared/i18n/useT'
import { relativeTime } from '../../shared/i18n/relativeTime'
import type { SyncErrorKind, SyncProbe } from '../../shared/ipc/types'
import { usePreset } from '../../shared/motion/presets'
import { AutoHeight, Button, Icon, Spinner } from '../../shared/ui'
import { Disclosure } from '../components/Disclosure'
import { failedStep } from './model'

export type ProbeRun =
  | { phase: 'idle' }
  | { phase: 'running' }
  | { phase: 'done'; probe: SyncProbe }
  /** 整个调用被拒绝（参数校验：地址不合规等）：已翻译的原因 */
  | { phase: 'error'; message: string }

const CONNECT_ADVICE = ['auth', 'credentials_missing', 'https_required', 'keychain'] as const
type ConnectAdvice = (typeof CONNECT_ADVICE)[number]
function isConnectAdvice(kind: SyncErrorKind): kind is ConnectAdvice {
  return (CONNECT_ADVICE as readonly SyncErrorKind[]).includes(kind)
}

type StepState = 'running' | 'ok' | 'failed' | 'skipped' | 'idle'

function StepIcon({ state }: { state: StepState }) {
  if (state === 'running') return <Spinner size={14} className="st-step-icon" />
  if (state === 'ok' || state === 'failed')
    return (
      <span
        className="st-step-icon"
        data-ok={state === 'ok' || undefined}
        data-failed={state === 'failed' || undefined}
      >
        <Icon icon={state === 'ok' ? Check : X} size={12} />
      </span>
    )
  return <Icon icon={Minus} size={14} className="st-step-icon" />
}

function Step({ title, state, detail }: { title: string; state: StepState; detail: string }) {
  return (
    <div className="st-step" data-state={state}>
      <StepIcon state={state} />
      <span className="st-step-main">
        <span className="st-step-title">{title}</span>
        {/* 第二行始终占位（运行中是脉冲的占位条），结果回来时高度不变 */}
        <span className="st-sync-step-detail">
          {state === 'running' ? (
            <span className="st-step-url-pending" aria-hidden="true" />
          ) : (
            detail || '\u00a0'
          )}
        </span>
      </span>
    </div>
  )
}

export function ProbeReport({
  run,
  stale,
  now,
  onRetry,
}: {
  run: ProbeRun
  /** 结论是在另一组地址 / 账号 / 密码下得出的 */
  stale: boolean
  now: number
  onRetry: () => void
}) {
  const t = useT()
  const fade = usePreset('footerIn')
  const cardIn = usePreset('cardIn')
  const running = run.phase === 'running'
  const probe = run.phase === 'done' ? run.probe : null
  const failedAt = probe && !probe.ok ? failedStep(probe.error) : null
  const states: StepState[] = [0, 1, 2].map((i) => {
    if (running) return 'running'
    if (run.phase === 'error') return i === 0 ? 'failed' : 'skipped'
    if (!probe) return 'idle'
    if (failedAt === null) return i === 2 && probe.vault && !probe.vault.supported ? 'failed' : 'ok'
    return i < failedAt ? 'ok' : i === failedAt ? 'failed' : 'skipped'
  })
  const skipped = t('settings.providers.step_skipped')
  const errorReason = (kind: NonNullable<SyncProbe['error']>) =>
    t(`settings.sync.error.${kind}.reason`)
  const detail = (i: number): string => {
    const state = states[i]
    if (state === 'skipped') return skipped
    if (state === 'failed' && probe?.error) return errorReason(probe.error)
    // 测试本身没能发出（例如读不到已保存的密码）：没有结果可写，原因在报告下面
    if (!probe || (state !== 'ok' && state !== 'failed')) return ''
    if (i === 0) return t('settings.sync.connect.step_ok.connect')
    if (i === 1) return t('settings.sync.connect.step_ok.write')
    const vault = probe?.vault ?? null
    if (!vault) return state === 'ok' ? t('settings.sync.connect.data.none') : ''
    if (!vault.supported) return t('settings.sync.connect.data.unsupported')
    if (vault.restorable) return t('settings.sync.connect.data.restorable')
    const devices = t('settings.sync.connect.data.devices', {
      count: vault.device_count,
    })
    return vault.last_modified_ms === null
      ? devices
      : `${devices} · ${t('settings.sync.connect.data.updated', { time: relativeTime(vault.last_modified_ms, now) })}`
  }
  // 失败的那一步只写原因；建议放在报告下面（一行写不下两句）。通用建议里提到已连接页的按钮（「更新密码…」
  // 「立即同步」）的几种，在这个表单里换成就地可做的说法
  const advice = probe?.error
    ? isConnectAdvice(probe.error)
      ? t(`settings.sync.connect.advice.${probe.error}`)
      : t(`settings.sync.error.${probe.error}.advice`)
    : null

  return (
    // 高度随「状态」过渡（未测试 → 测试中 → 结果），过渡中结果又长高（建议、详情）时追到新高度，不会先裁掉再跳一下
    <AutoHeight swapKey={run.phase}>
      {run.phase !== 'idle' ? (
        <m.div key="report" {...fade}>
          <div
            className="st-test"
            data-testid="sync-probe-report"
            data-stale={stale || undefined}
            aria-live="polite"
          >
            {stale ? (
              <p className="st-test-stale" data-testid="sync-probe-stale">
                <span>{t('settings.sync.connect.test_stale')}</span>
                <Button size="sm" variant="ghost" icon={RotateCw} onClick={onRetry}>
                  {t('settings.providers.test_again')}
                </Button>
              </p>
            ) : null}
            <Step
              title={t('settings.sync.connect.step.connect')}
              state={states[0] ?? 'idle'}
              detail={detail(0)}
            />
            <Step
              title={t('settings.sync.connect.step.write')}
              state={states[1] ?? 'idle'}
              detail={detail(1)}
            />
            <Step
              title={t('settings.sync.connect.step.data')}
              state={states[2] ?? 'idle'}
              detail={detail(2)}
            />
            {run.phase === 'error' ? (
              <m.p
                className="st-test-error"
                role="alert"
                data-testid="sync-probe-error"
                {...cardIn}
              >
                <span className="st-grow">{run.message}</span>
              </m.p>
            ) : null}
            {advice ? (
              <m.p
                className="st-sync-advice"
                role="alert"
                data-testid="sync-probe-advice"
                {...cardIn}
              >
                {advice}
              </m.p>
            ) : null}
            {probe?.error_detail ? (
              <Disclosure title={t('settings.about.update.error_details')} variant="plain">
                <pre className="st-notes st-error-detail" data-testid="sync-probe-detail">
                  {probe.error_detail}
                </pre>
              </Disclosure>
            ) : null}
          </div>
        </m.div>
      ) : null}
    </AutoHeight>
  )
}
