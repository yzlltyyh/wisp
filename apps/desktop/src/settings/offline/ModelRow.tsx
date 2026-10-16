/**
 * 离线模型卡里的一行：图标 · 名称（运行状态的小圆点、适配标记）· 一句话说明 · 右侧操作；下载中 / 已暂停 / 校验中在下方
 * 多一行进度（状态文字 + 百分比 + 细进度条）。
 *
 * - 状态与操作：未下载「下载 · 1.2 GB」（需要推理组件时说明里注明另需多少）；排队中可取消（静止的空进度条：还没开始的
 *   任务不显示在动的进度）；下载中 暂停 / 取消；已暂停 继续 / 取消（暂停与继续是同一位置的图标按钮，切换时这一行不变宽）；
 *   校验中只显示不确定进度；已下载「用于识别 / 用于翻译」（按下即选用，再按一次改回默认模型）+ ⋯（在文件夹中显示、删除）；
 *   损坏「重新下载」；失败说明原因 +「重试」/ 取消；
 * - 设备适配不是「适合本机」（内存不足 / 较慢 / 显卡还没检测）时，点下载先在这一行原地确认（说明原因）；显卡在推理组件
 *   装好后才检测，检测出较慢 / 内存不足时下载会自动暂停：已暂停的这一行写出检测结论，点「继续」同样先确认（这一行刚确认过
 *   同一个结论时不再重复问）；删除同样原地确认。确认条取这一行当时的高度，不让布局跳动（与 DangerRow 一致），焦点给
 *   「取消」，Esc 取消；
 * - 动效：状态文字只在**种类**变化时淡入（数字刷新不重播）；进度行出现 / 消失用 cardIn；进度条只改 scaleX，由 CSS 以
 *   --dur-base 过渡；加载中的小圆点脉动（减少动效时静止），悬停说明第一次加载为什么慢（llama.cpp 的模型要先编译显卡
 *   着色器——下载完成后的预热也是这一步）。
 * 每次操作只让被点的按钮转圈；结果由 `offline_models:changed` 推送回来，这里不轮询。
 */
import {
  CircleAlert,
  Download,
  Ellipsis,
  FolderOpen,
  Languages,
  Pause,
  Play,
  RotateCw,
  ScanText,
  Trash2,
  TriangleAlert,
  X,
  Check,
  type LucideIcon,
} from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useEffect, useRef, useState, type ReactNode } from 'react'
import { usePlatform } from '../../shared/hooks/usePlatform'
import { formatBytes } from '../../shared/i18n/format'
import { useT, useTKey } from '../../shared/i18n/useT'
import { api } from '../../shared/ipc'
import type { FitnessLevel, LocalModelInfo } from '../../shared/ipc/types'
import { usePreset } from '../../shared/motion/presets'
import {
  downloadSize,
  etaSeconds,
  needsFitConfirm,
  needsResumeConfirm,
  progressOf,
  usesLlama,
} from '../../shared/offline/catalog'
import {
  Badge,
  Button,
  DropdownMenu,
  Icon,
  IconButton,
  ProgressBar,
  Tooltip,
  type MenuEntry,
} from '../../shared/ui'
import { useConfirmKeys } from '../components/Confirm'
import { ipcErrorText } from '../lib/errors'
import { notify } from '../store'
import { FIT_TONE, formatEta, formatSpeed, rowKind } from './model'

const PERCENT = 100

type Confirm = 'fit' | 'delete' | null

export interface ModelRowProps {
  model: LocalModelInfo
  /** 本机正选用它（offline.*_model） */
  selected: boolean
  /** 选用 / 取消选用（写 offline.*_model） */
  onSelect: (next: boolean) => void
}

