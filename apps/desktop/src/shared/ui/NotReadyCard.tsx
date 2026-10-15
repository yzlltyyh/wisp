/**
 * NotReadyCard：翻译 / OCR 视图的 `not_ready`（没有接入模型、没有默认 / 视觉模型、缺屏幕录制权限、
 * 截图失败、系统凭据存储不可用）。结构同 ErrorCard，语气是引导而不是报错（图标用中性色）。按钮只显示调用方提供了回调的那些。
 * 出现动效 presets.cardIn。
 *
 * 取词失败（缺辅助功能权限、管理员窗口、Wayland 先复制、平台层给出原因的取词不可用）不走这张卡片：它们以输入视图打开，
 * 由 NotReadyBanner 在输入框上方显示提示条。这里仍能渲染这些原因（类型上穷尽），但不提供
 * 「改用输入模式」——用户已经在输入视图里。
 * `capture_failed.message` 是 i18n key（`errors.capture.*` / `errors.platform.wayland_capture`），缺文案时显示通用说明，
 * 不会把开发者文本原样显示给用户。
 *
 * - `detail`：覆盖默认说明（OCR 视图把「授权后按 ⌥S 重新截图」与说明合并成一句、带键帽）；
 * - `onRetry`：截图失败时的「重新截图」（`trigger_action(…, origin = panel)`；`retryShortcut` 是截图快捷键，
 *   以键帽显示在按钮里）。缺屏幕录制权限时没有它——macOS 要重启 Wisp 才生效，重新截图仍会落回这张卡片；
 * - `keychain_unavailable` 同样有「去设置」（Key 只能在设置里重新填写）。
 */
import type { LucideIcon } from 'lucide-react'
import {
  Accessibility,
  ClipboardCopy,
  Cpu,
  Eye,
  KeyRound,
  ScreenShare,
  ShieldAlert,
  Keyboard,
  ImageOff,
} from 'lucide-react'
import { m } from 'motion/react'
import type { ReactNode } from 'react'
import { keepQuoted } from '../i18n/keepQuoted'
import { useT, useTKey } from '../i18n/useT'
import type { NotReady, PermissionKind } from '../ipc/types'
import { usePreset } from '../motion/presets'
import { Button } from './Button'
import { cx } from './cx'
import { Icon } from './Icon'
import { Kbd } from './Kbd'

export interface NotReadyCardProps {
  notReady: NotReady
  onOpenSettings?: () => void
  onOpenSystemSettings?: (permission: PermissionKind) => void
  /**
   * 重新启动 Wisp（macOS 的屏幕录制权限要重启进程才生效：授权后不重启，再次截图仍会落到这张卡片上）。
   * 提供时屏幕录制卡片多一个「重新启动 Wisp」按钮
   */
  onRelaunch?: () => void
  /** 重新截图（截图失败时显示） */
  onRetry?: () => void
  /** 「重新截图」按钮里显示的快捷键（组合串） */
  retryShortcut?: string | null
  /** 覆盖默认说明（调用方把补充提示合并进来时用） */
  detail?: ReactNode
  className?: string
  'data-testid'?: string
}

const PERMISSION_ICONS: Record<PermissionKind, LucideIcon> = {
  accessibility: Accessibility,
  screen_recording: ScreenShare,
  input_monitoring: Keyboard,
}

