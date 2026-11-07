/**
 * 「自动同步」下面的「实时同步（实验性）」子行，以及再下一级的「把其他设备最新的复制直接放进本机剪贴板」。改动即时写入设置。
 *
 * - 实时同步依赖自动同步，并且至少同步剪贴板历史或识别历史（只同步设置时没有「实时」可言）。不满足时禁用并显示为关，
 *   说明换成缺了什么（条件见 `realtimeBlocker`，结论以 Rust 为准）；用户的选择保留在设置里，条件满足后自动恢复；
 * - 开着时说明下方一行状态：运行中 / 离开电脑或省电模式时暂停 / 已放慢 / 服务器不支持 / 下一次同步成功后开始。状态随
 *   `sync:status` 推送；没有变化的检查不推送，所以这里不显示「上次检查」的时间（会停在旧值上）；
 * - 写入剪贴板的子选项还要求同步剪贴板历史；
 * - 坚果云按账号限制请求次数、与同一账号下的其他应用共享：地址是坚果云时多一行说明（实时同步可以打开时就显示，
 *   用户决定开不开之前就能看到；不可用时整组都是灰的，不再多一行）。
 */
import type { SyncRealtime, SyncSettings } from '../../shared/ipc/types'
import { useT } from '../../shared/i18n/useT'
import { Badge } from '../../shared/ui'
import { SwitchRow } from '../components/Page'
import { at } from '../lib/patch'
import { patchSettings } from '../store'
import { isJianguoyun, realtimeBlocker, realtimeClipboardBlocker } from './model'

export function RealtimeRows({ sync, state }: { sync: SyncSettings; state: SyncRealtime }) {
  const t = useT()
  const blocker = realtimeBlocker(sync)
  const on = blocker === null && sync.realtime
  const clipBlocker = realtimeClipboardBlocker(sync)
  const preset = t('settings.sync.connect.preset_option.nutstore')

  return (
    <div className="st-subrows" data-testid="sync-realtime-rows">
      <SwitchRow
        label={t('settings.sync.realtime.label')}
        badge={<Badge tone="accent">{t('settings.sync.realtime.badge')}</Badge>}
        description={
          blocker === 'auto'
            ? t('settings.sync.realtime.needs_auto')
            : blocker === 'content'
              ? t('settings.sync.realtime.needs_content')
              : t('settings.sync.realtime.desc')
        }
        message={on ? t(`settings.sync.realtime.state.${state}`) : undefined}
        messageTone={on && state === 'unsupported' ? 'warning' : 'default'}
        checked={on}
        disabled={blocker !== null}
        onChange={(v) => void patchSettings(at('sync.realtime', v))}
        ariaLabel={t('settings.sync.realtime.label')}
        data-testid="sync-realtime"
      />
      <div className="st-subrows">
        <SwitchRow
          label={t('settings.sync.realtime.clipboard')}
          description={
            clipBlocker === 'realtime'
              ? t('settings.sync.realtime.clipboard_needs_realtime')
              : clipBlocker === 'clipboard'
                ? t('settings.sync.realtime.clipboard_needs_history')
                : t('settings.sync.realtime.clipboard_desc')
          }
          checked={clipBlocker === null && sync.realtime_clipboard}
          disabled={clipBlocker !== null}
          onChange={(v) => void patchSettings(at('sync.realtime_clipboard', v))}
          ariaLabel={t('settings.sync.realtime.clipboard')}
          data-testid="sync-realtime-clipboard"
        />
      </div>
      {blocker === null && isJianguoyun(sync.server_url) ? (
        <p className="st-sync-note" data-testid="sync-realtime-quota">
          {t('settings.sync.realtime.jianguoyun', { name: preset })}
        </p>
      ) : null}
    </div>
  )
}
