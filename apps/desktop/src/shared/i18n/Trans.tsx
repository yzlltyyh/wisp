/**
 * 富文本插值：文案里的 `{name}` 可以替换成 React 节点（例如 `<Kbd>`），
 * 用于「再按一次 ⌫ 删除置顶项」「{esc} 取消 · {backspace} 清除」这类句子，语序完全由文案决定。文字片段经 keepQuoted：
 * 引用的按钮名（「重新启动 Wisp」）整体不拆行。
 */
import { Fragment, type ReactNode } from 'react'
import type { MessageKey, TParams } from './i18n'
import { keepQuoted } from './keepQuoted'
import { useT } from './useT'

export interface TransProps {
  k: MessageKey
  /** 文本参数（参与复数选择与普通插值） */
  params?: TParams
  /** 节点参数：`{name}` 替换为对应节点 */
  nodes: Record<string, ReactNode>
}

const TOKEN = /\{(\w+)\}/g

export function Trans({ k, params, nodes }: TransProps) {
  const t = useT()
  // 先让 t 处理文本参数与复数，节点参数的占位符原样保留，再切分替换
  const text = t(k, params)
  const parts: ReactNode[] = []
  let last = 0
  for (const m of text.matchAll(TOKEN)) {
    const name = m[1] ?? ''
    if (!(name in nodes)) continue
    const at = m.index ?? 0
    if (at > last)
      parts.push(<Fragment key={`t-${last}`}>{keepQuoted(text.slice(last, at))}</Fragment>)
    parts.push(<Fragment key={`${name}-${at}`}>{nodes[name]}</Fragment>)
    last = at + m[0].length
  }
  // 文字片段里引用的按钮名（「重新启动 Wisp」）整体不拆行（keepQuoted.tsx）
  if (last < text.length)
    parts.push(<Fragment key={`t-${last}`}>{keepQuoted(text.slice(last))}</Fragment>)
  return <>{parts}</>
}