export function ModelRow({ model, selected, onSelect }: ModelRowProps) {
  const t = useT()
  const tKey = useTKey()
  const os = usePlatform()
  const cardIn = usePreset('cardIn')
  const fade = usePreset('footerIn')
  const [busy, setBusy] = useState<string | null>(null)
  const [confirm, setConfirm] = useState<Confirm>(null)
  const [rowHeight, setRowHeight] = useState<number | null>(null)
  const row = useRef<HTMLDivElement>(null)
  const cancelRef = useRef<HTMLButtonElement>(null)
  const confirmBox = useRef<HTMLDivElement>(null)
  /** 确认条关闭后把焦点还给哪个按钮（data-testid）：触发它的按钮在确认期间被换下，关闭后重新挂载 */
  const back = useRef<string | null>(null)
  /** 用户在这一行确认过的适配结论：手动暂停后继续不再重复确认；检测后结论变了（待检测 → 较慢）才再问 */
  const confirmedFit = useRef<FitnessLevel | null>(null)
  useConfirmKeys(confirm !== null, () => setConfirm(null), {
    cancel: cancelRef,
    container: confirmBox,
  })
  useEffect(() => {
    if (confirm !== null || back.current === null) return
    const target = back.current
    back.current = null
    const active = document.activeElement
    if (active && active !== document.body) return
    row.current
      ?.querySelector<HTMLElement>(`[data-testid="${target}"]`)
      ?.focus({ preventScroll: true })
  }, [confirm])

  const id = model.id
  const name = t(`common.offline_model.${id}`)
  const kind = rowKind(model)
  const s = model.state
  const size = formatBytes(downloadSize(model))
  const fit = model.fitness
  const testId = `offline-model-${id}`

  /** 执行一个命令：只有这个按钮转圈；失败时底部提示 */
  const run = async (what: string, command: string, call: () => Promise<unknown>) => {
    setBusy(what)
    try {
      await call()
      return true
    } catch (error) {
      notify(ipcErrorText(error, command), 'danger')
      return false
    } finally {
      setBusy(null)
    }
  }
  const openConfirm = (which: Exclude<Confirm, null>, from: string) => {
    back.current = from
    setRowHeight(row.current?.getBoundingClientRect().height ?? null)
    setConfirm(which)
  }
  /** 下载 / 继续：没下载且不完全适合、或已暂停且检测出较慢 / 内存不足时先原地确认（`confirmed` 跳过确认） */
  const download = (confirmed = false) => {
    if (!confirmed) {
      if (s.state === 'not_installed' && needsFitConfirm(fit.level)) {
        openConfirm('fit', `${testId}-download`)
        return
      }
      if (
        s.state === 'paused' &&
        needsResumeConfirm(fit.level) &&
        confirmedFit.current !== fit.level
      ) {
        openConfirm('fit', `${testId}-resume`)
        return
      }
    }
    setConfirm(null)
    void run('download', 'offline_model_download', () => api.offlineModelDownload(id, false))
  }
  const confirmFit = () => {
    confirmedFit.current = fit.level
    download(true)
  }
  const remove = () => {
    setConfirm(null)
    void run('delete', 'offline_model_delete', () => api.offlineModelDelete(id)).then(
      (ok) => ok && notify(t('settings.offline.deleted', { model: name }), 'success'),
    )
  }

  if (confirm) {
    const reason = fit.reason_key ? tKey(fit.reason_key, undefined, '') : ''
    const text =
      confirm === 'delete'
        ? t('settings.offline.delete_confirm', {
            model: name,
            size: formatBytes(model.download_bytes),
          })
        : t('settings.offline.fit_confirm', {
            reason: reason || t(`settings.offline.fit.${fit.level}`),
            model: name,
            size,
          })
    return (
      <m.div
        ref={confirmBox}
        className="st-confirm-row ui-confirm st-om-confirm"
        data-tone={confirm === 'delete' ? 'danger' : 'warning'}
        role="alert"
        style={rowHeight ? { minHeight: rowHeight } : undefined}
        data-testid={`${testId}-confirm`}
        {...fade}
      >
        <Icon icon={TriangleAlert} size={14} className="ui-confirm-icon" />
        <span className="ui-confirm-text">{text}</span>
        <span className="ui-confirm-actions">
          <Button ref={cancelRef} size="sm" variant="ghost" onClick={() => setConfirm(null)}>
            {t('common.action.cancel')}
          </Button>
          <Button
            size="sm"
            variant={confirm === 'delete' ? 'danger' : 'secondary'}
            onClick={() => (confirm === 'delete' ? remove() : confirmFit())}
            data-testid={`${testId}-confirm-ok`}
          >
            {confirm === 'delete'
              ? t('settings.offline.delete_ok')
              : t('settings.offline.fit_confirm_ok')}
          </Button>
        </span>
      </m.div>
    )
  }

  // ---- 说明行：一句话说明；已下载时注明大小 / 运行状态 ----
  const note = t(`settings.offline.summary.${id}`)
  let status: { icon?: LucideIcon; tone?: 'danger' | 'warning'; text: string } | null = null
  if (kind === 'installed') {
    if (model.load === 'loaded')
      status = {
        text: t('settings.offline.loaded', {
          mem: formatBytes(model.memory_estimate_mb * 1024 * 1024),
        }),
      }
    else if (model.load === 'loading') status = { text: t('settings.offline.loading') }
    else if (model.update_available) status = { text: t('settings.offline.update') }
    else
      status = {
        text: t('settings.offline.installed', { size: formatBytes(model.download_bytes) }),
      }
  } else if (kind === 'broken') {
    status = { icon: TriangleAlert, tone: 'warning', text: t('settings.offline.broken') }
  } else if (s.state === 'failed') {
    status = { icon: CircleAlert, tone: 'danger', text: t(`common.download_error.${s.code}`) }
  } else if (s.state === 'paused' && needsResumeConfirm(fit.level)) {
    // 检测结论（检测后自动暂停时用户最需要知道的）：继续之前先看到为什么停在这里
    status = {
      icon: TriangleAlert,
      tone: 'warning',
      text:
        (fit.reason_key ? tKey(fit.reason_key, undefined, '') : '') ||
        t(`settings.offline.fit.${fit.level}`),
    }
  } else if (kind === 'not_installed' && model.pending_runtime_bytes > 0) {
    status = {
      text: t('settings.offline.runtime_extra', { size: formatBytes(model.pending_runtime_bytes) }),
    }
  }

  // ---- 进度行（下载中 / 已暂停 / 校验中 / 排队中）----
  const ratio = progressOf(s)
  let progressText: string | null = null
  if (s.state === 'downloading') {
    const eta = etaSeconds(s)
    progressText = [
      t('settings.offline.progress', { done: formatBytes(s.done), total: formatBytes(s.total) }),
      s.bytes_per_sec > 0 ? formatSpeed(s.bytes_per_sec) : null,
      eta !== null ? t('settings.offline.eta', { time: formatEta(eta) }) : null,
    ]
      .filter(Boolean)
      .join(' · ')
  } else if (s.state === 'paused') {
    progressText = t('settings.offline.paused', {
      done: formatBytes(s.done),
      total: formatBytes(s.total),
    })
  } else if (s.state === 'verifying') progressText = t('settings.offline.verifying')
  else if (s.state === 'queued') progressText = t('settings.offline.queued')
  const showProgress = progressText !== null

  // ---- 操作 ----
  const cancelButton = (
    <IconButton
      icon={X}
      label={t('settings.offline.cancel')}
      onClick={() => void run('cancel', 'offline_model_cancel', () => api.offlineModelCancel(id))}
      disabled={busy !== null}
      data-testid={`${testId}-cancel`}
    />
  )
  const menuItems: MenuEntry[] = [
    {
      id: 'reveal',
      label: t(`settings.offline.reveal.${os}`),
      icon: FolderOpen,
      onSelect: () => void run('reveal', 'offline_reveal', () => api.offlineReveal()),
    },
    ...(model.update_available
      ? [
          {
            id: 'redownload',
            label: t('settings.offline.redownload'),
            icon: RotateCw,
            onSelect: () => download(true),
          },
        ]
      : []),
    { type: 'separator', id: 'sep' },
    {
      id: 'delete',
      label: t('settings.offline.delete'),
      icon: Trash2,
      danger: true,
      onSelect: () => openConfirm('delete', `${testId}-more`),
    },
  ]
  const more = (
    <DropdownMenu
      label={t('settings.offline.more')}
      align="end"
      items={menuItems}
      data-testid={`${testId}-menu`}
      renderTrigger={(p, open) => (
        <IconButton
          {...p}
          icon={Ellipsis}
          label={t('settings.offline.more')}
          noTooltip={open}
          data-testid={`${testId}-more`}
        />
      )}
    />
  )

  let actions: ReactNode
  switch (s.state) {
    case 'not_installed':
      actions = (
        <Button
          size="sm"
          icon={Download}
          loading={busy === 'download'}
          onClick={() => download()}
          data-testid={`${testId}-download`}
        >
          {t('settings.offline.download', { size: formatBytes(model.download_bytes) })}
        </Button>
      )
      break
    case 'queued':
      actions = cancelButton
      break
    case 'downloading':
      actions = (
        <>
          <IconButton
            icon={Pause}
            label={t('settings.offline.pause')}
            onClick={() =>
              void run('pause', 'offline_model_pause', () => api.offlineModelPause(id))
            }
            disabled={busy !== null}
            data-testid={`${testId}-pause`}
          />
          {cancelButton}
        </>
      )
      break
    case 'paused':
      actions = (
        <>
          <IconButton
            icon={Play}
            label={t('settings.offline.resume')}
            onClick={() => download()}
            disabled={busy !== null}
            data-testid={`${testId}-resume`}
          />
          {cancelButton}
        </>
      )
      break
    case 'verifying':
      actions = null
      break
    case 'installed':
      actions = (
        <>
          <Tooltip content={t(`settings.offline.in_use.${model.kind}`)} disabled={!selected}>
            <Button
              size="sm"
              variant="secondary"
              icon={selected ? Check : undefined}
              aria-pressed={selected}
              className="st-om-use"
              data-on={selected || undefined}
              onClick={() => onSelect(!selected)}
              data-testid={`${testId}-use`}
            >
              {t(`settings.offline.use.${model.kind}`)}
            </Button>
          </Tooltip>
          {more}
        </>
      )
      break
    case 'broken':
      actions = (
        <>
          <Button
            size="sm"
            icon={RotateCw}
            loading={busy === 'download'}
            onClick={() => download(true)}
            data-testid={`${testId}-redownload`}
          >
            {t('settings.offline.redownload')}
          </Button>
          {more}
        </>
      )
      break
    case 'failed':
      actions = (
        <>
          <Button
            size="sm"
            icon={RotateCw}
            loading={busy === 'download'}
            onClick={() => download(true)}
            data-testid={`${testId}-retry`}
          >
            {t('settings.offline.retry')}
          </Button>
          {cancelButton}
        </>
      )
      break
  }

  const dot =
    kind === 'installed' && model.load !== 'unloaded' ? (
      <span
        className="st-om-dot"
        data-load={model.load}
        // 运行状态由下方的状态文字说明（读屏读它），圆点只是视觉上的提示
        aria-hidden="true"
        title={
          model.load === 'loading' && usesLlama(id)
            ? t('settings.offline.dot.preparing')
            : t(`settings.offline.dot.${model.load}`)
        }
        data-testid={`${testId}-dot`}
      />
    ) : null

  return (
    <div
      ref={row}
      className="st-om-row"
      data-state={kind}
      data-selected={selected || undefined}
      data-testid={testId}
    >
      <div className="st-om-main">
        <span className="st-logo st-om-icon" data-tone="neutral" aria-hidden="true">
          <Icon icon={model.kind === 'ocr' ? ScanText : Languages} size={16} />
        </span>
        <div className="st-om-text">
          <div className="st-om-title">
            <b>{name}</b>
            {dot}
            <FitBadge model={model} />
          </div>
          <p className="st-om-note">{note}</p>
          {status ? (
            <m.p
              key={`${kind}:${model.load}:${status.tone ?? ''}`}
              className="st-om-status"
              data-tone={status.tone}
              {...cardIn}
            >
              {status.icon ? <Icon icon={status.icon} size={12} /> : null}
              <span>{status.text}</span>
            </m.p>
          ) : null}
        </div>
        <span className="st-om-actions">{actions}</span>
      </div>
      <AnimatePresence initial={false}>
        {showProgress ? (
          <m.div key="progress" className="st-om-progress" {...cardIn}>
            <div className="st-om-progress-line">
              <m.span key={kind} className="st-om-progress-text" {...cardIn}>
                {progressText}
              </m.span>
              {ratio !== null ? (
                <span className="st-om-percent">
                  {t('settings.offline.percent', { n: Math.floor(ratio * PERCENT) })}
                </span>
              ) : null}
            </div>
            <ProgressBar
              value={s.state === 'verifying' ? null : s.state === 'queued' ? 0 : ratio}
              paused={s.state === 'paused'}
              label={t('settings.offline.downloading_label', { model: name })}
              data-testid={`${testId}-progress`}
            />
          </m.div>
        ) : null}
      </AnimatePresence>
    </div>
  )
}

/** 适配标记：原因（例如「下载推理组件后检测显卡」）放在悬停说明里 */
function FitBadge({ model }: { model: LocalModelInfo }) {
  const t = useT()
  const tKey = useTKey()
  const fit = model.fitness
  const badge = (
    <Badge tone={FIT_TONE[fit.level]} data-testid={`offline-model-${model.id}-fit`}>
      {t(`settings.offline.fit.${fit.level}`)}
    </Badge>
  )
  const reason = fit.reason_key ? tKey(fit.reason_key, undefined, '') : ''
  return reason ? (
    <Tooltip content={reason} className="st-om-fit">
      {badge}
    </Tooltip>
  ) : (
    badge
  )
}
