/**
 * 已连接页的「设备」：`sync_devices` 只在同步页打开时调用（设备名不进运行时快照与日志）；每完成一次同步（最近成功时间
 * 或设备数变化）重新读一次。本机排在最前并标「本机」，其余显示系统与最近同步时间。
 * 读取中显示骨架（只在没有旧列表时），失败时就地给出「重试」。
 */
import { RefreshCw } from 'lucide-react'
import { useEffect, useState } from 'react'
import { useT } from '../../shared/i18n/useT'
import { relativeTime } from '../../shared/i18n/relativeTime'
import { api } from '../../shared/ipc'
import type { SyncDevice } from '../../shared/ipc/types'
import { Badge, Button, Field, Section, Skeleton } from '../../shared/ui'

/** 本机在前，其余按最近同步时间从新到旧 */
export function sortDevices(list: readonly SyncDevice[]): SyncDevice[] {
  return [...list].sort(
    (a, b) => Number(b.this_device) - Number(a.this_device) || b.last_seen_ms - a.last_seen_ms,
  )
}

export function Devices({ refreshKey, now }: { refreshKey: string; now: number }) {
  const t = useT()
  const [devices, setDevices] = useState<SyncDevice[] | null>(null)
  const [failed, setFailed] = useState(false)
  const [attempt, setAttempt] = useState(0)
  /** 这一轮要读的列表（同步完成或点了「重试」时变化）与已经读过的那一轮 */
  const token = `${refreshKey}#${attempt}`
  const [loaded, setLoaded] = useState<string | null>(null)

  useEffect(() => {
    if (loaded === token) return
    let alive = true
    api
      .syncDevices()
      .then((list) => {
        if (!alive) return
        setDevices(sortDevices(list))
        setFailed(false)
        setLoaded(token)
      })
      .catch(() => {
        if (!alive) return
        setFailed(true)
        setLoaded(token)
      })
    return () => {
      alive = false
    }
  }, [token, loaded])

  let body
  if (failed && devices === null) {
    body = (
      <div className="st-card-empty" data-testid="sync-devices-failed">
        <span>{t('settings.sync.devices.failed')}</span>
        <Button size="sm" variant="ghost" icon={RefreshCw} onClick={() => setAttempt((n) => n + 1)}>
          {t('common.action.retry')}
        </Button>
      </div>
    )
  } else if (devices === null) {
    body = (
      <div className="st-card-pad" aria-busy="true">
        <Skeleton lines={2} />
      </div>
    )
  } else if (devices.length === 0) {
    body = <div className="st-card-empty">{t('settings.sync.devices.empty')}</div>
  } else {
    body = devices.map((d) => {
      const os = t(`settings.about.os.${d.os}`)
      return (
        <Field
          key={d.id}
          label={
            <span className="st-label-with-badge">
              <span className="st-sync-device-name">{d.name}</span>
              {d.this_device ? (
                <Badge tone="accent">{t('settings.sync.devices.this_device')}</Badge>
              ) : null}
            </span>
          }
          description={
            d.this_device
              ? os
              : t('settings.sync.devices.seen', { os, time: relativeTime(d.last_seen_ms, now) })
          }
          data-testid="sync-device"
        >
          <span className="st-sync-device-version st-tabular">{d.app_version}</span>
        </Field>
      )
    })
  }

  return (
    <Section title={t('settings.sync.devices.title')} data-testid="sync-devices">
      {body}
    </Section>
  )
}
