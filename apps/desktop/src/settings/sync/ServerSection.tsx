/**
 * 已连接页的「服务器」分组：`主机 · 账号` 一行，下面一条安静的状态行（照搬「关于 → 更新」的写法），右侧「立即同步」。
 *
 * - 状态行：同步中（转圈；有总数时「同步中 · 120 / 1000」+ 4 px 进度条）/ 已同步 · 2 分钟前（对勾）/ 有改动等待同步 /
 *   尚未同步 / 暂时无法连接，稍后自动重试（暂时性失败不标红）/ 需要处理的错误（原因 + 建议，红色；技术细节收进
 *   可折叠的「错误详情」）。状态切换时 cardIn 过渡，变绿时对勾 statusPulse；
 * - 需要用户处理的错误就地给出操作：密码不对 →「密码」一行的「更新密码…」变成主要操作，服务器上的数据被清除 →
 *   状态行右侧出现「重新连接…」；
 * - 「密码」一行进入同一个连接子视图（地址与账号只读）。
 */
import {
  CircleCheck,
  CloudOff,
  CloudUpload,
  KeyRound,
  RefreshCw,
  RotateCw,
  TriangleAlert,
} from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useState } from 'react'
import { useT } from '../../shared/i18n/useT'
import { relativeTime } from '../../shared/i18n/relativeTime'
import { api } from '../../shared/ipc'
import type { SyncStatus } from '../../shared/ipc/types'
import { usePreset } from '../../shared/motion/presets'
import { Button, Field, Icon, Section, Spinner } from '../../shared/ui'
import { Disclosure } from '../components/Disclosure'
import { ipcErrorText } from '../lib/errors'
import { notify, setSync } from '../store'
import { errorAction, hostOf, progressRatio, statusView, type StatusView } from './model'

export function ServerSection({
  serverUrl,
  username,
  status,
  now,
  onUpdate,
}: {
  serverUrl: string
  username: string
  status: SyncStatus
  now: number
  /** 打开连接子视图（更新密码 / 重新连接） */
  onUpdate: () => void
}) {
  const t = useT()
  const cardIn = usePreset('cardIn')
  const pulse = usePreset('statusPulse')
  const [busy, setBusy] = useState(false)
  const view = statusView(status)
  const syncNow = async () => {
    setBusy(true)
    try {
      setSync(await api.syncNow())
    } catch (error) {
      notify(ipcErrorText(error, 'sync_now'), 'danger')
    } finally {
      setBusy(false)
    }
  }
  const action = view.kind === 'problem' ? errorAction(view.error) : null
  const ratio = view.kind === 'syncing' ? progressRatio(view.progress) : null

  return (
    <Section title={t('settings.sync.server.title')} data-testid="sync-server">
      <div className="st-update st-sync-server" data-state={view.kind}>
        <div className="st-update-row">
          <div className="st-update-status">
            <span className="st-sync-account" data-testid="sync-account">
              <span className="st-sync-host">{hostOf(serverUrl)}</span>
              <span className="st-sync-sep" aria-hidden="true">
                ·
              </span>
              <span className="st-sync-user">{username}</span>
            </span>
            <m.span key={lineKey(view)} className="st-sync-status" {...cardIn}>
              <StatusLine view={view} now={now} pulse={pulse} />
            </m.span>
          </div>
          <span className="st-update-actions">
            {/* 密码不对时下面「密码」一行的按钮变成主要操作（建议里引用的就是它），这里不再放第二个同名按钮 */}
            {action === 'reconnect' ? (
              <Button
                size="sm"
                variant="primary"
                icon={RotateCw}
                onClick={onUpdate}
                data-testid="sync-fix"
              >
                {t('settings.sync.server.reconnect')}
              </Button>
            ) : null}
            <Button
              size="sm"
              icon={RefreshCw}
              // 后台同步可能持续很久：进行中由状态行的转圈与进度条表示，按钮只是暂不可点；转圈只留给这次点击本身
              loading={busy}
              disabled={view.kind === 'syncing'}
              onClick={() => void syncNow()}
              data-testid="sync-now"
            >
              {t('settings.sync.server.sync_now')}
            </Button>
          </span>
        </div>
        <AnimatePresence initial={false}>
          {ratio !== null ? (
            <m.div key="progress" className="st-progress" {...cardIn}>
              <span
                className="st-progress-bar"
                data-testid="sync-progress"
                style={{ transform: `scaleX(${ratio})` }}
              />
            </m.div>
          ) : null}
        </AnimatePresence>
        {(view.kind === 'problem' || view.kind === 'offline') && status.error_detail ? (
          <Disclosure title={t('settings.about.update.error_details')} variant="plain">
            <pre className="st-notes st-error-detail" data-testid="sync-error-detail">
              {status.error_detail}
            </pre>
          </Disclosure>
        ) : null}
      </div>
      <Field
        label={t('settings.sync.server.update_label')}
        description={t('settings.sync.server.update_desc')}
      >
        <Button
          size="sm"
          variant={action === 'update' ? 'primary' : 'secondary'}
          icon={action === 'update' ? KeyRound : undefined}
          onClick={onUpdate}
          data-nav-id="sync-update"
          data-testid="sync-update"
        >
          {t('settings.sync.server.update')}
        </Button>
      </Field>
    </Section>
  )
}

/** 状态行换「种类」时才过渡（进度数字、相对时间的变化不重播） */
function lineKey(view: StatusView): string {
  return view.kind === 'problem' ? `problem:${view.error}` : view.kind
}

function StatusLine({
  view,
  now,
  pulse,
}: {
  view: StatusView
  now: number
  pulse: ReturnType<typeof usePreset>
}) {
  const t = useT()
  switch (view.kind) {
    case 'syncing':
      return (
        <span className="st-update-line" data-testid="sync-status-line">
          <Spinner size={14} />
          {view.progress && view.progress.total > 0
            ? t('settings.sync.status.progress', {
                done: view.progress.done,
                total: view.progress.total,
              })
            : t('settings.sync.status.syncing')}
        </span>
      )
    case 'synced':
      return (
        <span className="st-update-line" data-tone="success" data-testid="sync-status-line">
          <m.span className="st-sync-icon" {...pulse}>
            <Icon icon={CircleCheck} size={14} />
          </m.span>
          {t('settings.sync.status.synced', { time: relativeTime(view.at, now) })}
        </span>
      )
    case 'pending':
      return (
        <span className="st-update-line" data-tone="accent" data-testid="sync-status-line">
          <Icon icon={CloudUpload} size={14} />
          {t('settings.sync.status.pending')}
        </span>
      )
    case 'never':
      return (
        <span className="st-update-line" data-testid="sync-status-line">
          {t('settings.sync.status.never')}
        </span>
      )
    case 'offline':
      return (
        <span className="st-update-line" data-tone="warning" data-testid="sync-status-line">
          <Icon icon={CloudOff} size={14} />
          {t('settings.sync.status.transient')}
        </span>
      )
    case 'problem':
      return (
        <>
          <span
            className="st-update-line"
            data-tone={view.transient ? 'warning' : 'danger'}
            data-testid="sync-status-line"
          >
            <Icon icon={TriangleAlert} size={14} />
            {t(`settings.sync.error.${view.error}.reason`)}
          </span>
          <span className="st-update-error" data-testid="sync-status-advice">
            {t(`settings.sync.error.${view.error}.advice`)}
          </span>
        </>
      )
  }
}
