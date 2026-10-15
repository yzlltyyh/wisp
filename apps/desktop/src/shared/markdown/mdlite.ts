/**
 * Markdown-lite：只支持粗体、斜体、行内代码、代码块、有序 / 无序列表、标题，
 * 另外把 `---` 识别为分隔线、把以 `|` 开头的行识别为表格并原样等宽显示。不支持链接与 HTML（原样显示）。
 *
 * 块级（逐行）：
 * - 围栏代码块 ``` / ~~~（未闭合时按代码块显示：看到围栏行就立即切换样式）；
 * - ATX 标题 `#`–`######`；setext 标题（段落下一行是 `===` / `---`）；
 * - 列表项 `- * +` / `1. 1)`；空行之后同类型的列表项继续同一个列表；非空、非块起始的行接在当前项后面；
 * - 空行结束段落；段落内的单个换行保留（CSS `white-space: pre-line`，中英文都不会插入多余空格）。
 *
 * 行内（简化的 CommonMark 定界符匹配）：`` ` `` 代码、`**` / `__` 粗体、`*` / `_` 斜体、`\` 转义；
 * 单词内部的 `_` 不算定界符（snake_case）；左右两侧是空白的 `*` 原样显示（`5 * 3`）。
 * `optimistic` 为 true 时（流式中的最后一块），未闭合的 `**` / `*` / `` ` `` 按已闭合渲染到块尾，避免先显示星号再变样式。
 */

export interface Inline {
  text: string
  bold: boolean
  italic: boolean
  code: boolean
}

export type MdBlock =
  | { kind: 'paragraph'; inlines: Inline[] }
  | { kind: 'heading'; level: number; inlines: Inline[] }
  | { kind: 'code'; lang: string | null; text: string }
  | { kind: 'list'; ordered: boolean; start: number; items: Inline[][] }
  | { kind: 'rule' }
  | { kind: 'table'; text: string }

// ---------------------------------------------------------------------------
// 行内

type Token =
  | { type: 'text'; text: string }
  | { type: 'code'; text: string }
  | {
      type: 'delim'
      char: '*' | '_'
      count: number
      canOpen: boolean
      canClose: boolean
      opens: Array<'b' | 'i'>
      closes: Array<'b' | 'i'>
    }

const PUNCT = /[!-/:-@[-`{-~]/
const SPACE = /\s/
const WORD = /[\p{L}\p{N}]/u

function tokenize(src: string, optimistic: boolean): Token[] {
  const out: Token[] = []
  let buf = ''
  const flush = () => {
    if (buf) out.push({ type: 'text', text: buf })
    buf = ''
  }
  const n = src.length
  let i = 0
  while (i < n) {
    const c = src[i] as string
    if (c === '\\' && i + 1 < n && PUNCT.test(src[i + 1] as string)) {
      buf += src[i + 1]
      i += 2
      continue
    }
    if (c === '`') {
      let run = 1
      while (src[i + run] === '`') run++
      const fence = '`'.repeat(run)
      let close = src.indexOf(fence, i + run)
      // 找到的必须恰好是同样长度的反引号串
      while (close >= 0 && (src[close + run] === '`' || src[close - 1] === '`')) {
        close = src.indexOf(fence, close + 1)
      }
      if (close >= 0) {
        flush()
        out.push({ type: 'code', text: src.slice(i + run, close) })
        i = close + run
      } else if (optimistic) {
        flush()
        out.push({ type: 'code', text: src.slice(i + run) })
        i = n
      } else {
        buf += fence
        i += run
      }
      continue
    }
    if (c === '*' || c === '_') {
      let run = 1
      while (src[i + run] === c) run++
      const prev = i > 0 ? (src[i - 1] as string) : ''
      const next = i + run < n ? (src[i + run] as string) : ''
      let canOpen = next !== '' && !SPACE.test(next)
      let canClose = prev !== '' && !SPACE.test(prev)
      if (c === '_' && WORD.test(prev) && WORD.test(next)) {
        canOpen = false
        canClose = false
      }
      if (!canOpen && !canClose) {
        buf += c.repeat(run)
      } else {
        flush()
        out.push({ type: 'delim', char: c, count: run, canOpen, canClose, opens: [], closes: [] })
      }
      i += run
      continue
    }
    buf += c
    i++
  }
  flush()
  return out
}

