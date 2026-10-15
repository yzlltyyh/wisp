/**
 * 测试连接：`provider_test` 同时打 `GET /models` 与一条 max_tokens=1 的流式请求，这里分步展示：
 * ① 获取模型列表 ② 流式请求（模型、URL、耗时）；失败的一步用 ErrorCard 给出原因与建议（404 → 「添加 / 去掉 /v1」等 hint
 * 由 wisp-llm 下发；无效 Key → auth 卡片）；只有模型列表失败（partial）时标黄：可以手动添加模型。
 * 错误卡片出现后滚到可见位置（窗口矮时它常在视野外）。连接配置改过之后旧结果变淡并提示重新测试（hooks.ts 的 stale）。
 *
 * - 每一步的第二行（请求方法 + URL）从运行时就占好位（脉冲的占位条），结果回来只换图标、URL 与耗时，报告高度不变；
 * - Key 无效（auth）不再用错误卡片重复说一遍：Key 输入框下的就地提示 + 步骤行的状态码已经说清楚（KeyField 负责把
 *   整个 Key 字段滚入视野）；其他失败照旧给出带建议的错误卡片。
 */
import { Check, Minus, RotateCw, X } from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useRef } from 'react'
import { formatDuration } from '../../shared/i18n/format'
import { useT, useTKey } from '../../shared/i18n/useT'
import type { ProbeResult, ProviderTestReport } from '../../shared/ipc/types'
import { usePreset } from '../../shared/motion/presets'
import { useReducedMotionPref } from '../../shared/motion/useReducedMotionPref'
import { Badge, Button, ErrorCard, Icon, Spinner } from '../../shared/ui'

export type TestRun =
  | { phase: 'idle' }
  | { phase: 'running' }
  | { phase: 'done'; report: ProviderTestReport }
  | { phase: 'error'; message: string }

/**
 * 流式探针有没有真正发出去：新服务还没有模型、/models 又失败时没有可测的模型，探针不会发（`model_used` 为空，
 * stream 里只是一条 `not_configured` 占位）
 */
export function streamSkipped(report: ProviderTestReport): boolean {
  return report.model_used === null && !report.stream.ok
}

/**
 * 决定结论的那一步：优先流式请求（真正决定能否翻译）；探针没发出去时看 /models——Key 填错、Base URL 写错时
 * 这是唯一有服务端响应的一步（401 → auth、404 → 「添加 / 去掉 /v1」），不能被 `not_configured` 占位盖掉
 */
export function failedProbe(report: ProviderTestReport): ProbeResult | null {
  if (streamSkipped(report) && !report.models.ok) return report.models
  if (!report.stream.ok) return report.stream
  if (!report.models.ok) return report.models
  return null
}

/** 最近一次测试是不是被服务拒绝了 Key（401 / 403 → auth） */
export function isAuthFailure(run: TestRun): boolean {
  if (run.phase !== 'done' || run.report.overall !== 'failed') return false
  const report = run.report
  const err = failedProbe(report)?.error ?? report.stream.error ?? report.models.error
  return err?.code === 'auth'
}

export function TestVerdictBadge({ run }: { run: TestRun }) {
  const t = useT()
  if (run.phase === 'running')
    return <Badge tone="neutral">{t('settings.providers.testing')}</Badge>
  if (run.phase === 'error')
    return <Badge tone="danger">{t('settings.providers.test_failed')}</Badge>
  if (run.phase !== 'done') return null
  const r = run.report
  if (r.overall === 'ok')
    return (
      <Badge tone="success" dot data-testid="test-verdict">
        {t('settings.providers.test_ok', { ms: formatDuration(r.stream.elapsed_ms) })}
      </Badge>
    )
  if (r.overall === 'partial')
    return (
      <Badge tone="warning" dot data-testid="test-verdict">
        {t('settings.providers.test_partial')}
      </Badge>
    )
  return (
    <Badge tone="danger" dot data-testid="test-verdict">
      {t('settings.providers.test_failed')}
    </Badge>
  )
}

function StepIcon({
  probe,
  running,
  skipped,
}: {
  probe: ProbeResult | null
  running: boolean
  skipped: boolean
}) {
  if (running) return <Spinner size={14} className="st-step-icon" />
  if (!probe || skipped) return <Icon icon={Minus} size={14} className="st-step-icon" />
  return (
    <span
      className="st-step-icon"
      data-ok={probe.ok || undefined}
      data-failed={!probe.ok || undefined}
    >
      <Icon icon={probe.ok ? Check : X} size={12} />
    </span>
  )
}

