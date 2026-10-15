/**
 * Markdown-lite 的增量解析：维护 `committed` 边界，已提交的块只解析一次，之后每帧只重新解析
 * 边界之后的尾部（复杂度 O(tail)）。
 *
 * 提交点（都在完整的行上判断）：
 * - 代码围栏之外的空行之后——除非空行之前是列表（空行之后可能还有同一列表的项，见 mdlite 的列表规则），
 *   这时把提交点挂起，等下一个非空行出现：不是列表项就在空行处提交，是列表项就取消；
 * - 代码围栏闭合行之后。
 * 这些位置把源码切开后分别解析、再拼起来，与整段一次解析的结果完全一致（单测：逐字符输入与一次性输入的 DOM 相同）。
 */
import { parseMdLite, type MdBlock } from './mdlite'

const FENCE = /^ {0,3}(`{3,}|~{3,})\s*([\w+#.-]*)/
const LIST_ITEM = /^ {0,3}(?:[-*+]|\d{1,9}[.)])\s+/
const ATX = /^ {0,3}#{1,6}(?:\s|$)/
const TABLE = /^ {0,3}\|.*\|/
const RULE = /^ {0,3}(?:(?:\*\s*){3,}|(?:-\s*){3,}|(?:_\s*){3,})$/

export class IncrementalMdLite {
  private source = ''
  /** 已提交部分的末尾（source 下标） */
  private committedEnd = 0
  /** 下一次扫描完整行的起点 */
  private scanPos = 0
  /** 在围栏代码块内：开围栏的标记 */
  private fence: string | null = null
  /** 最近的非空行属于列表 */
  private inList = false
  /** 列表之后的空行：挂起的提交点 */
  private pending: number | null = null
  private readonly committedBlocks: MdBlock[] = []
  private fresh: MdBlock[] = []

  get text(): string {
    return this.source
  }

  append(text: string): void {
    if (!text) return
    this.source += text
    this.scan()
  }

  /** 全部已提交的块 */
  committed(): readonly MdBlock[] {
    return this.committedBlocks
  }

  /** 自上次调用以来新提交的块（渲染器 append 用） */
  takeFresh(): MdBlock[] {
    const out = this.fresh
    this.fresh = []
    return out
  }

  /** 尾部（边界之后）的块；`optimistic` 为 true 时最后一块按乐观规则解析 */
  tail(optimistic: boolean): MdBlock[] {
    return parseMdLite(this.source.slice(this.committedEnd), optimistic)
  }

  /** 尾部源码长度（调试 / 性能观测） */
  get tailLength(): number {
    return this.source.length - this.committedEnd
  }

  private commit(pos: number): void {
    if (pos <= this.committedEnd) return
    const blocks = parseMdLite(this.source.slice(this.committedEnd, pos), false)
    this.committedEnd = pos
    for (const b of blocks) {
      this.committedBlocks.push(b)
      this.fresh.push(b)
    }
  }

  private scan(): void {
    const src = this.source
    for (;;) {
      const nl = src.indexOf('\n', this.scanPos)
      if (nl < 0) return
      const line = src.slice(this.scanPos, nl)
      const end = nl + 1
      this.scanPos = end
      if (this.fence !== null) {
        const marker = FENCE.exec(line)?.[1] ?? ''
        if (
          marker[0] === this.fence[0] &&
          marker.length >= this.fence.length &&
          line.trim() === marker
        ) {
          this.fence = null
          this.inList = false
          this.commit(end)
        }
        continue
      }
      const blank = line.trim() === ''
      if (blank) {
        if (this.inList) this.pending = end
        else this.commit(end)
        continue
      }
      const item = LIST_ITEM.test(line) && !RULE.test(line)
      if (this.pending !== null) {
        const boundary = this.pending
        this.pending = null
        if (!item) {
          this.commit(boundary)
          this.inList = false
        }
      }
      const fence = FENCE.exec(line)
      if (fence) {
        this.fence = fence[1] ?? '```'
        this.inList = false
        continue
      }
      if (item) this.inList = true
      else if (ATX.test(line) || TABLE.test(line) || RULE.test(line)) this.inList = false
      // 其余普通文字行：紧跟列表项时是该项的续行（仍在列表里），否则是段落
    }
  }
}