/** 定界符匹配（栈），未匹配的在 optimistic 时视为开到块尾，否则原样显示 */
function match(tokens: Token[], optimistic: boolean): void {
  const stack: number[] = []
  for (let k = 0; k < tokens.length; k++) {
    const tok = tokens[k]
    if (!tok || tok.type !== 'delim') continue
    if (tok.canClose) {
      let j = stack.length - 1
      while (tok.count > 0 && j >= 0) {
        const op = tokens[stack[j] as number]
        if (op && op.type === 'delim' && op.char === tok.char && op.count > 0) {
          const use = op.count >= 2 && tok.count >= 2 ? 2 : 1
          const style = use === 2 ? 'b' : 'i'
          op.count -= use
          tok.count -= use
          op.opens.push(style)
          tok.closes.push(style)
          // 匹配到的开定界符之上的都不再参与匹配
          stack.length = op.count > 0 ? j + 1 : j
          j = stack.length - 1
        } else {
          j--
        }
      }
    }
    if (tok.count > 0 && tok.canOpen) stack.push(k)
  }
  if (!optimistic) return
  for (const k of stack) {
    const op = tokens[k]
    if (!op || op.type !== 'delim') continue
    while (op.count > 0) {
      const use = op.count >= 2 ? 2 : 1
      op.opens.push(use === 2 ? 'b' : 'i')
      op.count -= use
    }
  }
}

export function parseInline(src: string, optimistic = false): Inline[] {
  const tokens = tokenize(src, optimistic)
  match(tokens, optimistic)
  const runs: Inline[] = []
  let bold = 0
  let italic = 0
  const push = (text: string, code: boolean) => {
    if (!text) return
    const last = runs[runs.length - 1]
    const b = bold > 0
    const it = italic > 0
    if (last && last.bold === b && last.italic === it && last.code === code) last.text += text
    else runs.push({ text, bold: b, italic: it, code })
  }
  for (const tok of tokens) {
    if (tok.type === 'text') push(tok.text, false)
    else if (tok.type === 'code') push(tok.text, true)
    else {
      for (const s of tok.closes) {
        if (s === 'b') bold--
        else italic--
      }
      if (tok.count > 0) push(tok.char.repeat(tok.count), false)
      for (const s of tok.opens) {
        if (s === 'b') bold++
        else italic++
      }
    }
  }
  return runs
}

// ---------------------------------------------------------------------------
// 块级

