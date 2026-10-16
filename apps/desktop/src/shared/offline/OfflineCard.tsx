/**
 * 面板里的「离线模型还没下载 / 正在下载」卡片（翻译视图与 OCR 视图共用，替代 NotReadyCard 的默认内容）：
 *
 * - 没下载（或损坏、失败、已暂停）：说明里注明大小，主按钮就地开始下载（「下载 · 1.2 GB」/「继续下载」/「重试」），
 *   次按钮「切换模型」；下载失败时说明写具体原因（磁盘空间不足、校验没有通过……，与设置页同一套文案）；
 * - 下载前的适配确认（与设置页的模型行一致）：没下载且不完全适合本机（较慢 / 内存不足 / 显卡待检测）时，点「下载」先在
 *   卡片里确认并说明原因；显卡检测出较慢 / 内存不足时下载会自动暂停，点「继续下载」同样先确认（在这张卡片上刚确认过同一个
 *   结论时不再问）；
 * - 下载中：细进度条 + 百分比（校验时是不确定进度；排队时是静止的空进度条，不让还没开始的任务看起来在动），说明改为
 *   「下载完成后会自动继续」；
 * - 下载完成（模型与它需要的推理组件都装好）：调用 `onReady`——有可重试的请求时视图直接重新发起（卡片随之消失）；
 *   没有时（OCR 在截图之前就发现模型没下载）卡片换成「已下载」并给出 `readyAction`（重新截图）。每一轮下载最多自动
 *   重试一次（store 的 `claimAutoRetry`）：再次落到这张卡片时只给出手动的「重试」，不会循环。
 * 进度来自 `offline_models:changed`（shared/offline/store：这张卡片出现时才读快照、订阅事件），这里没有定时器。
 */
import { CircleCheck, Download, HardDriveDownload, Play, RotateCw } from 'lucide-react'
import { m } from 'motion/react'
import { useEffect, useRef, useState, type ReactNode } from 'react'
import { formatBytes } from '../i18n/format'
import { useT, useTKey } from '../i18n/useT'
import { api } from '../ipc'
import type { Fitness, FitnessLevel, LocalModelId, LocalModelState, NotReady } from '../ipc/types'
import { usePreset } from '../motion/presets'
import { Button, InlineConfirm, NotReadyCard, ProgressBar } from '../ui'
import {
  downloadSize,
  isActive,
  isUsable,
  needsFitConfirm,
  needsResumeConfirm,
  progressOf,
} from './catalog'
import { claimAutoRetry, useOfflineModels } from './store'

const PERCENT = 100

export type OfflineNotReady = Extract<
  NotReady,
  { reason: 'offline_model_missing' | 'offline_model_downloading' }
>

export function isOfflineNotReady(nr: NotReady | null): nr is OfflineNotReady {
  return nr?.reason === 'offline_model_missing' || nr?.reason === 'offline_model_downloading'
}

export interface OfflineCardProps {
  notReady: OfflineNotReady
  /** 模型下载完成（只调用一次） */
  onReady: () => void
  /** 下载完成后、视图没有自动继续时的按钮（例如「重新截图」）；不给时只显示「已下载」 */
  readyAction?: ReactNode
  /** 「切换模型」：打开设置里选模型的地方 */
  onSwitchModel?: () => void
  /** 下载完成后会自动继续这次请求（false：OCR 还没有截图，下载完要重新截图） */
  resumes?: boolean
  'data-testid'?: string
}

