/**
 * 粘贴结果反馈视图：Rust 在只复制的各个分支（⌘↩ 仅复制、Wayland、目标应用
 * 以管理员身份运行、原应用已退出、焦点切换超时、缺少辅助功能权限）以本视图不激活地显示面板，停留一会儿后前端播放
 * 退出动画并 `panel_hide(other)`；点击即关闭，指针停在上面时暂停计时。
 *
 * 版式：图标 + 一行主文案（「已复制」或「已复制，按 ⌘V 粘贴」，粘贴键按 `ToastPayload.paste_key` 原样渲染成键帽）
 * + 原因说明（仅复制时没有）。内容出现用 presets.toast，图标 presets.statusPulse。
 *
 * - 停留时长：只有「已复制」一行时 `timing.toastViewHold`（约 1.6 s）；带原因说明的按文案长度加长、夹在
 *   `timing.toastViewDetailMin` / `Max`（4–5 s）之间——两三行说明在 1.6 s 里读不完，可点按的也来不及把指针移上去；
 * - macOS 缺少辅助功能权限：说明下面一行强调色的行动入口「在 … 中允许 Wisp ↗」，悬停有底色，点按直接打开
 *   「辅助功能」设置页；
 * - 面板持有焦点时（KDE Wayland 由剪贴板视图在原位切过来：Wayland 只允许获焦的客户端写剪贴板，只能复制、不能代为
 *   粘贴）：主文案改为「关闭提示后按 Ctrl+V 粘贴」，任何按键（包括单按修饰键）或失焦都立即隐藏、把焦点还给
 *   原来的应用——这时按下的那一次粘贴键可能落在 Wisp 上（隐藏与还原焦点需要一点时间），所以文案请用户在提示关闭之后再按；完整的「写入 → 隐藏 → 不激活的 toast」
 *   需要 Rust。这种情况停留时长不加长（用户在等它关掉）；
 * - 读屏：live region 先挂载、文案在下一帧写入，确保会被播报（aria-label 会盖掉内容，不用）。
 */
import './toast.css'
import { ArrowUpRight, CircleCheck, ClipboardCheck } from 'lucide-react'
import { m } from 'motion/react'
import { useEffect, useState } from 'react'
import type { ViewProps } from '../../panel/types'
import { api } from '../../shared/ipc'
import type { CopiedReason, OsKind } from '../../shared/ipc/types'
import { usePlatform } from '../../shared/hooks/usePlatform'
import { keepQuoted } from '../../shared/i18n/keepQuoted'
import { Trans } from '../../shared/i18n/Trans'
import { useT, type TFunction } from '../../shared/i18n/useT'
import { usePreset } from '../../shared/motion/presets'
import { ms, timing } from '../../shared/motion/tokens'
import { Icon } from '../../shared/ui'
import { loadClipboardMessages } from '../clipboard/i18n'

loadClipboardMessages()

