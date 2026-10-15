/**
 * ErrorCard：面板内的错误卡片，不弹系统对话框。
 *
 * 结构：图标 · 原因（标题）· 建议（一行，可带参数）· 按钮 · 可折叠详情（错误码、请求 id、HTTP 状态、URL、服务端信息；
 * 不含正文，可复制）。规则：
 * - 按钮按错误码取舍（`ERROR_ACTIONS`），只显示调用方提供了回调的那些；
 * - `rate_limited`：按 `retry_after_s` 倒计时，归零前「重试」禁用；
 * - `quota`：不提供重试；
 * - `cancelled` / `param_unsupported` 不出卡片（返回 null）——前者在 StatusLine 显示「已取消」，后者只做弱提示；
 * - 兼容端点（`preferServerMessage`）优先显示服务端原文作为建议——只限确实收到了服务端响应的错误（有 HTTP 状态，或
 *   错误码属于 auth / not_found / quota / rate_limited / upstream / content_filter / too_long 等服务端错误）；
 *   network / timeout / internal / keychain 照常显示本地化建议，原文放进详情；
 * - 建议文案优先级：带参数的错误码文案（provider / url / status / 倒计时）> Rust 的 hint（`errors.hint.*`）> 错误码默认建议；
 *   有 subcode 时在下面补一行（地区限制、返回了网页、/v1 提示……）。
 * 出现动效 presets.cardIn；详情展开 presets.sectionExpand。
 */
import type { LucideIcon } from 'lucide-react'
import {
  ChevronDown,
  CircleAlert,
  Clock,
  KeyRound,
  ShieldAlert,
  Unplug,
  Wallet,
  WifiOff,
} from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useEffect, useId, useState } from 'react'
import { useT, useTKey } from '../i18n/useT'
import type { ErrorCode, LlmError } from '../ipc/types'
import { usePreset } from '../motion/presets'
import { Button } from './Button'
import { CopyButton } from './CopyButton'
import { cx } from './cx'
import { Icon } from './Icon'

export type ErrorAction = 'retry' | 'open_settings' | 'choose_model' | 'switch_model'

/** 各错误码提供的按钮 */
export const ERROR_ACTIONS: Record<ErrorCode, readonly ErrorAction[]> = {
  auth: ['open_settings'],
  not_found: ['open_settings'],
  no_vision: ['choose_model', 'open_settings'],
  image_too_large: ['retry', 'open_settings'],
  param_unsupported: [],
  too_long: ['switch_model'],
  quota: ['open_settings'],
  rate_limited: ['retry'],
  upstream: ['retry'],
  content_filter: ['switch_model'],
  network: ['retry'],
  timeout: ['retry', 'open_settings'],
  cancelled: [],
  protocol: ['open_settings'],
  not_configured: ['open_settings'],
  keychain: ['retry', 'open_settings'],
  internal: ['retry'],
}

const ICONS: Partial<Record<ErrorCode, LucideIcon>> = {
  auth: KeyRound,
  quota: Wallet,
  rate_limited: Clock,
  timeout: Clock,
  network: WifiOff,
  content_filter: ShieldAlert,
  protocol: Unplug,
  keychain: KeyRound,
}

/**
 * 一定来自服务端响应的错误码：兼容端点只有这些（或带 HTTP 状态的）错误才优先显示服务端原文。network / timeout /
 * internal / keychain 这类本地错误没有服务端响应，`message` 是客户端的英文报错（「connection refused」），
 * 不能盖掉本地化的建议、Rust 的 hint 与 subcode 提示
 */
const SERVER_CODES: ReadonlySet<ErrorCode> = new Set<ErrorCode>([
  'auth',
  'not_found',
  'quota',
  'rate_limited',
  'upstream',
  'content_filter',
  'too_long',
  'no_vision',
  'image_too_large',
])
/** 本地产生的错误码：即使带了状态码也不显示原文 */
const LOCAL_CODES: ReadonlySet<ErrorCode> = new Set<ErrorCode>([
  'network',
  'timeout',
  'internal',
  'keychain',
  'not_configured',
  'cancelled',
  'protocol',
])

