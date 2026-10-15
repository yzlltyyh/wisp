/**
 * 设置 → 同步：经用户自己的 WebDAV 在多台设备之间同步设置与使用记录（数据在本机加密后才上传）。
 *
 * 根视图按 `settings.sync.enabled` 分两种：
 * - 未连接：一张卡片——一句说明 + 主按钮「连接 WebDAV…」；
 * - 已连接：服务器（`主机 · 账号` + 状态行 + 立即同步 + 更新密码…）、自动同步、同步内容（改动即时写入设置）、设备、
 *   危险操作（断开同步 / 清除服务器上的数据，原地变形确认）。
 * 连接与更新密码是同一个草稿子视图（sync/ConnectView，单独的 chunk：只在点开时加载，悬停按钮时预取），切换方式与
 * 「模型与服务」的编辑器相同（lib/subview.ts：新视图进入、旧视图立即卸载，返回时恢复滚动位置与焦点）。
 * 成功不弹提示：远端改动经 `settings:changed` 实时出现，进度与错误都在状态行里。
 */
import '../sync/sync.css'
import { Cloud, Trash2, Unplug } from 'lucide-react'
import { m } from 'motion/react'
import { lazy, Suspense, useState } from 'react'
import { useT } from '../../shared/i18n/useT'
import { api } from '../../shared/ipc'
import type { SyncContent } from '../../shared/ipc/types'
import { adaptPreset, presets } from '../../shared/motion/presets'
import { useReducedMotionPref } from '../../shared/motion/useReducedMotionPref'
import { Button, Field, Section, Skeleton } from '../../shared/ui'
import { DangerRow } from '../components/Confirm'
import { Page, SwitchRow } from '../components/Page'
import { ipcErrorText } from '../lib/errors'
import { at } from '../lib/patch'
import { useSubviewNav } from '../lib/subview'
import { applySnapshot, notify, patchSettings, useRuntime, useSettings } from '../store'
import { ContentRows } from '../sync/ContentRows'
import { Devices } from '../sync/Devices'
import { ServerSection } from '../sync/ServerSection'
import { useNow } from '../sync/useNow'
import type { ConnectMode } from '../sync/ConnectView'

const loadConnect = () => import('../sync/ConnectView')
const ConnectView = lazy(loadConnect)

type View = { kind: 'root' } | { kind: 'connect'; mode: ConnectMode }

export default function SyncPage() {
  const settings = useSettings()
  const reduced = useReducedMotionPref()
  const [view, setView] = useState<View>({ kind: 'root' })
  const [dir, setDir] = useState<1 | -1>(1)
  const key = view.kind === 'root' ? 'root' : `connect:${view.mode}`
  const { host, leave, entered } = useSubviewNav(key)

  const go = (next: View, returnTo: string | null = null) => {
    leave(returnTo)
    setDir(next.kind === 'root' ? -1 : 1)
    setView(next)
  }

  if (!settings) return null
  const back = () => go({ kind: 'root' })
  const motion = adaptPreset(presets.stepSwitch(dir), 'fade', reduced)

  const body =
    view.kind === 'connect' ? (
      <Suspense fallback={<ConnectSkeleton />}>
        <ConnectView mode={view.mode} onBack={back} onConnected={back} />
      </Suspense>
    ) : settings.sync.enabled ? (
      <Connected onUpdate={() => go({ kind: 'connect', mode: 'update' }, 'sync-update')} />
    ) : (
      <NotConnected onConnect={() => go({ kind: 'connect', mode: 'new' }, 'sync-open')} />
    )

  return (
    <div ref={host} className="st-subview-host">
      <m.div
        key={key}
        className="st-subview"
        initial={entered ? motion.initial : false}
        animate={motion.animate}
      >
        {body}
      </m.div>
    </div>
  )
}

function NotConnected({ onConnect }: { onConnect: () => void }) {
  const t = useT()
  return (
    <Page
      title={t('settings.nav.sync')}
      description={t('settings.sync.description')}
      data-testid="sync-not-connected"
    >
      <Section>
        <Field label={t('settings.sync.intro.label')} description={t('settings.sync.intro.detail')}>
          <Button
            variant="primary"
            icon={Cloud}
            onClick={onConnect}
            // 子视图是单独的 chunk：指针移上来 / 键盘聚焦时就开始加载，点下去时通常已经就绪
            onPointerEnter={() => void loadConnect()}
            onFocus={() => void loadConnect()}
            data-nav-id="sync-open"
            data-testid="sync-open"
          >
            {t('settings.sync.intro.connect')}
          </Button>
        </Field>
      </Section>
    </Page>
  )
}