/** 原因说明（仅复制、`manual`——只提示按键、不写原因——没有说明） */
export function toastReason(reason: CopiedReason, os: OsKind, t: TFunction): string | null {
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

/**
 * 停留时长（毫秒）：只有一行时是基准（约 1.6 s）；有说明或行动入口时按字数加长，夹在约 4–5 s；`inPlace`（面板持有
 * 焦点、用户在等它关掉再粘贴）时恒为基准
 */
export function toastHoldMs(text: string, hasDetail: boolean, inPlace: boolean): number {
  const base = timing.toastViewHold
  if (inPlace || !hasDetail) return ms(base)
  const chars = [...text.replace(/\s+/g, '')].length
  const hold = base + chars * timing.toastViewPerChar
  return ms(Math.min(timing.toastViewDetailMax, Math.max(timing.toastViewDetailMin, hold)))
}

export function ToastView(props: ViewProps<'toast'>) {
  const { payload, phase, requestHide, sessionId, focused } = props
  const t = useT()
  const os = usePlatform()
  const pulse = usePreset('statusPulse')
  const enter = usePreset('toast')
  const [hovered, setHovered] = useState(false)
  const visible = phase === 'entering' || phase === 'shown'
  const copyOnly = payload.reason === 'copy_only'
  // 面板持有焦点：剪贴板视图在原位切过来的前端退路（正常情况下 Rust 不激活地显示 toast，focused 为 false）
  const inPlace = focused
  const detail = toastReason(payload.reason, os, t)
  const openSettings = payload.reason === 'no_permission' && os === 'macos'
  const action = openSettings ? t('clipboard.toast.action.allow_accessibility') : null
  const titleKey = copyOnly
    ? null
    : inPlace
      ? 'clipboard.toast.press_after_close'
      : 'clipboard.toast.press_to_paste'
  const title = titleKey ? t(titleKey, { key: payload.paste_key }) : t('clipboard.toast.copied')
  const spokenText = [title, detail, action].filter(Boolean).join('. ')
  const hold = toastHoldMs(spokenText, detail !== null, inPlace)

  // 显示后停留一会儿再淡出；指针停在上面时暂停，移开后重新计时（以 visible 而不是 phase 为依赖：entering → shown
  // 不会重置计时）
  useEffect(() => {
    if (!visible || hovered) return
    const id = setTimeout(() => requestHide('other'), hold)
    return () => clearTimeout(id)
  }, [visible, hovered, requestHide, hold])

  // 面板持有焦点（前端退路）：任何按键（包括单按 Ctrl / ⌘）或失焦立即让出焦点
  useEffect(() => {
    if (!visible || !inPlace) return
    const leave = () => requestHide('other')
    window.addEventListener('keydown', leave, true)
    window.addEventListener('blur', leave)
    return () => {
      window.removeEventListener('keydown', leave, true)
      window.removeEventListener('blur', leave)
    }
  }, [visible, inPlace, requestHide])

  // 读屏播报：live region 先空着挂载，文案下一帧再写进去
  const [spoken, setSpoken] = useState('')
  useEffect(() => {
    if (!visible) return
    const id = requestAnimationFrame(() => setSpoken(spokenText))
    return () => cancelAnimationFrame(id)
  }, [visible, spokenText])
  return (
    <div
      className="toast"
      data-testid="view-toast"
      data-view="toast"
      data-session={sessionId}
      data-reason={payload.reason}
      data-actionable={openSettings || undefined}
    >
      <m.span className="toast-icon" data-tone={copyOnly ? 'success' : 'info'} {...pulse}>
        <Icon icon={copyOnly ? CircleCheck : ClipboardCheck} size={18} />
      </m.span>
      <m.span className="toast-text" {...enter}>
        <span className="toast-title" data-testid="toast-title">
          {titleKey ? (
            <Trans
              k={titleKey}
              nodes={{
                key: (
                  <kbd className="ui-kbd toast-key">
                    <span className="ui-kbd-key">{payload.paste_key}</span>
                  </kbd>
                ),
              }}
            />
          ) : (
            keepQuoted(title)
          )}
        </span>
        {detail ? (
          <span className="toast-detail" data-testid="toast-detail">
            {keepQuoted(detail)}
          </span>
        ) : null}
        {action ? (
          <span className="toast-action" data-testid="toast-action">
            {action}
            <Icon icon={ArrowUpRight} size={12} className="toast-action-icon" />
          </span>
        ) : null}
      </m.span>
      {/* 整张卡片都是关闭按钮（面板不激活、拿不到焦点，所以不进 Tab 顺序）；指针停在上面时暂停计时。
          macOS 缺少辅助功能权限时点按打开设置页（有可见的行动入口与悬停底色） */}
      <button
        type="button"
        className="toast-dismiss"
        tabIndex={-1}
        aria-label={openSettings ? t('clipboard.toast.open_settings') : t('common.a11y.dismiss')}
        onClick={() => {
          if (openSettings) void api.permissionsOpen('accessibility').catch(() => undefined)
          requestHide('other')
        }}
        onPointerEnter={() => setHovered(true)}
        onPointerLeave={() => setHovered(false)}
        data-testid="toast-dismiss"
      />
      <output className="ui-sr-only" aria-live="polite" data-testid="toast-live">
        {spoken}
      </output>
    </div>
  )
}
