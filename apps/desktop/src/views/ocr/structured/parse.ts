/**
 * 结构化识别结果（Markdown）的预览解析：把正文切成「普通 Markdown」「表格」「公式」三种块。
 *
 * - 管道表格（GFM）：表头行 + 分隔行（`| --- | :-: |`）+ 数据行；单元格按未转义、不在行内代码里的 `|` 切开；
 * - HTML 表格（PaddleX 的版面解析与 PaddleOCR-VL 输出 `<table>`，可能带合并单元格）：用 DOMParser 解析成**惰性**文档
 *   （不执行脚本、不加载资源），只取行、单元格的纯文本与 colspan / rowspan，再由 React 重新生成——不把任何 HTML 插进页面；
 * - 公式：独立成行的 `$$…$$` / `\[…\]`（可以跨行）原样保留源码，等宽显示（不引入公式排版库）；行内的 `$…$` 只在
 *   像 LaTeX（含 `\`、`^`、`_`、`{`）时按行内代码显示，「$5 和 $10」这类金额不受影响；
 * - 围栏代码块里的内容不解析；图片（`<img>`、`![](…)`）不显示（识别结果里的区块截图没有下载）。
 * 其余内容交给 Markdown-lite（shared/markdown）按原来的方式渲染。
 */

export interface Cell {
  text: string
  header: boolean
  colSpan: number
  rowSpan: number
  align: 'left' | 'center' | 'right' | null
}

export type Block =
  { kind: 'md'; text: string } | { kind: 'table'; rows: Cell[][] } | { kind: 'math'; tex: string }

/** 合并单元格的上限（异常的 colspan="9999" 不会撑出一张巨表） */
const MAX_SPAN = 50

const FENCE = /^\s{0,3}(`{3,}|~{3,})/
const DELIM_ROW = /^\s*\|?(\s*:?-+:?\s*\|)+\s*:?-*:?\s*$/
const IMAGE_MD = /!\[[^\]]*\]\([^)]*\)/g
const IMAGE_HTML = /<img\b[^>]*>/gi
/** 只包着表格 / 图片的外层标签（PaddleX 的 `<div style="text-align: center;">`、`<html><body>`） */
const WRAPPER_LINE = /^\s*(<\/?(?:div|html|body|center|p)\b[^>]*>\s*)+$/i

/** 切开一行管道表格的单元格 */
export function splitRow(line: string): string[] {
  let s = line.trim()
  if (s.startsWith('|')) s = s.slice(1)
  if (s.endsWith('|') && !s.endsWith('\\|')) s = s.slice(0, -1)
  const cells: string[] = []
  let buf = ''
  let code = false
  for (let i = 0; i < s.length; i++) {
    const c = s[i] as string
    if (c === '\\' && s[i + 1] === '|') {
      buf += '|'
      i++
      continue
    }
    if (c === '`') code = !code
    if (c === '|' && !code) {
      cells.push(buf.trim())
      buf = ''
      continue
    }
    buf += c
  }
  cells.push(buf.trim())
  return cells
}

function alignOf(spec: string): Cell['align'] {
  const s = spec.trim()
  const left = s.startsWith(':')
  const right = s.endsWith(':')
  if (left && right) return 'center'
  if (right) return 'right'
  if (left) return 'left'
  return null
}

const cell = (text: string, header: boolean, align: Cell['align'] = null): Cell => ({
  text,
  header,
  colSpan: 1,
  rowSpan: 1,
  align,
})

/** 管道表格：lines[0] 表头、lines[1] 分隔行、其余数据行 */
function pipeTable(lines: readonly string[]): Cell[][] {
  const aligns = splitRow(lines[1] ?? '').map(alignOf)
  const width = aligns.length
  const pad = (cells: string[], header: boolean) =>
    Array.from({ length: Math.max(width, 1) }, (_, i) =>
      cell(cells[i] ?? '', header, aligns[i] ?? null),
    )
  return [
    pad(splitRow(lines[0] ?? ''), true),
    ...lines.slice(2).map((l) => pad(splitRow(l), false)),
  ]
}

const clampSpan = (n: number) =>
  Number.isFinite(n) && n >= 1 ? Math.min(Math.floor(n), MAX_SPAN) : 1