export function OfflineCard({
  notReady,
  onReady,
  readyAction,
  onSwitchModel,
  resumes = true,
  'data-testid': testId = 'offline-card',
}: OfflineCardProps) {
  const t = useT()
  const tKey = useTKey()
  const snap = useOfflineModels()
  const cardIn = usePreset('cardIn')
  const [busy, setBusy] = useState(false)
  const [failed, setFailed] = useState(false)
  /** 确认条为哪种状态打开（没下载 / 已暂停）：下载在别处开始或结束之后状态变了，确认条随之收起 */
  const [confirmFor, setConfirmFor] = useState<LocalModelState['state'] | null>(null)
  /** 在这张卡片上确认过的适配结论：之后检测出别的结论（待检测 → 较慢）才再问 */
  const confirmedFit = useRef<FitnessLevel | null>(null)
  const id: LocalModelId = notReady.model
  const info = snap?.models.find((x) => x.id === id) ?? null
  // 模型文件在、推理组件缺：当作没下载（再下载一次只补齐推理组件），不当作「已下载」去重试
  const usable = info !== null && isUsable(info)
  const state = info && !usable && info.state.state === 'installed' ? null : (info?.state ?? null)
  const name = t(`common.offline_model.${id}`)
  const confirming = confirmFor !== null && confirmFor === state?.state

  // 下载完成：通知视图一次（重新发起这次请求）；这一轮已经自动重试过时不再自动重试，等用户点「重试」
  const notified = useRef(false)
  useEffect(() => {
    if (!usable || notified.current) return
    notified.current = true
    if (claimAutoRetry(id)) onReady()
  }, [usable, id, onReady])

  const startDownload = async () => {
    setBusy(true)
    setFailed(false)
    try {
      await api.offlineModelDownload(id, false)
    } catch {
      setFailed(true)
    } finally {
      setBusy(false)
    }
  }
  /** 下载 / 继续：没下载且不完全适合、或已暂停且检测出较慢 / 内存不足时先确认 */
  const download = () => {
    const level = info?.fitness.level
    const ask =
      level !== undefined &&
      ((state?.state === 'not_installed' && needsFitConfirm(level)) ||
        (state?.state === 'paused' && needsResumeConfirm(level) && confirmedFit.current !== level))
    if (ask) {
      setConfirmFor(state.state)
      return
    }
    void startDownload()
  }
  const confirmFit = () => {
    confirmedFit.current = info?.fitness.level ?? null
    setConfirmFor(null)
    void startDownload()
  }

  const switchButton = onSwitchModel ? (
    <Button size="sm" variant="secondary" onClick={onSwitchModel} data-testid={`${testId}-switch`}>
      {t('common.action.switch_model')}
    </Button>
  ) : null

  if (usable) {
    // 自动重试之后视图会换掉这张卡片；卡片还留着、又没有 readyAction，说明这一轮已经自动重试过：给一个手动的「重试」
    const retry = !readyAction ? (
      <Button
        size="sm"
        variant="primary"
        icon={RotateCw}
        onClick={onReady}
        data-testid={`${testId}-retry`}
      >
        {t('panel.offline.retry')}
      </Button>
    ) : null
    return (
      <NotReadyCard
        notReady={notReady}
        icon={CircleCheck}
        title={t('panel.offline.ready_title', { model: name })}
        detail={readyAction ? t('panel.offline.ready_retake') : t('panel.offline.ready_detail')}
        actions={
          readyAction ??
          (retry ? (
            <>
              {retry}
              {switchButton}
            </>
          ) : null)
        }
        data-testid={testId}
      />
    )
  }

  const active = state !== null && isActive(state)
  const ratio = state ? progressOf(state) : null
  // 只缺推理组件时只要下载它
  const size = info
    ? formatBytes(
        state === null && info.state.state === 'installed'
          ? info.pending_runtime_bytes
          : downloadSize(info),
      )
    : null
  let line: string | null = null
  if (state?.state === 'downloading' && ratio !== null) {
    line = t('panel.offline.progress', {
      n: Math.floor(ratio * PERCENT),
      done: formatBytes(state.done),
      total: formatBytes(state.total),
    })
  } else if (state?.state === 'paused' && ratio !== null) {
    line = t('panel.offline.paused', { n: Math.floor(ratio * PERCENT) })
  } else if (state?.state === 'queued') line = t('panel.offline.queued')
  else if (state?.state === 'verifying') line = t('panel.offline.verifying')

  let primary: ReactNode = null
  // 确认期间主按钮让位给确认条（确认 / 取消都在条里）
  if (!active && !confirming) {
    const paused = state?.state === 'paused'
    const retry = failed || state?.state === 'failed' || state?.state === 'broken'
    primary = (
      <Button
        size="sm"
        variant="primary"
        icon={paused ? Play : retry ? RotateCw : Download}
        loading={busy}
        disabled={!info}
        onClick={download}
        data-testid={`${testId}-download`}
      >
        {paused
          ? t('panel.offline.resume')
          : retry
            ? t('panel.offline.retry')
            : size
              ? t('panel.offline.download', { size })
              : t('panel.offline.download_plain')}
      </Button>
    )
  }

  const detail = active
    ? resumes
      ? t('panel.offline.auto_continue')
      : t('panel.offline.downloading_retake')
    : state?.state === 'failed'
      ? t(`common.download_error.${state.code}`)
      : failed
        ? t('panel.offline.failed')
        : resumes
          ? t('panel.offline.missing_detail')
          : t('panel.offline.missing_plain')

  return (
    <NotReadyCard
      notReady={notReady}
      icon={HardDriveDownload}
      title={
        active
          ? t('panel.not_ready.offline_model_downloading.title', { model: name })
          : t('panel.not_ready.offline_model_missing.title', { model: name })
      }
      detail={detail}
      actions={
        primary || switchButton ? (
          <>
            {primary}
            {switchButton}
          </>
        ) : null
      }
      data-testid={testId}
    >
      {info ? (
        <InlineConfirm
          open={confirming}
          className="ui-offline-confirm"
          tone="warning"
          timeoutMs={null}
          message={t('panel.offline.fit_confirm', {
            reason: fitReason(info.fitness, t, tKey),
            model: name,
            size: size ?? '',
          })}
          confirmLabel={t('panel.offline.fit_confirm_ok')}
          onCancel={() => setConfirmFor(null)}
          onConfirm={confirmFit}
          data-testid={`${testId}-confirm`}
        />
      ) : null}
      {line !== null ? (
        <m.div className="ui-offline-progress" {...cardIn} data-testid={`${testId}-progress`}>
          <ProgressBar
            value={state?.state === 'verifying' ? null : state?.state === 'queued' ? 0 : ratio}
            paused={state?.state === 'paused'}
            label={t('panel.offline.progress_label', { model: name })}
          />
          <span className="ui-offline-progress-text">{line}</span>
        </m.div>
      ) : null}
    </NotReadyCard>
  )
}

/**
 * 适配结论的说明：后端给的是设置页的文案 key（`settings.offline.fitness.*`，面板不加载设置页的命名空间），面板里同名的
 * 一份在 `panel.offline.fitness.*`；没有原因时用结论本身
 */
function fitReason(
  fit: Fitness,
  t: ReturnType<typeof useT>,
  tKey: ReturnType<typeof useTKey>,
): string {
  const reason = fit.reason_key?.split('.').pop()
  const text = reason ? tKey(`panel.offline.fitness.${reason}`, undefined, '') : ''
  return text || t(`panel.offline.fit.${fit.level}`)
}
