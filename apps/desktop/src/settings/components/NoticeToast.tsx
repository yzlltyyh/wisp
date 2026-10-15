/**
 * 窗口底部的轻提示（保存失败、已导出、已复制路径……）。只有一条；presets.toast 进出（减少动效时只淡入淡出）。
 * aria-live 播报。
 *
 * - 停留时长来自动效令牌（store.notify：成功 / 信息 `timing.toastHold`，错误与带路径的结果不自动消失）；
 *   指针悬停或焦点在提示上时暂停计时；
 * - 提示出现期间给 `<html>` 加 `data-notice`：设置页底部多留出提示的高度、滚入视野时让开它（settings.css），
 *   页尾的确认条、下拉框不会被它压住。
 */
import { CircleAlert, CircleCheck, Info, X } from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useEffect } from 'react'
import { useT } from '../../shared/i18n/useT'
import { usePreset } from '../../shared/motion/presets'
import { Icon } from '../../shared/ui'
import { dismissNotice, holdNotice, useSettingsStore } from '../store'

const ICONS = { info: Info, success: CircleCheck, danger: CircleAlert } as const

export function NoticeToast() {
  const t = useT()
  const notice = useSettingsStore((s) => s.notice)
  const motion = usePreset('toast')
  const shown = notice !== null
  useEffect(() => {
    const root = document.documentElement
    if (shown) root.dataset.notice = 'true'
    else delete root.dataset.notice
    return () => {
      delete root.dataset.notice
    }
  }, [shown])
  return (
    <output className="st-notice-host" aria-live="polite">
      <AnimatePresence>
        {notice ? (
          <m.div
            key={notice.id}
            className="st-notice"
            data-tone={notice.tone}
            data-testid="settings-notice"
            {...motion}
            onPointerEnter={() => holdNotice(true)}
            onPointerLeave={() => holdNotice(false)}
            onFocus={() => holdNotice(true)}
            onBlur={(e) => {
              if (!e.currentTarget.contains(e.relatedTarget as Node | null)) holdNotice(false)
            }}
          >
            <Icon icon={ICONS[notice.tone]} size={16} className="st-notice-icon" />
            <span className="st-notice-text">{notice.text}</span>
            <button
              type="button"
              className="st-notice-close"
              aria-label={t('common.a11y.dismiss')}
              onClick={dismissNotice}
            >
              <Icon icon={X} size={12} />
            </button>
          </m.div>
        ) : null}
      </AnimatePresence>
    </output>
  )
}
