/**
 * 中文说明里引用的按钮名 / 词组整体不拆行：「重新启动 Wisp」『…』“…”《…》（…）这类全角括号 / 引号括住的
 * 短片段包进 `<span class="ui-nobr">`（white-space: nowrap）。
 *
 * 断行规则本身交给 CSS（`word-break: normal; line-break: strict`）：严格断行保证开括号不留在行尾、闭括号与句读不出现在
 * 行首。不用 `word-break: keep-all` 来避免拆词（它为韩文设计，在 WebKit 里配合 `text-wrap: balance` 会在开括号后面
 * 断行）；真正不想被拆开的只有这些引用的名字，由这里按片段处理。
 *
 * - 只认全角括号 / 引号（英文文案不受影响，也不需要）；片段内容 1–14 个字符、不含换行与嵌套括号——更长的片段照常折行，
 *   窄容器里不会因为一整段不折而溢出；
 * - 只处理字符串；其他节点原样返回。结果的文本内容与原文逐字相同（读屏、textContent、复制都不变）。
 */
import { Fragment, type ReactNode } from 'react'

const QUOTED = /[「『“《（][^「」『』“”《》（）\n]{1,14}[」』”》）]/g

export function keepQuoted(text: ReactNode): ReactNode {
  if (typeof text !== 'string' || !text) return text
  const parts: ReactNode[] = []
  let last = 0
  for (const m of text.matchAll(QUOTED)) {
    const at = m.index ?? 0
    if (at > last) parts.push(text.slice(last, at))
    parts.push(
      <span key={at} className="ui-nobr">
        {m[0]}
      </span>,
    )
    last = at + m[0].length
  }
  if (parts.length === 0) return text
  if (last < text.length) parts.push(text.slice(last))
  return <Fragment>{parts}</Fragment>
}
