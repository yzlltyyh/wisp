/**
 * 剪贴板视图的宿主适配：同一个视图既在面板里、也在悬浮剪贴板里运行，两者在高度、焦点、拖动、Esc、粘贴只复制之后的
 * 提示与头部按钮上不同。这些差异只在这里按宿主取值，视图与操作注册表只读 `ClipHost` 的字段，不写
 * `host === 'panel'` 分支。
 *
 * 悬浮剪贴板的窗口外壳（`src/floating/`）经 [`ClipFloatContext`] 把窗口级的东西交给视图：继承的查询 / 筛选 / 光标、
 * 置顶开关、关闭、首帧就绪。面板里这个上下文为 null。
 */
import { createContext, useCallback, useContext, useMemo, useState } from 'react'
import type { ViewHost, ViewProps } from '../../panel/types'
import { api, boot } from '../../shared/ipc'
import { usePanelDrag, type PanelDragHandlers } from '../../shared/hooks/usePanelDrag'
import { OPEN_MAX_HEIGHT, openHeight, toastWidth } from './model'
import type { ClipSeed } from './store'

/**
 * 会话内长高时底部（提示条 / 确认条 / 轻提示）怎样跟上卡片当前的底边（见 ClipboardView 的 useGrowFollow）：
 * follow 逐帧读卡片底边（Css 帧模式）、spring 与 Rust 窗口同一条弹簧（Native 帧模式）、none 直接到位
 */
export type GrowFollow = 'follow' | 'spring' | 'none'

/** 前端代写剪贴板之后提示的原因（与 toast 视图的 `reason` 一致） */
export type CopiedReason = 'copy_only' | 'wayland'

/** 宿主提供的能力：注册表的操作据此出现或隐藏（不在操作里判断宿主种类） */
export interface ClipCaps {
  /** 可以交接到悬浮剪贴板（⌘P / 头部的钉子按钮） */
  readonly float: boolean
  /** 连续粘贴不可用的原因（i18n key，例如 Wayland 会话）；可用时为 null */
  readonly queueUnavailable: string | null
  /** Esc 没有别的事可做时关闭窗口（面板）；否则只交还键盘焦点（悬浮窗） */
  readonly escapeCloses: boolean
}

/** 悬浮剪贴板的窗口外壳交给视图的东西（面板里没有） */
export interface ClipFloatShell {
  /** 从面板继承的查询、筛选与光标（只在视图创建时用一次） */
  readonly seed: ClipSeed
  /** 当前是否置顶 */
  readonly onTop: boolean
  setOnTop(on: boolean): void
  /** 关闭（销毁）窗口 */
  close(): void
}

export const ClipFloatContext = createContext<ClipFloatShell | null>(null)

/**
 * 悬浮剪贴板窗口是否已经接入（Rust 的 `clipboard_float_*` 与悬浮窗入口）。接入之前 ⌘P 只拦下、不出现在任何界面上
 */
const FLOAT_WINDOW = true

export interface ClipHost {
  readonly kind: ViewHost
  readonly caps: ClipCaps
  /**
   * 高度策略：grow = clamp(内容, 下限, min(560, 上限))、会话内只增不减（面板：与 Rust 的锁定规则一致，菜单放不下时
   * 先长高）；fill = 占满窗口（悬浮窗由用户调整大小，菜单放不下时翻转或在菜单里滚动）
   */
  readonly sizing: 'grow' | 'fill'
  readonly growFollow: GrowFollow
  /** 从头部 / 提示条的空白处拖动窗口；undefined 表示由系统的拖动区域负责 */
  readonly drag: PanelDragHandlers | undefined
  /** 头部标成原生拖动区域（悬浮窗） */
  readonly dragRegion: boolean
  /**
   * 显示时自动聚焦、焦点丢了补回搜索框（面板「唤出即可打字」）；悬浮窗不抢焦点，点了搜索框才成为 key
   */
  readonly focusOnShow: boolean
  /**
   * 粘贴只复制成功之后谁来提示：toast = 面板（Rust 在原位弹 toast 视图；前端代写剪贴板时由视图切到 toast 视图）；
   * status = 悬浮窗（Rust 不弹，视图在底部轻提示）
   */
  readonly copiedNotice: 'toast' | 'status'
  /** 悬浮窗的窗口外壳（面板里为 null） */
  readonly float: ClipFloatShell | null
  /** 置顶开关的当前状态（没有这个开关的宿主为 null） */
  readonly onTop: boolean | null
  /**
   * 视图里的 Esc 没有可做的事（没有确认条、搜索框已空）时：返回 false 交给外壳（面板关闭）；悬浮窗交还键盘焦点、
   * 不关闭（只有 ✕ / ⌘W 关闭）
   */
  escapeFallback(): boolean
  /** 搜索框获得 / 失去焦点（悬浮窗：打字期间临时可聚焦；面板什么都不做） */
  searchFocus(focused: boolean): void
  /**
   * 前端代写剪贴板之后的提示（KDE Wayland，只用于 `copiedNotice === 'toast'` 的宿主）：面板在原位切换到 toast 视图
   */
  announceCopied(reason: CopiedReason, pasteKey: string): void
}

