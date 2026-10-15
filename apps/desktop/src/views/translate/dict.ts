/**
 * 词典式输出的排版整理。按 wisp-prompts 的词典式格式（`dictionary.system.txt`）：
 *
 * ```
 * **serendipity** /ˌser.ənˈdɪp.ə.ti/ n.
 * 1. 机缘巧合 — 意外发现美好事物的运气
 * 例：It was pure serendipity that we met. → 我们的相遇纯属机缘巧合。
 * ```
 *
 * 视觉稿 design.translate-dict 的结构是：词头 + 音标、词性小胶囊；例句**原句 / 译句分两行**、原句里的词头加粗、左侧竖线。
 * 提示词的格式与视觉稿不完全一致（词性在首行而不是每个义项前），这里在不改提示词的前提下把能对齐的部分对齐：
 * - 首行音标之后的词性（`n.` / `adj.`……）包成行内代码，由 translate.css 画成小胶囊；
 * - 「例：」行前补一个空行（成为独立段落，否则 Markdown 会把它当作上一个义项的延续），去掉「例：」标签（左侧竖线
 *   已经表明是例句），在「→」处换行；原句里出现的词头加粗——英文要求完整的词头出现在词首（覆盖词头加后缀的形式，
 *   例如 token → tokens、run → runs；词干本身有变化的形式不加粗，例如 serendipity → serendipitous、run → ran），
 *   中文不要求词边界。
 *
 * 增量处理（流式中逐块调用 `push`）：只有「可能是词头的开头」与「行首的『例』」需要暂扣几个字符，`end()` 时吐出。
 * 复制、固定用的是模型的原文（run.ts 另存），这里的标记只用于显示。
 */

/** 词性里的字符（`n.`、`adj.`、`vt.`、`phr. v.`） */
const POS_CHAR = /[\p{L}.]/u
/** 单词字符（判断词头的词首边界） */
const WORD_CHAR = /[\p{L}\p{N}'’_-]/u
const CJK = /[぀-ヿ㐀-鿿가-힯]/u

type Head = 'start' | 'word' | 'after' | 'ipa' | 'pos' | 'done'

export class DictFormat {
  /** 输出里最近的两个字符（判断行首、空行） */
  private tail = '\n\n'
  /** 第几行（0 = 词头行） */
  private line = 0
  /** 词头行的状态：`**词头**` → 音标 → 词性 */
  private head: Head = 'start'
  private headword = ''
  private stars = 0
  private posOpen = false
  /** 行首暂扣的「例」（还不知道后面是不是「：」） */
  private lineHold = ''
  private atLineStart = true
  /** 当前行是例句：原句部分（「→」之前）与译句部分 */
  private example: 'none' | 'source' | 'target' = 'none'
  /** 例句原句里暂扣的、可能是词头开头的字符 */
  private match = ''
  /** 上一个已处理的原句字符（词首边界） */
  private prevSource = ''

  /** 处理一块文字，返回要显示的部分（可能少于输入：暂扣的字符在后续块或 end() 时吐出） */
  push(chunk: string): string {
    let out = ''
    for (const ch of chunk) out += this.char(ch)
    return out
  }

  /** 流结束：吐出暂扣的字符，补上未闭合的词性标记 */
  end(): string {
    let out = this.flushMatch() + this.lineHold
    this.lineHold = ''
    if (this.posOpen) {
      out += '`'
      this.posOpen = false
    }
    return out
  }

  private emit(s: string): string {
    if (s) this.tail = (this.tail + s).slice(-2)
    return s
  }

  private char(ch: string): string {
    if (ch === '\r') return ''
    if (ch === '\n') return this.newline()
    if (this.line === 0) return this.headChar(ch)
    if (this.atLineStart) {
      if (this.lineHold) {
        const held = this.lineHold
        this.lineHold = ''
        this.atLineStart = false
        if (ch === '：' || ch === ':') {
          // 「例：」：独立段落、去掉标签
          this.example = 'source'
          this.prevSource = ''
          return this.tail.endsWith('\n\n') ? '' : this.emit('\n')
        }
        return this.emit(held) + this.bodyChar(ch)
      }
      if (ch === '例') {
        this.lineHold = ch
        return ''
      }
      if (ch !== ' ' && ch !== '\t') this.atLineStart = false
    }
    return this.bodyChar(ch)
  }

  private newline(): string {
    let out = this.flushMatch() + this.lineHold
    this.lineHold = ''
    if (this.line === 0) {
      if (this.posOpen) out += '`'
      this.posOpen = false
      this.head = 'done'
    }
    this.line++
    this.atLineStart = true
    this.example = 'none'
    return this.emit(out + '\n')
  }

  /** 词头行：`**词头**`、音标（两条斜线之间）、词性 */
  private headChar(ch: string): string {
    switch (this.head) {
      case 'start':
        if (ch === '*') {
          this.stars++
          if (this.stars === 2) this.head = 'word'
        } else if (ch !== ' ') this.head = 'done'
        return this.emit(ch)
      case 'word':
        if (ch === '*') this.head = 'after'
        else this.headword += ch
        return this.emit(ch)
      case 'after':
        if (ch === '/') this.head = 'ipa'
        else if (ch !== '*' && ch !== ' ') this.head = 'done'
        return this.emit(ch)
      case 'ipa':
        if (ch === '/') this.head = 'pos'
        return this.emit(ch)
      case 'pos': {
        if (POS_CHAR.test(ch)) {
          if (this.posOpen) return this.emit(ch)
          this.posOpen = true
          return this.emit('`' + ch)
        }
        if (this.posOpen) {
          this.posOpen = false
          return this.emit('`' + ch)
        }
        return this.emit(ch)
      }
      case 'done':
        return this.emit(ch)
    }
  }

  private bodyChar(ch: string): string {
    if (this.example === 'none') return this.emit(ch)
    if (this.example === 'target') return this.emit(ch)
    // 原句部分
    if (ch === '→') {
      this.example = 'target'
      return this.emit(this.flushMatch() + '\n')
    }
    return this.sourceChar(ch)
  }

  /** 例句原句：词头加粗（暂扣可能的开头，确定之后整体吐出） */
  private sourceChar(ch: string): string {
    const word = this.headword.trim()
    if (word.length < 2) return this.plain(ch)
    if (this.match) {
      const next = this.match + ch
      if (word.toLocaleLowerCase().startsWith(next.toLocaleLowerCase())) {
        if (next.length < word.length) {
          this.match = next
          return ''
        }
        this.match = ''
        this.prevSource = ch
        return this.emit(`**${next}**`)
      }
      const held = this.flushMatch()
      return held + this.sourceChar(ch)
    }
    const boundary = CJK.test(word) || !WORD_CHAR.test(this.prevSource)
    if (
      boundary &&
      this.prevSource !== '*' &&
      ch.toLocaleLowerCase() === word[0]?.toLocaleLowerCase()
    ) {
      this.match = ch
      return ''
    }
    return this.plain(ch)
  }

  private plain(ch: string): string {
    this.prevSource = ch
    return this.emit(ch)
  }

  private flushMatch(): string {
    const held = this.match
    this.match = ''
    if (held) this.prevSource = held.slice(-1)
    return this.emit(held)
  }
}

/** 一次性整理整段词典式输出（静态结果） */
export function formatDict(text: string): string {
  const f = new DictFormat()
  return f.push(text) + f.end()
}