/** HTML 表格：只取文本与合并信息；解析失败（没有 DOMParser 或没有行）时为 null */
export function htmlTable(source: string): Cell[][] | null {
  if (typeof DOMParser === 'undefined') return null
  const doc = new DOMParser().parseFromString(source, 'text/html')
  const table = doc.querySelector('table')
  if (!table) return null
  const rows: Cell[][] = []
  for (const tr of Array.from(table.querySelectorAll('tr'))) {
    const inHead = tr.parentElement?.tagName === 'THEAD'
    const cells = Array.from(tr.children)
      .filter((el) => el.tagName === 'TD' || el.tagName === 'TH')
      .map((el) => ({
        text: (el.textContent ?? '').replace(/\s+/g, ' ').trim(),
        header: inHead || el.tagName === 'TH',
        colSpan: clampSpan(Number(el.getAttribute('colspan') ?? 1)),
        rowSpan: clampSpan(Number(el.getAttribute('rowspan') ?? 1)),
        align: null,
      }))
    if (cells.length > 0) rows.push(cells)
  }
  return rows.length > 0 ? rows : null
}

/** 行内公式（像 LaTeX 的 `$…$`）改成行内代码，交给 Markdown-lite 等宽显示 */
function inlineMath(text: string): string {
  return text.replace(
    /(^|[^\\$])\$([^$\n`]*[\\^_{][^$\n`]*)\$(?!\$)/g,
    (_m, pre: string, tex: string) => {
      return `${pre}\`$${tex}$\``
    },
  )
}

/** 切块 */
export function parseStructured(source: string): Block[] {
  const lines = source.replace(/\r\n?/g, '\n').split('\n')
  const out: Block[] = []
  let md: string[] = []
  const flush = () => {
    const text = md
      .join('\n')
      .replace(IMAGE_MD, '')
      .replace(IMAGE_HTML, '')
      .replace(/\n{3,}/g, '\n\n')
    if (text.trim() !== '') out.push({ kind: 'md', text: inlineMath(text) })
    md = []
  }
  let fence: string | null = null
  let i = 0
  while (i < lines.length) {
    const line = lines[i] as string
    const f = FENCE.exec(line)
    if (fence !== null) {
      md.push(line)
      if (f && (f[1] as string).startsWith(fence)) fence = null
      i++
      continue
    }
    if (f) {
      fence = (f[1] as string).slice(0, 3)
      md.push(line)
      i++
      continue
    }
    // 公式块：$$ … $$ / \[ … \]（同一行闭合或跨行）
    const open = /^\s*(\$\$|\\\[)/.exec(line)
    if (open) {
      const close = open[1] === '$$' ? '$$' : '\\]'
      const start = line.indexOf(open[1] as string) + (open[1] as string).length
      const rest = line.slice(start)
      const end = rest.indexOf(close)
      let body: string
      if (end >= 0) {
        body = rest.slice(0, end)
        i++
      } else {
        const parts = [rest]
        let j = i + 1
        while (j < lines.length && !(lines[j] as string).includes(close))
          parts.push(lines[j++] as string)
        if (j < lines.length)
          parts.push((lines[j] as string).slice(0, (lines[j] as string).indexOf(close)))
        body = parts.join('\n')
        i = j + 1
      }
      flush()
      out.push({ kind: 'math', tex: body.trim() })
      continue
    }
    // HTML 表格：从 <table 到 </table>（可以跨行）
    const tableAt = line.search(/<table[\s>]/i)
    if (tableAt >= 0) {
      const before = line.slice(0, tableAt)
      let buf = line.slice(tableAt)
      let j = i
      while (!/<\/table>/i.test(buf) && j + 1 < lines.length) buf += `\n${lines[++j] as string}`
      const endAt = buf.search(/<\/table>/i)
      const html = endAt >= 0 ? buf.slice(0, endAt + '</table>'.length) : buf
      const after = endAt >= 0 ? buf.slice(endAt + '</table>'.length) : ''
      const rows = htmlTable(html)
      if (!WRAPPER_LINE.test(before) && before.trim() !== '') md.push(before)
      if (rows) {
        flush()
        out.push({ kind: 'table', rows })
      } else md.push(html)
      if (!WRAPPER_LINE.test(after) && after.trim() !== '') md.push(after)
      i = j + 1
      continue
    }
    // 管道表格：这一行有 `|`、下一行是分隔行
    if (line.includes('|') && DELIM_ROW.test(lines[i + 1] ?? '')) {
      let j = i + 2
      while (
        j < lines.length &&
        (lines[j] as string).includes('|') &&
        (lines[j] as string).trim() !== ''
      )
        j++
      flush()
      out.push({ kind: 'table', rows: pipeTable(lines.slice(i, j)) })
      i = j
      continue
    }
    if (!WRAPPER_LINE.test(line)) md.push(line)
    i++
  }
  flush()
  return out
}