const FENCE = /^ {0,3}(`{3,}|~{3,})\s*([\w+#.-]*)/
const ATX = /^ {0,3}(#{1,6})(?:\s+(.*?))?\s*#*\s*$/
const RULE = /^ {0,3}(?:(?:\*\s*){3,}|(?:-\s*){3,}|(?:_\s*){3,})$/
const SETEXT_1 = /^ {0,3}=+\s*$/
const SETEXT_2 = /^ {0,3}-+\s*$/
const BULLET = /^ {0,3}[-*+]\s+(.*)$/
const ORDERED = /^ {0,3}(\d{1,9})[.)]\s+(.*)$/
const TABLE = /^ {0,3}\|.*\|/

type Open =
  | { kind: 'paragraph'; lines: string[] }
  | { kind: 'list'; ordered: boolean; start: number; items: string[]; blank: boolean }
  | { kind: 'code'; fence: string; lang: string | null; lines: string[] }
  | { kind: 'table'; lines: string[] }

interface RawBlock {
  block: Open | { kind: 'heading'; level: number; text: string } | { kind: 'rule' }
}

function listItem(line: string): { ordered: boolean; start: number; text: string } | null {
  const b = BULLET.exec(line)
  if (b) return { ordered: false, start: 1, text: b[1] ?? '' }
  const o = ORDERED.exec(line)
  if (o) return { ordered: true, start: Number(o[1]), text: o[2] ?? '' }
  return null
}

/** 把源码切成原始块（行内尚未解析） */
function splitBlocks(src: string): RawBlock[] {
  const out: RawBlock[] = []
  let open: Open | null = null
  const close = () => {
    if (open) out.push({ block: open })
    open = null
  }
  const lines = src.split('\n')
  for (const line of lines) {
    // 围栏代码块内部
    if (open?.kind === 'code') {
      const f = FENCE.exec(line)
      const marker = f?.[1] ?? ''
      // 闭合围栏：同一种字符、不短于开围栏、后面没有信息串
      if (
        marker[0] === open.fence[0] &&
        marker.length >= open.fence.length &&
        (f?.[2] ?? '') === '' &&
        line.trim() === marker
      ) {
        close()
      } else {
        open.lines.push(line)
      }
      continue
    }
    const blank = line.trim() === ''
    if (blank) {
      if (open?.kind === 'list') open.blank = true
      else close()
      continue
    }
    const fence = FENCE.exec(line)
    if (fence) {
      close()
      open = { kind: 'code', fence: fence[1] ?? '```', lang: fence[2] || null, lines: [] }
      continue
    }
    if (open?.kind === 'paragraph' && SETEXT_1.test(line)) {
      out.push({ block: { kind: 'heading', level: 1, text: open.lines.join('\n') } })
      open = null
      continue
    }
    if (open?.kind === 'paragraph' && SETEXT_2.test(line)) {
      out.push({ block: { kind: 'heading', level: 2, text: open.lines.join('\n') } })
      open = null
      continue
    }
    const atx = ATX.exec(line)
    if (atx) {
      close()
      out.push({ block: { kind: 'heading', level: (atx[1] ?? '#').length, text: atx[2] ?? '' } })
      continue
    }
    if (RULE.test(line)) {
      close()
      out.push({ block: { kind: 'rule' } })
      continue
    }
    const item = listItem(line)
    if (item) {
      if (open?.kind === 'list' && open.ordered === item.ordered) {
        open.items.push(item.text)
        open.blank = false
      } else {
        close()
        open = {
          kind: 'list',
          ordered: item.ordered,
          start: item.start,
          items: [item.text],
          blank: false,
        }
      }
      continue
    }
    if (TABLE.test(line)) {
      if (open?.kind !== 'table') {
        close()
        open = { kind: 'table', lines: [] }
      }
      open.lines.push(line.trim())
      continue
    }
    // 普通文字行：接在当前段落 / 列表项后面
    if (open?.kind === 'list') {
      if (open.blank) {
        // 列表之后空行再出现普通文字：列表结束，开始新段落
        close()
        open = { kind: 'paragraph', lines: [line.trim()] }
      } else {
        const last = open.items.length - 1
        open.items[last] = `${open.items[last] ?? ''}\n${line.trim()}`
      }
      continue
    }
    if (open?.kind === 'paragraph') {
      open.lines.push(line.trim())
      continue
    }
    close()
    open = { kind: 'paragraph', lines: [line.trim()] }
  }
  close()
  return out
}

/**
 * 解析为块。`optimistic`：最后一块的行内按乐观规则解析（流式中）。
 */
export function parseMdLite(src: string, optimistic = false): MdBlock[] {
  const raw = splitBlocks(src)
  return raw.map(({ block: b }, idx): MdBlock => {
    const last = optimistic && idx === raw.length - 1
    switch (b.kind) {
      case 'paragraph':
        return { kind: 'paragraph', inlines: parseInline(b.lines.join('\n'), last) }
      case 'heading':
        return { kind: 'heading', level: b.level, inlines: parseInline(b.text, last) }
      case 'code':
        return { kind: 'code', lang: b.lang, text: b.lines.join('\n') }
      case 'list':
        return {
          kind: 'list',
          ordered: b.ordered,
          start: b.start,
          items: b.items.map((t, i) => parseInline(t, last && i === b.items.length - 1)),
        }
      case 'table':
        return { kind: 'table', text: b.lines.join('\n') }
      case 'rule':
        return { kind: 'rule' }
    }
  })
}

/** 两个块是否完全相同（渲染器用来决定能否复用 DOM） */
export function sameBlock(a: MdBlock, b: MdBlock): boolean {
  return JSON.stringify(a) === JSON.stringify(b)
}

/** 块的纯文本长度（渲染器决定哪些字是新增的） */
export function inlineLength(runs: readonly Inline[]): number {
  let n = 0
  for (const r of runs) n += r.text.length
  return n
}