/** 兼容端点的服务端原文能不能当作建议显示 */
export function serverMessageShown(error: LlmError, preferServerMessage: boolean): boolean {
  if (!preferServerMessage || !error.message || LOCAL_CODES.has(error.code)) return false
  return error.status !== null || SERVER_CODES.has(error.code)
}

/** 不出卡片的错误码 */
export function isCardError(code: ErrorCode): boolean {
  return code !== 'cancelled' && code !== 'param_unsupported'
}

export interface ErrorCardProps {
  error: LlmError
  /** 服务名（auth 文案里的「{provider}」） */
  providerName?: string | null
  /** 兼容端点：优先显示服务端原文 */
  preferServerMessage?: boolean
  onRetry?: () => void
  onOpenSettings?: () => void
  onChooseModel?: () => void
  onSwitchModel?: () => void
  /** 初始展开详情（展示页用） */
  defaultExpanded?: boolean
  className?: string
  'data-testid'?: string
}

/** rate_limited 的倒计时（秒）：1 Hz 更新，归零后停止 */
function useCountdown(seconds: number | null): number {
  const initial = Math.max(0, Math.ceil(seconds ?? 0))
  const [state, setState] = useState({ seconds, left: initial })
  // 秒数变了（新的错误）：在渲染中直接重置（React 推荐的「随 props 调整 state」写法）
  if (state.seconds !== seconds) setState({ seconds, left: initial })
  useEffect(() => {
    if (!seconds || seconds <= 0) return
    const started = performance.now()
    const tick = setInterval(() => {
      const left = Math.max(0, Math.ceil(seconds - (performance.now() - started) / SECOND_MS))
      setState((s) => (s.seconds === seconds ? { seconds, left } : s))
      if (left <= 0) clearInterval(tick)
    }, SECOND_MS)
    return () => clearInterval(tick)
  }, [seconds])
  return state.seconds === seconds ? state.left : initial
}

const SECOND_MS = 1000

