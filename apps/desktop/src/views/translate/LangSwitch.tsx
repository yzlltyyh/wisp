/**
 * 翻译 / 输入视图头部的语言条「英语 → 中文 ▾」：组件库的 LangBar（交换方向的
 * Tooltip / 不可用原因 / 目标语言的无障碍名称都在那里）加上本视图的两点：
 * - 交换方向的三种状态由 `SwapControl` 表达（text.ts `planSwap`：可用 / 不可用的原因 / 未就绪）；
 * - 目标语言菜单打开期间吞掉 ⌘R / ⌘⇧R / ⌘P（Windows WebView2 默认会重载 / 打印，keys.ts）。
 */
import { useState } from 'react'
import type { Lang } from '../../shared/ipc/types'
import { LangBar } from '../../shared/ui'
import { useMenuKeys } from './keys'
import { loadTranslateMessages } from './i18n'

loadTranslateMessages()

/** 交换方向：可用（点击执行）/ 不可用（原因）；null = 不提供（未就绪） */
export type SwapControl = { readonly run: () => void } | { readonly reason: string } | null

export interface LangSwitchProps {
  /** 显示的源语言；null 且 detecting 为 true 时显示「检测中…」 */
  source: Lang | null
  target: Lang
  detecting?: boolean
  lowConfidence?: boolean
  /** 可选的目标语言（主语言 / 第二语言） */
  targets: readonly Lang[]
  /** 不传则目标语言不可点（未就绪） */
  onPickTarget?: (lang: Lang) => void
  swap: SwapControl
  'data-testid'?: string
}

export function LangSwitch({
  source,
  target,
  detecting = false,
  lowConfidence = false,
  targets,
  onPickTarget,
  swap,
  'data-testid': testId = 'lang-switch',
}: LangSwitchProps) {
  const [open, setOpen] = useState(false)
  useMenuKeys(open)
  return (
    <LangBar
      source={source}
      target={target}
      detecting={detecting}
      lowConfidence={lowConfidence}
      targets={targets}
      onPickTarget={onPickTarget}
      onSwap={swap === null ? undefined : 'run' in swap ? swap.run : () => undefined}
      swapDisabledReason={swap !== null && 'reason' in swap ? swap.reason : null}
      onMenuOpenChange={setOpen}
      className="tv-langbar"
      data-testid={testId}
    />
  )
}