export function NotReadyCard({
  notReady,
  onOpenSettings,
  onOpenSystemSettings,
  onRelaunch,
  onRetry,
  retryShortcut = null,
  detail: detailOverride,
  className,
  'data-testid': testId = 'not-ready-card',
}: NotReadyCardProps) {
  const t = useT()
  const tKey = useTKey()
  const enter = usePreset('cardIn')
  let icon: LucideIcon = Cpu
  let title: string
  let detail: ReactNode
  const buttons: Array<{ id: string; label: string; run: () => void; shortcut?: string | null }> =
    []
  switch (notReady.reason) {
    case 'no_provider':
      title = t('panel.not_ready.no_provider.title')
      detail = t('panel.not_ready.no_provider.detail')
      if (onOpenSettings)
        buttons.push({
          id: 'settings',
          label: t('panel.not_ready.no_provider.action'),
          run: onOpenSettings,
        })
      break
    case 'no_default_model':
      title = t('panel.not_ready.no_default_model.title')
      detail = t('panel.not_ready.no_default_model.detail', {
        task: t(`panel.task.${notReady.task}`),
      })
      if (onOpenSettings)
        buttons.push({
          id: 'settings',
          label: t('panel.not_ready.no_default_model.action'),
          run: onOpenSettings,
        })
      break
    case 'no_vision_model':
      icon = Eye
      title = t('panel.not_ready.no_vision_model.title')
      detail = t('panel.not_ready.no_vision_model.detail')
      if (onOpenSettings)
        buttons.push({
          id: 'settings',
          label: t('panel.not_ready.no_vision_model.action'),
          run: onOpenSettings,
        })
      break
    case 'permission_missing': {
      const p = notReady.permission
      icon = PERMISSION_ICONS[p]
      title = t(`panel.not_ready.permission.${p}.title`)
      detail = t(`panel.not_ready.permission.${p}.detail`)
      if (onOpenSystemSettings)
        buttons.push({
          id: 'system',
          label: t('panel.not_ready.permission.action'),
          run: () => onOpenSystemSettings(p),
        })
      if (p === 'screen_recording' && onRelaunch)
        buttons.push({
          id: 'relaunch',
          label: t('panel.not_ready.permission.relaunch'),
          run: onRelaunch,
        })
      break
    }
    case 'wayland_copy_first':
      icon = ClipboardCopy
      title = t('panel.not_ready.wayland_copy_first.title')
      detail = t('panel.not_ready.wayland_copy_first.detail')
      break
    case 'capture_failed':
      icon = ImageOff
      title = t('panel.not_ready.capture_failed.title')
      // message 是 i18n key；缺文案时退回通用说明
      detail = tKey(notReady.message, undefined, t('panel.not_ready.capture_failed.detail'))
      if (onRetry)
        buttons.push({
          id: 'retry',
          label: t('panel.not_ready.retake'),
          run: onRetry,
          shortcut: retryShortcut,
        })
      break
    case 'elevated':
      icon = ShieldAlert
      title = t('panel.not_ready.elevated.title')
      detail = t('panel.not_ready.elevated.detail')
      break
    case 'keychain_unavailable':
      icon = KeyRound
      title = t('panel.not_ready.keychain_unavailable.title')
      detail = t('panel.not_ready.keychain_unavailable.detail')
      if (onOpenSettings)
        buttons.push({ id: 'settings', label: t('common.action.go_settings'), run: onOpenSettings })
      break
    case 'selection_unavailable':
      icon = ClipboardCopy
      title = t('panel.not_ready.selection_unavailable.title')
      // 原因是平台层下发的 errors.platform.* key；万一缺文案，退回通用说明
      detail = tKey(
        notReady.reason_key,
        undefined,
        t('panel.not_ready.selection_unavailable.detail'),
      )
      break
  }
  return (
    <m.section
      aria-live="polite"
      className={cx('ui-notready', className)}
      data-reason={notReady.reason}
      data-testid={testId}
      {...enter}
    >
      <span className="ui-notready-icon" aria-hidden="true">
        <Icon icon={icon} size={18} />
      </span>
      <p className="ui-notready-title">{title}</p>
      <p className="ui-notready-detail">{keepQuoted(detailOverride ?? detail)}</p>
      {buttons.length > 0 ? (
        <div className="ui-notready-actions">
          {buttons.map((b, i) => (
            <Button
              key={b.id}
              size="sm"
              variant={i === 0 ? 'primary' : 'secondary'}
              onClick={b.run}
              data-testid={`${testId}-${b.id}`}
            >
              {b.label}
              {b.shortcut ? <Kbd combo={b.shortcut} variant="inline" /> : null}
            </Button>
          ))}
        </div>
      ) : null}
    </m.section>
  )
}