export function useClipHost(props: ViewProps<'clipboard'>, reducedMotion: boolean): ClipHost {
  const { host: kind, sessionId, frame, switchView, payload } = props
  const panel = kind === 'panel'
  const float = useContext(ClipFloatContext)
  const drag = usePanelDrag(sessionId, panel)
  // 不能透明的窗口（Css 帧模式且没有阴影边距）没有卡片高度动画，减少动效时同样直接到位
  const growFollow: GrowFollow = !panel
    ? 'none'
    : frame === 'css'
      ? boot.shadow_margin > 0
        ? 'follow'
        : 'none'
      : reducedMotion
        ? 'none'
        : 'spring'
  const queueUnavailable = payload.queue_unavailable ?? null
  return useMemo<ClipHost>(
    () => ({
      kind,
      // 悬浮窗本身不再交接到悬浮窗
      caps: { float: panel && FLOAT_WINDOW, queueUnavailable, escapeCloses: panel },
      sizing: panel ? 'grow' : 'fill',
      growFollow,
      drag,
      dragRegion: !panel,
      focusOnShow: panel,
      copiedNotice: panel ? 'toast' : 'status',
      float,
      onTop: float?.onTop ?? null,
      escapeFallback: () => {
        if (panel) return false
        // 悬浮窗：交还键盘焦点（失败无妨，用户点别处一样交还）
        void api.clipboardFloatFocus(false).catch(() => undefined)
        return true
      },
      searchFocus: (focused) => {
        if (panel) return
        void api.clipboardFloatFocus(focused).catch(() => undefined)
      },
      announceCopied: (reason, pasteKey) =>
        switchView({ kind: 'toast', reason, paste_key: pasteKey }, { width: toastWidth(pasteKey) }),
    }),
    [kind, panel, growFollow, drag, switchView, float, queueUnavailable],
  )
}

export interface HostHeight {
  /** 视图的总高度（显式 px：Native 帧模式下 WebView 比卡片高，不能用 100%） */
  readonly total: number
  /** 右键菜单需要的高度超出当前卡片时，请视图先长高（只在 grow 策略下生效） */
  ensureHeight(bottom: number): void
}

/**
 * 视图高度：grow 策略下 clamp(内容, 下限, min(560, 上限))，会话内只增不减（记住出现过的最大值：渲染期更新，
 * 不多一次提交）；fill 策略下占满上限
 */
export function useHostHeight(host: ClipHost, natural: number, maxHeight: number): HostHeight {
  const grow = host.sizing === 'grow'
  const cap = Math.min(OPEN_MAX_HEIGHT, maxHeight)
  const [peak, setPeak] = useState(0)
  const [extra, setExtra] = useState(0)
  const total = grow
    ? Math.max(peak, openHeight(natural, maxHeight), Math.min(extra, cap))
    : maxHeight
  if (grow && total !== peak) setPeak(total)
  const ensureHeight = useCallback(
    (bottom: number) => {
      if (grow) setExtra((e) => Math.max(e, Math.ceil(bottom)))
    },
    [grow],
  )
  return { total, ensureHeight }
}
