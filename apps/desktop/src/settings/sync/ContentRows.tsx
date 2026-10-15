/**
 * 「同步内容」的开关行：已连接页里每次切换直接写入设置，连接子视图里只改草稿（随「连接」一起提交）。
 *
 * - 「API Key」是「设置与模型服务」的子项（缩进的子行）：Key 按服务 id 存，脱离服务同步没有意义，所以父项关闭时它也
 *   禁用并显示为关；
 * - 剪贴板历史与识别历史关闭后，本机上传过的记录会在之后从服务器上移除（其他设备上已有的保留）：已连接时在行内说明；
 * - 识别历史只在本机开启了识别历史时同步，本机关着时就地提示。
 */
import type { SyncContent } from '../../shared/ipc/types'
import { useT } from '../../shared/i18n/useT'
import { SwitchRow } from '../components/Page'

export function ContentRows({
  content,
  onChange,
  ocrHistoryOn,
  connected,
}: {
  content: SyncContent
  onChange: (key: keyof SyncContent, value: boolean) => void
  /** 本机开启了识别历史（`ocr.history`） */
  ocrHistoryOn: boolean
  /** 已连接：关闭剪贴板 / 识别历史时说明服务器上的记录会被移除 */
  connected: boolean
}) {
  const t = useT()
  const removed = t('settings.sync.content.removed')
  return (
    <>
      <SwitchRow
        label={t('settings.sync.content.settings')}
        description={t('settings.sync.content.settings_desc')}
        checked={content.settings}
        onChange={(v) => onChange('settings', v)}
        ariaLabel={t('settings.sync.content.settings')}
        data-testid="sync-content-settings"
      />
      <div className="st-subrows">
        <SwitchRow
          label={t('settings.sync.content.api_keys')}
          description={
            content.settings
              ? t('settings.sync.content.api_keys_desc')
              : t('settings.sync.content.api_keys_off')
          }
          checked={content.settings && content.api_keys}
          disabled={!content.settings}
          onChange={(v) => onChange('api_keys', v)}
          ariaLabel={t('settings.sync.content.api_keys')}
          data-testid="sync-content-api_keys"
        />
      </div>
      <SwitchRow
        label={t('settings.sync.content.clipboard')}
        description={t('settings.sync.content.clipboard_desc')}
        message={connected && !content.clipboard ? removed : undefined}
        messageTone="default"
        checked={content.clipboard}
        onChange={(v) => onChange('clipboard', v)}
        ariaLabel={t('settings.sync.content.clipboard')}
        data-testid="sync-content-clipboard"
      />
      <SwitchRow
        label={t('settings.sync.content.images')}
        description={t('settings.sync.content.images_desc')}
        checked={content.images}
        onChange={(v) => onChange('images', v)}
        ariaLabel={t('settings.sync.content.images')}
        data-testid="sync-content-images"
      />
      <SwitchRow
        label={t('settings.sync.content.ocr_history')}
        description={t('settings.sync.content.ocr_history_desc')}
        message={
          content.ocr_history && !ocrHistoryOn
            ? t('settings.sync.content.ocr_history_local_off')
            : connected && !content.ocr_history
              ? removed
              : undefined
        }
        messageTone="default"
        checked={content.ocr_history}
        onChange={(v) => onChange('ocr_history', v)}
        ariaLabel={t('settings.sync.content.ocr_history')}
        data-testid="sync-content-ocr_history"
      />
    </>
  )
}