function Connected({ onUpdate }: { onUpdate: () => void }) {
  const t = useT()
  const settings = useSettings()
  const runtime = useRuntime()
  const now = useNow()
  const [resetting, setResetting] = useState(false)
  if (!settings || !runtime) return null
  const sync = settings.sync
  const status = runtime.sync

  const setContent = (key: keyof SyncContent, value: boolean) =>
    void patchSettings(at(`sync.content.${key}`, value))

  const refresh = async () => {
    try {
      applySnapshot(await api.settingsGet())
    } catch {
      // 随后的 settings:changed 同样会带来新快照
    }
  }
  const disconnect = async () => {
    try {
      await api.syncDisconnect()
      await refresh()
    } catch (error) {
      notify(ipcErrorText(error, 'sync_disconnect'), 'danger')
    }
  }
  const resetRemote = async () => {
    setResetting(true)
    try {
      await api.syncResetRemote()
      await refresh()
    } catch (error) {
      notify(ipcErrorText(error, 'sync_reset_remote'), 'danger')
    } finally {
      setResetting(false)
    }
  }

  return (
    <Page
      title={t('settings.nav.sync')}
      description={t('settings.sync.description')}
      data-testid="sync-connected"
    >
      <ServerSection
        serverUrl={sync.server_url}
        username={sync.username}
        status={status}
        now={now}
        onUpdate={onUpdate}
      />

      <Section>
        <SwitchRow
          label={t('settings.sync.auto.label')}
          description={t('settings.sync.auto.desc')}
          checked={sync.auto}
          onChange={(v) => void patchSettings(at('sync.auto', v))}
          ariaLabel={t('settings.sync.auto.label')}
          data-testid="sync-auto"
        />
      </Section>

      <Section title={t('settings.sync.content.title')} data-testid="sync-content">
        <ContentRows
          content={sync.content}
          onChange={setContent}
          ocrHistoryOn={settings.ocr.history}
          connected
        />
        <p className="st-sync-note" data-testid="sync-local-note">
          {t('settings.sync.content.local_note')}
        </p>
      </Section>

      <Devices refreshKey={`${status.last_ok_ms ?? 0}:${status.device_count}`} now={now} />

      <Section>
        <DangerRow
          label={t('settings.sync.danger.disconnect')}
          description={t('settings.sync.danger.disconnect_desc')}
          action={t('settings.sync.danger.disconnect_action')}
          confirm={
            status.pending
              ? t('settings.sync.danger.disconnect_confirm_pending')
              : t('settings.sync.danger.disconnect_confirm')
          }
          icon={Unplug}
          onConfirm={() => void disconnect()}
          disabled={resetting}
          data-testid="sync-disconnect"
        />
        {resetting ? (
          <Field
            label={t('settings.sync.danger.reset')}
            description={t('settings.sync.danger.reset_desc')}
          >
            <Button size="sm" loading className="st-btn-danger" icon={Trash2}>
              {t('settings.sync.danger.reset_action')}
            </Button>
          </Field>
        ) : (
          <DangerRow
            label={t('settings.sync.danger.reset')}
            description={t('settings.sync.danger.reset_desc')}
            action={t('settings.sync.danger.reset_action')}
            confirm={t('settings.sync.danger.reset_confirm')}
            icon={Trash2}
            onConfirm={() => void resetRemote()}
            data-testid="sync-reset"
          />
        )}
      </Section>
    </Page>
  )
}

/** 连接子视图的 chunk 还没到（极少：按钮悬停时已预取）：占住编辑器的版式，不闪空白 */
function ConnectSkeleton() {
  return (
    <div className="st-editor" aria-busy="true">
      <div className="st-skeleton-title">
        <Skeleton lines={1} />
      </div>
      <div className="st-skeleton-card">
        <Skeleton lines={4} />
      </div>
    </div>
  )
}