export function ErrorCard({
  error,
  providerName,
  preferServerMessage = false,
  onRetry,
  onOpenSettings,
  onChooseModel,
  onSwitchModel,
  defaultExpanded = false,
  className,
  'data-testid': testId = 'error-card',
}: ErrorCardProps) {
  const t = useT()
  const tKey = useTKey()
  const enter = usePreset('cardIn')
  const expand = usePreset('sectionExpand')
  const [expanded, setExpanded] = useState(defaultExpanded)
  const detailsId = useId()
  const code = error.code
  const countdown = useCountdown(code === 'rate_limited' ? error.retry_after_s : null)
  if (!isCardError(code)) return null

  const title = t(`errors.code.${code}.title`)
  const hint = error.hint ? tKey(error.hint) : null
  const serverSaid = serverMessageShown(error, preferServerMessage)
  let suggestion: string
  if (serverSaid) suggestion = error.message
  else if (code === 'auth')
    suggestion = providerName
      ? t('errors.code.auth.detail', { provider: providerName })
      : (hint ?? t('errors.code.auth.detail_generic'))
  else if (code === 'not_found')
    suggestion = error.url
      ? t('errors.code.not_found.detail', { url: error.url })
      : (hint ?? t('errors.code.not_found.detail_generic'))
  else if (code === 'upstream')
    suggestion =
      error.status !== null
        ? t('errors.code.upstream.detail', { status: error.status })
        : (hint ?? t('errors.code.upstream.detail_generic'))
  else if (code === 'rate_limited')
    // 有重试按钮时倒计时只在按钮上（「17 秒后重试」），说明行讲原因；没有按钮时说明行自己带倒计时
    suggestion = onRetry
      ? t('errors.code.rate_limited.reason')
      : error.retry_after_s !== null && error.retry_after_s > 0
        ? countdown > 0
          ? t('errors.code.rate_limited.detail', { n: countdown })
          : t('errors.code.rate_limited.detail_ready')
        : t('errors.code.rate_limited.detail_generic')
  else suggestion = hint ?? t(`errors.code.${code}.detail`)
  const sub = error.subcode ? t(`errors.subcode.${error.subcode}`) : null

  const handlers: Record<ErrorAction, (() => void) | undefined> = {
    retry: onRetry,
    open_settings: onOpenSettings,
    choose_model: onChooseModel,
    switch_model: onSwitchModel,
  }
  const actions = ERROR_ACTIONS[code].filter((a) => handlers[a])
  const waiting = code === 'rate_limited' && countdown > 0

  const rows: Array<[string, string]> = [
    [t('errors.details.code'), error.subcode ? `${code} · ${error.subcode}` : code],
  ]
  if (error.upstream_request_id)
    rows.push([t('errors.details.request_id'), error.upstream_request_id])
  if (error.status !== null) rows.push([t('errors.details.status'), String(error.status)])
  if (error.url) rows.push([t('errors.details.url'), error.url])
  // 原文没有当作建议显示时放进详情（本地错误的客户端报错也在这里，可复制）
  if (error.message && !serverSaid) rows.push([t('errors.details.message'), error.message])
  const detailText = rows.map(([k, v]) => `${k}: ${v}`).join('\n')

  return (
    <m.section
      role="alert"
      className={cx('ui-errorcard', className)}
      data-code={code}
      data-testid={testId}
      {...enter}
    >
      <div className="ui-errorcard-head">
        <span className="ui-errorcard-icon" aria-hidden="true">
          <Icon icon={ICONS[code] ?? CircleAlert} size={16} />
        </span>
        <div className="ui-errorcard-text">
          <p className="ui-errorcard-title">{title}</p>
          <p className="ui-errorcard-suggestion">{suggestion}</p>
          {sub ? <p className="ui-errorcard-sub">{sub}</p> : null}
        </div>
      </div>
      {/* 没有操作按钮时「详情」紧贴正文、与文字左对齐，不在卡片右下角留一整行空白 */}
      <div className="ui-errorcard-actions" data-bare={actions.length === 0 || undefined}>
        {actions.map((a, i) => {
          const variant = i === 0 ? 'primary' : 'secondary'
          const labelText =
            a === 'retry'
              ? waiting
                ? t('errors.action.retry_in', { n: countdown })
                : t('errors.action.retry')
              : t(`errors.action.${a}`)
          return (
            <Button
              key={a}
              size="sm"
              variant={variant}
              disabled={a === 'retry' && waiting}
              onClick={handlers[a]}
              data-testid={`${testId}-${a}`}
            >
              {labelText}
            </Button>
          )
        })}
        <button
          type="button"
          className="ui-disclosure"
          aria-expanded={expanded}
          aria-controls={detailsId}
          onClick={() => setExpanded((e) => !e)}
          data-testid={`${testId}-details-toggle`}
        >
          {expanded ? t('common.action.hide_details') : t('common.action.details')}
          <Icon icon={ChevronDown} size={12} className="ui-disclosure-caret" />
        </button>
      </div>
      <AnimatePresence initial={false}>
        {expanded ? (
          <m.div key="details" id={detailsId} className="ui-errorcard-details-clip" {...expand}>
            <div className="ui-errorcard-details">
              <dl>
                {rows.map(([k, v]) => (
                  <div key={k} className="ui-errorcard-row">
                    <dt>{k}</dt>
                    <dd className="ui-selectable">{v}</dd>
                  </div>
                ))}
              </dl>
              <CopyButton
                text={detailText}
                label={t('errors.details.copy')}
                data-testid={`${testId}-copy`}
              />
            </div>
          </m.div>
        ) : null}
      </AnimatePresence>
    </m.section>
  )
}
