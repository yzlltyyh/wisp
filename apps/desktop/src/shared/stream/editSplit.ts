/**
 * 润色 / 校对输出的切分（TS 版；Rust `wisp_prompts::split_edit_output` 规则相同，
 * 两边共用 `crates/wisp-prompts/tests/fixtures/edit_split.json`）。
 *
 * 规则（逐条对应 Rust 实现）：先把 `\r\n` 规范为 `\n`；找最后一个 `\n---\n`，没有时输出以 `\n---` 结尾也算（改动为空）；
 * 分隔线之后 trim 的内容（可带 `**` 加粗）以「改动」或 Changes（大小写不敏感）开头才切分，否则整段都是正文；
 * 切分时正文去掉末尾空白，改动 trim，空则为 null。
 * 正文里自带的 `---`（Markdown 分隔线）后面不是「改动」，不会被误切。
 */
export interface EditSplit {
  body: string
  changes: string | null
}

const SEPARATOR = '\n---\n'
const DANGLING = '\n---'
/** 分隔线之后的内容以「改动」/ Changes 开头（允许 Markdown 加粗 / 下划线与前导空格） */
const LABEL = /^[*_ ]*(?:改动|changes)/i
const TRAILING_SPACE = /\s+$/u

export function splitEditOutput(input: string): EditSplit {
  // 先把 \r\n 规范为 \n（返回的正文与改动都是规范化之后的文本）
  const text = input.includes('\r\n') ? input.replaceAll('\r\n', '\n') : input
  let body: string
  let rest: string
  const at = text.lastIndexOf(SEPARATOR)
  if (at >= 0) {
    body = text.slice(0, at)
    rest = text.slice(at + SEPARATOR.length)
  } else {
    const trimmed = text.replace(TRAILING_SPACE, '')
    if (!trimmed.endsWith(DANGLING)) return { body: text, changes: null }
    body = trimmed.slice(0, -DANGLING.length)
    rest = ''
  }
  const changes = rest.trim()
  if (changes && !LABEL.test(changes)) return { body: text, changes: null }
  return { body: body.replace(TRAILING_SPACE, ''), changes: changes || null }
}

/**
 * 流式中是否已经可以切换为「正文 + 改动」显示：分隔线之后已经出现了「改动」标签（只切换一次）。
 * 结束时以 `splitEditOutput` 的结果为准。
 */
export function streamingSplit(text: string): EditSplit | null {
  const s = splitEditOutput(text)
  return s.changes !== null ? s : null
}

/** 「改动」标签只看分隔线之后第一个非空白字符起的这么多个字符（标签前只允许少量 `*` / `_` / 空格） */
const LABEL_WINDOW = 64
const WS = /\s/u

/**
 * 流式切分的增量检测（StreamView 的 edit_split）：每次 `push` 只扫描新到达的文字（加上与上次结尾相接的 4 个字符），
 * 不再对全文反复做 `splitEditOutput`——4k token 的润色输出里逐 token 全文扫描是 O(n²)。
 *
 * 维护规范化（`\r\n` → `\n`）之后的分隔线位置、分隔线之后的文字与其首尾非空白位置：
 * - `labeled`：最后一条分隔线之后的内容以「改动」/ Changes 开头（与 `streamingSplit(全文) !== null` 一致）；
 * - `version`：出现新的分隔线时递增（切分点变了，调用方据此用 `splitEditOutput` 全量重算一次）；
 * - `changes()`：分隔线之后去掉首尾空白的内容；同一 version 内只会在末尾增长（调用方据此只追加增量）；
 * - `body()`：分隔线之前的正文（去掉末尾空白），O(n)，只在切换时调用一次。
 * 全文只做拼接、从不在热路径上切片或下标访问（V8 会把拼接出的 rope 整个拍平）。流结束时仍以 `splitEditOutput(全文)` 为准。
 */
export class EditSplitStream {
  /** 规范化之后的全文（只在 body() 里切片） */
  private norm = ''
  private normLength = 0
  /** 全文最后 4 个字符（分隔线可能跨块） */
  private tail = ''
  /** 上一块以 `\r` 结尾：先扣住，看下一块是否以 `\n` 开头 */
  private heldCR = false
  /** 最后一条分隔线之后的文字 */
  private post = ''
  private postLength = 0
  /** 最后一条分隔线在全文里的起点（-1：还没有） */
  private sepAt = -1
  /** post 里第一个 / 最后一个非空白字符的位置（-1：还没有） */
  private first = -1
  private last = -1
  /** 标签判定：null 表示窗口还没收满、下一块可能改变结论 */
  private decided: boolean | null = null
  private label = false
  /** 切分点（最后一条分隔线）的版本 */
  version = 0

  get labeled(): boolean {
    return this.label
  }

  push(chunk: string): void {
    if (!chunk) return
    let text = (this.heldCR ? '\r' : '') + chunk
    this.heldCR = text.endsWith('\r')
    if (this.heldCR) text = text.slice(0, -1)
    if (text.includes('\r\n')) text = text.replaceAll('\r\n', '\n')
    if (!text) return
    const window = this.tail + text
    const base = this.normLength - this.tail.length
    this.norm += text
    this.normLength += text.length
    this.tail = window.slice(-(SEPARATOR.length - 1))
    const found = window.lastIndexOf(SEPARATOR)
    if (found >= 0 && base + found > this.sepAt) {
      this.sepAt = base + found
      this.post = window.slice(found + SEPARATOR.length)
      this.postLength = this.post.length
      this.first = -1
      this.last = -1
      this.decided = null
      this.version += 1
      this.scan(this.post, 0)
    } else if (this.sepAt >= 0) {
      const offset = this.postLength
      this.post += text
      this.postLength += text.length
      this.scan(text, offset)
    } else {
      return
    }
    this.label = this.decideLabel()
  }

  /** 分隔线之后去掉首尾空白的内容（没有分隔线或全是空白时为空串） */
  changes(): string {
    return this.first < 0 ? '' : this.post.slice(this.first, this.last + 1)
  }

  /** 分隔线之前的正文（去掉末尾空白）；没有分隔线时为全文 */
  body(): string {
    return this.sepAt < 0 ? this.norm : this.norm.slice(0, this.sepAt).replace(TRAILING_SPACE, '')
  }

  /** 新到达的一段（位于 post 的 offset 处）里的首尾非空白位置 */
  private scan(piece: string, offset: number): void {
    for (let i = piece.length - 1; i >= 0; i--) {
      if (!WS.test(piece[i] as string)) {
        this.last = offset + i
        break
      }
    }
    if (this.first >= 0) return
    for (let i = 0; i < piece.length; i++) {
      if (!WS.test(piece[i] as string)) {
        this.first = offset + i
        break
      }
    }
  }

  private decideLabel(): boolean {
    if (this.decided !== null) return this.decided
    if (this.first < 0) return false
    const window = this.post.slice(this.first, this.first + LABEL_WINDOW)
    const result = LABEL.test(window)
    // 标签是前缀匹配：匹配上了、或窗口已经收满，结论都不会再变，之后不再切片
    if (result || window.length >= LABEL_WINDOW) this.decided = result
    return result
  }
}
