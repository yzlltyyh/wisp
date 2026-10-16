/**
 * 粘贴只复制成功、连续粘贴进度的文案：面板的 toast 视图与悬浮剪贴板的轻提示共用，同一个原因在两处说法一致。
 */
import type { CopiedReason, OsKind, QueueStatus } from '../../shared/ipc/types'
import type { TFunction } from '../../shared/i18n/useT'

/** 原因说明（仅复制、`manual`——只提示按键、不写原因——没有说明） */
export function copiedReasonText(reason: CopiedReason, os: OsKind, t: TFunction): string | null {
  switch (reason) {
    case 'copy_only':
    case 'manual':
      return null
    case 'no_permission':
      return os === 'macos'
        ? t('clipboard.toast.reason.no_permission_macos')
        : t('clipboard.toast.reason.no_permission')
    default:
      return t(`clipboard.toast.reason.${reason}`)
  }
}

/** 一行写完的提示（悬浮窗的轻提示）：「已复制，按 ⌘V 粘贴 · 原因」；仅复制只说「已复制」 */
export function copiedNotice(
  reason: CopiedReason,
  os: OsKind,
  pasteKey: string,
  t: TFunction,
): string {
  const title =
    reason === 'copy_only'
      ? t('clipboard.toast.copied')
      : t('clipboard.toast.press_to_paste', { key: pasteKey })
  const detail = copiedReasonText(reason, os, t)
  return detail ? `${title} · ${detail}` : title
}

/** 连续粘贴的进度：进行中「连续粘贴 · 2 / 5」，结束时说明是粘贴完了还是停下了 */
export function queueNotice(q: QueueStatus, t: TFunction): string {
  if (q.active) return t('clipboard.queue.progress', { index: q.index, total: q.total })
  return q.total > 0 && q.index >= q.total
    ? t('clipboard.queue.done')
    : t('clipboard.queue.stopped')
}
