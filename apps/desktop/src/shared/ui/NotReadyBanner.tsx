/**
 * NotReadyBanner：输入视图顶部的提示条，渲染
 * `InputPayload.hint`。**所有**取词失败都以输入视图打开：用户已经在输入框里，提示条只说明「为什么
 * 没有读到选中的文字」，并给出唯一有意义的动作（缺权限时「去授权」）——已经是输入模式，不需要「改用输入模式」。
 *
 * 映射：`permission_missing` → 「授予…权限后可直接读取选中文本」+ [去授权]；`elevated` / `wayland_copy_first` → 先复制再
 * 粘贴的说明；`selection_unavailable` → 平台层下发的原因（`errors.platform.*`，缺文案时退回通用说明）。其他原因不属于
 * 取词失败（它们出现在翻译 / OCR 视图的 `not_ready` 里，用 NotReadyCard），这里退回该原因的标题，保证不出现空提示条。
 * 出现动效与 Banner 相同（presets.rowInsert）。
 */
import { Accessibility, ClipboardCopy, Keyboard, ScreenShare, ShieldAlert } from 'lucide-react'
import type { LucideIcon } from 'lucide-react'
import { useT, useTKey } from '../i18n/useT'
import type { NotReady, PermissionKind } from '../ipc/types'
import { Banner } from './Banner'
import { Button } from './Button'

export interface NotReadyBannerProps {
  notReady: NotReady
  /** 打开系统设置的授权页（缺权限时显示「去授权」） */
  onOpenSystemSettings?: (permission: PermissionKind) => void
  onDismiss?: () => void
  className?: string
  'data-testid'?: string
}

const PERMISSION_ICONS: Record<PermissionKind, LucideIcon> = {
  accessibility: Accessibility,
  screen_recording: ScreenShare,
  input_monitoring: Keyboard,
}

export function NotReadyBanner({
  notReady,
  onOpenSystemSettings,
  onDismiss,
  className,
  'data-testid': testId = 'not-ready-banner',
}: NotReadyBannerProps) {
  const t = useT()
  const tKey = useTKey()
  let icon: LucideIcon = ClipboardCopy
  let text: string
  let action: { label: string; run: () => void } | null = null
  switch (notReady.reason) {
    case 'permission_missing': {
      const p = notReady.permission
      icon = PERMISSION_ICONS[p]
      text = t(`panel.input_hint.${p}`)
      if (onOpenSystemSettings)
        action = { label: t('panel.input_hint.grant'), run: () => onOpenSystemSettings(p) }
      break
    }
    case 'elevated':
      icon = ShieldAlert
      text = t('panel.input_hint.elevated')
      break
    case 'wayland_copy_first':
      text = t('panel.input_hint.wayland_copy_first')
      break
    case 'selection_unavailable':
      // 平台层下发的 errors.platform.* key；万一缺文案，退回通用说明
      text = tKey(notReady.reason_key, undefined, t('panel.input_hint.selection_unavailable'))
      break
    case 'no_provider':
      text = t('panel.not_ready.no_provider.title')
      break
    case 'no_default_model':
      text = t('panel.not_ready.no_default_model.title')
      break
    case 'no_vision_model':
      text = t('panel.not_ready.no_vision_model.title')
      break
    case 'capture_failed':
      text = t('panel.not_ready.capture_failed.title')
      break
    case 'keychain_unavailable':
      text = t('panel.not_ready.keychain_unavailable.title')
      break
    case 'offline_model_missing':
      text = t('panel.not_ready.offline_model_missing.title', {
        model: t(`common.offline_model.${notReady.model}`),
      })
      break
    case 'offline_model_downloading':
      text = t('panel.not_ready.offline_model_downloading.title', {
        model: t(`common.offline_model.${notReady.model}`),
      })
      break
    case 'offline_task_unsupported':
      text = t('panel.not_ready.offline_task_unsupported.title')
      break
  }
  return (
    <Banner
      tone="warning"
      icon={icon}
      className={className}
      data-testid={testId}
      onDismiss={onDismiss}
      action={
        action ? (
          <Button
            size="sm"
            variant="secondary"
            onClick={action.run}
            data-testid={`${testId}-action`}
          >
            {action.label}
          </Button>
        ) : undefined
      }
    >
      <span data-reason={notReady.reason}>{text}</span>
    </Banner>
  )
}