function Step({
  title,
  probe,
  running,
  method,
  extra,
  skipped = false,
}: {
  title: string
  probe: ProbeResult | null
  running: boolean
  method: string
  extra?: string | null
  /** 这一步没有执行（前一步失败、没有可测的模型）：显示「未执行」，不当作失败 */
  skipped?: boolean
}) {
  const t = useT()
  return (
    <div
      className="st-step"
      data-state={
        running ? 'running' : skipped ? 'skipped' : probe ? (probe.ok ? 'ok' : 'failed') : 'idle'
      }
    >
      <StepIcon probe={probe} running={running} skipped={skipped} />
      <span className="st-step-main">
        <span className="st-step-title">
          {title}
          {extra ? <span className="st-step-extra">{extra}</span> : null}
        </span>
        {/* 第二行始终占位：运行中是脉冲的占位条，结果回来时高度不变 */}
        {probe ? (
          <span className="st-step-url">
            {method} {probe.url}
          </span>
        ) : (
          <span className="st-step-url" aria-hidden="true">
            {running ? <span className="st-step-url-pending" /> : '\u00a0'}
          </span>
        )}
      </span>
      <span className="st-step-time">
        {running
          ? t('settings.providers.testing')
          : skipped
            ? t('settings.providers.step_skipped')
            : probe
              ? probe.status !== null && !probe.ok
                ? t('settings.providers.http_status', { status: probe.status })
                : formatDuration(probe.elapsed_ms)
              : null}
      </span>
    </div>
  )
}

export function TestReport({
  run,
  stale = false,
  providerName,
  onRetry,
}: {
  run: TestRun
  /** 结论是在另一套连接配置下得出的 */
  stale?: boolean
  providerName: string
  onRetry: () => void
}) {
  const t = useT()
  const tKey = useTKey()
  const expand = usePreset('sectionExpand')
  const cardIn = usePreset('cardIn')
  const reduced = useReducedMotionPref()
  const cardRef = useRef<HTMLDivElement>(null)
  const errorRef = useRef<HTMLDivElement>(null)
  const visible = run.phase !== 'idle'
  const report = run.phase === 'done' ? run.report : null
  const running = run.phase === 'running'
  // 优先展示流式请求的错误（真正决定能否翻译的一步）；探针没发出去时展示 /models 的错误
  const failed = report ? failedProbe(report) : null
  const skipped = report !== null && streamSkipped(report)
  const notice = report?.notice ? tKey(report.notice) : null
  // 卡片 / 错误出现后滚到可见位置（引导窗口只有 580 高、不能调整大小，它们常在视野外）
  const reveal = (el: HTMLElement | null) =>
    el?.scrollIntoView?.({ block: 'nearest', behavior: reduced ? 'auto' : 'smooth' })
  return (
    <AnimatePresence initial={false}>
      {visible ? (
        <m.div key="report" className="st-clip" {...expand}>
          <div
            className="st-test"
            data-testid="test-report"
            data-stale={stale || undefined}
            aria-live="polite"
          >
            {stale ? (
              <p className="st-test-stale" data-testid="test-stale">
                <span>{t('settings.providers.test_stale')}</span>
                <Button size="sm" variant="ghost" icon={RotateCw} onClick={onRetry}>
                  {t('settings.providers.test_again')}
                </Button>
              </p>
            ) : null}
            <Step
              title={t('settings.providers.step_models')}
              probe={report?.models ?? null}
              running={running}
              method="GET"
              skipped={run.phase === 'error'}
            />
            <Step
              title={t('settings.providers.step_stream')}
              probe={report?.stream ?? null}
              running={running}
              method="POST"
              extra={report?.model_used ?? null}
              skipped={skipped || run.phase === 'error'}
            />
            {run.phase === 'error' ? (
              // 整个调用被拒绝（例如钥匙串授权被拒）：两步都没有执行，原因和「重试」放在一起，并滚入视野
              <m.div
                ref={errorRef}
                className="st-test-error"
                role="alert"
                data-testid="test-error"
                {...cardIn}
                onAnimationComplete={() => reveal(errorRef.current)}
              >
                <span className="st-grow">{run.message}</span>
                <Button size="sm" variant="ghost" icon={RotateCw} onClick={onRetry}>
                  {t('errors.action.retry')}
                </Button>
              </m.div>
            ) : null}
            {notice ? (
              // 测试本身之外的情况（凭据存储读不出来，本次没带 Key）：Rust 下发 i18n key，不认识的 key 不显示
              <m.p className="st-test-hint" data-testid="test-notice" {...cardIn}>
                {notice}
              </m.p>
            ) : null}
            {report?.overall === 'partial' ? (
              <m.p className="st-test-hint" {...cardIn}>
                {t('settings.providers.test_partial_hint')}
              </m.p>
            ) : null}
            {failed?.error && report?.overall === 'failed' && failed.error.code !== 'auth' ? (
              <m.div
                ref={cardRef}
                {...cardIn}
                className="st-test-card"
                onAnimationComplete={() => reveal(cardRef.current)}
              >
                <ErrorCard error={failed.error} providerName={providerName} onRetry={onRetry} />
              </m.div>
            ) : null}
          </div>
        </m.div>
      ) : null}
    </AnimatePresence>
  )
}
