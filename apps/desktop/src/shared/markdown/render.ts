/**
 * Markdown-lite 的 DOM 渲染。只用 createElement / createTextNode / textContent，**禁止 innerHTML**。
 *
 * - `renderBlocks`：一次性渲染（静态内容、单测的对照组）；
 * - `MdRenderer`：增量渲染。已提交的块渲染一次后不再变化；尾块每帧对比上一帧的结构做最小更新：
 *   同一段文字变长时只把新增部分包进 chunk span 追加（淡入只用于尾块里新增的文字，已显示的文字不会重播动画）；
 *   结构变化（乐观的 `**` 被确认、段落变成 setext 标题……）时重建该块，原先已显示过的字数不做动画。
 * - 末尾光标：最后一个文本块（列表取最后一项）带 `data-caret`，由 CSS `::after` 绘制。
 */
import type { ChunkTracker } from '../stream/chunks'
import { breakUrls } from '../stream/urlBreaks'
import { inlineLength, sameBlock, type Inline, type MdBlock } from './mdlite'

type BlockEl = HTMLElement

function runTag(run: Inline): HTMLElement {
  if (run.code) return document.createElement('code')
  if (run.bold) {
    const el = document.createElement('strong')
    if (run.italic) el.className = 'md-i'
    return el
  }
  if (run.italic) return document.createElement('em')
  return document.createElement('span')
}

const sameFlags = (a: Inline, b: Inline) =>
  a.bold === b.bold && a.italic === b.italic && a.code === b.code

/**
 * 往元素里写一段文字：前 `still` 个字直接显示，其余的作为新到达的文字（chunk span，可能淡入）。
 * 返回消耗掉的 still 字数。
 */
function fill(el: HTMLElement, text: string, still: number, chunks: ChunkTracker | null): number {
  if (!chunks || still >= text.length) {
    el.append(document.createTextNode(text))
    return Math.min(still, text.length)
  }
  if (still > 0) el.append(document.createTextNode(text.slice(0, still)))
  el.append(chunks.create(text.slice(still)))
  return still
}

/**
 * 行内 run 显示用的文字：URL 的分隔符之后插入零宽空格（urlBreaks.ts，前缀稳定）；行内代码原样（代码不折行点也不插）
 */
function shownText(run: Inline): string {
  return run.code ? run.text : breakUrls(run.text)
}

/**
 * 往 run 元素里写文字：`still` 按原文字数计（与 textLength / 已显示字数同一口径），换算成显示文字（含折行点）里的位置。
 * 返回消耗掉的原文字数
 */
function fillRun(el: HTMLElement, run: Inline, still: number, chunks: ChunkTracker | null): number {
  const shown = shownText(run)
  const raw = Math.min(still, run.text.length)
  const at =
    raw >= run.text.length
      ? shown.length
      : shown === run.text
        ? raw
        : shownText({ ...run, text: run.text.slice(0, raw) }).length
  fill(el, shown, at, chunks)
  return raw
}

function blockTag(b: MdBlock): HTMLElement {
  switch (b.kind) {
    case 'paragraph':
      return document.createElement('p')
    case 'heading':
      return document.createElement(`h${Math.min(6, Math.max(1, b.level))}`)
    case 'code': {
      const pre = document.createElement('pre')
      if (b.lang) pre.dataset.lang = b.lang
      pre.append(document.createElement('code'))
      return pre
    }
    case 'list': {
      const el = document.createElement(b.ordered ? 'ol' : 'ul')
      if (b.ordered && b.start !== 1) el.setAttribute('start', String(b.start))
      return el
    }
    case 'rule':
      return document.createElement('hr')
    case 'table': {
      const pre = document.createElement('pre')
      pre.className = 'md-table'
      return pre
    }
  }
}

/** 创建块元素；`still` 个字不做动画（重建时沿用已显示的字数），其余交给 chunks */
function createBlock(b: MdBlock, still: number, chunks: ChunkTracker | null): BlockEl {
  const el = blockTag(b)
  let budget = still
  const runs = (host: HTMLElement, inlines: readonly Inline[]) => {
    for (const r of inlines) {
      const run = runTag(r)
      budget -= fillRun(run, r, budget, chunks)
      host.append(run)
    }
  }
  switch (b.kind) {
    case 'paragraph':
    case 'heading':
      runs(el, b.inlines)
      break
    case 'list':
      for (const item of b.items) {
        const li = document.createElement('li')
        runs(li, item)
        el.append(li)
      }
      break
    case 'code':
      fill(el.firstElementChild as HTMLElement, b.text, budget, chunks)
      break
    case 'table':
      fill(el, b.text, budget, chunks)
      break
    case 'rule':
      break
  }
  return el
}

function textLength(b: MdBlock): number {
  switch (b.kind) {
    case 'paragraph':
    case 'heading':
      return inlineLength(b.inlines)
    case 'list':
      return b.items.reduce((n, it) => n + inlineLength(it), 0)
    case 'code':
    case 'table':
      return b.text.length
    case 'rule':
      return 0
  }
}

/** 一次性渲染（静态） */
export function renderBlocks(blocks: readonly MdBlock[], container: HTMLElement): void {
  container.textContent = ''
  for (const b of blocks) container.append(createBlock(b, Infinity, null))
}

/** 原地更新一串行内 run（段落、标题、列表项） */
function patchRuns(
  host: HTMLElement,
  prev: readonly Inline[],
  next: readonly Inline[],
  chunks: ChunkTracker | null,
): void {
  const els = Array.from(host.children) as HTMLElement[]
  let k = 0
  while (k < prev.length && k < next.length) {
    const a = prev[k] as Inline
    const b = next[k] as Inline
    if (sameFlags(a, b) && a.text === b.text) k++
    else break
  }
  if (k < prev.length) {
    const a = prev[k] as Inline
    const b = next[k]
    if (b && sameFlags(a, b) && b.text.startsWith(a.text) && k === prev.length - 1) {
      // 最常见：最后一个 run 变长，只追加新增部分（折行点前缀稳定：旧的显示文字一定是新的前缀）
      const delta = shownText(b).slice(shownText(a).length)
      if (delta) els[k]?.append(chunks ? chunks.create(delta) : document.createTextNode(delta))
      k++
    } else {
      // 结构变化：从分歧点重建；原先已显示的字数不做动画
      let still = 0
      for (let i = k; i < prev.length; i++) {
        still += (prev[i] as Inline).text.length
        els[i]?.remove()
      }
      for (let i = k; i < next.length; i++) {
        const r = next[i] as Inline
        const el = runTag(r)
        still -= fillRun(el, r, still, chunks)
        host.append(el)
      }
      return
    }
  }
  for (let i = k; i < next.length; i++) {
    const r = next[i] as Inline
    const el = runTag(r)
    fillRun(el, r, 0, chunks)
    host.append(el)
  }
}

function compatible(a: MdBlock, b: MdBlock): boolean {
  if (a.kind !== b.kind) return false
  if (a.kind === 'heading' && b.kind === 'heading') return a.level === b.level
  if (a.kind === 'list' && b.kind === 'list') return a.ordered === b.ordered && a.start === b.start
  if (a.kind === 'code' && b.kind === 'code') return a.lang === b.lang
  return true
}

/** 原地更新一个块；不能原地更新时返回 false */
function patchBlock(
  el: BlockEl,
  prev: MdBlock,
  next: MdBlock,
  chunks: ChunkTracker | null,
): boolean {
  if (!compatible(prev, next)) return false
  switch (next.kind) {
    case 'paragraph':
    case 'heading':
      patchRuns(el, (prev as typeof next).inlines, next.inlines, chunks)
      return true
    case 'list': {
      const p = prev as typeof next
      const lis = Array.from(el.children) as HTMLElement[]
      for (let i = 0; i < next.items.length; i++) {
        const item = next.items[i] as Inline[]
        const li = lis[i]
        if (li) patchRuns(li, p.items[i] ?? [], item, chunks)
        else {
          const nli = document.createElement('li')
          patchRuns(nli, [], item, chunks)
          el.append(nli)
        }
      }
      for (let i = next.items.length; i < lis.length; i++) lis[i]?.remove()
      return true
    }
    case 'code':
    case 'table': {
      const host = (next.kind === 'code' ? el.firstElementChild : el) as HTMLElement
      const old = (prev as typeof next).text
      if (next.text.startsWith(old)) {
        const delta = next.text.slice(old.length)
        if (delta) host.append(chunks ? chunks.create(delta) : document.createTextNode(delta))
      } else {
        host.textContent = next.text
      }
      return true
    }
    case 'rule':
      return true
  }
}

/** 光标挂在哪个元素上：列表取最后一项，其余取块本身 */
function caretHost(el: Element | null): HTMLElement | null {
  if (!el || el.tagName === 'HR') return null
  if (el.tagName === 'UL' || el.tagName === 'OL')
    return (el.lastElementChild as HTMLElement) ?? null
  if (el.tagName === 'PRE')
    return (el.firstElementChild as HTMLElement | null) ?? (el as HTMLElement)
  return el as HTMLElement
}

export class MdRenderer {
  private tail: Array<{ block: MdBlock; el: BlockEl }> = []
  private caret: HTMLElement | null = null

  constructor(
    private readonly root: HTMLElement,
    private readonly chunks: ChunkTracker | null,
  ) {}

  /**
   * 一帧的更新：`fresh` 是本帧新提交的块（按顺序，必然排在尾块之前），`tail` 是当前尾部的全部块。
   * `animate` 为 false 时新文字直接显示（重置后写入初始内容）。
   */
  update(fresh: readonly MdBlock[], tail: readonly MdBlock[], animate = true): void {
    const chunks = animate ? this.chunks : null
    for (const f of fresh) {
      const r = this.tail.shift()
      if (r && sameBlock(r.block, f)) continue
      if (r) {
        // 提交时的最终解析与乐观渲染不同（例如未闭合的 ** 最后原样显示）：重建，不做动画
        r.el.replaceWith(createBlock(f, Infinity, null))
      } else {
        const el = createBlock(f, 0, chunks)
        const first = this.tail[0]?.el ?? null
        this.root.insertBefore(el, first)
      }
    }
    for (let j = 0; j < tail.length; j++) {
      const next = tail[j] as MdBlock
      const old = this.tail[j]
      if (!old) {
        const el = createBlock(next, 0, chunks)
        this.root.append(el)
        this.tail.push({ block: next, el })
        continue
      }
      if (sameBlock(old.block, next)) continue
      if (!patchBlock(old.el, old.block, next, chunks)) {
        const el = createBlock(next, textLength(old.block), chunks)
        old.el.replaceWith(el)
        old.el = el
      }
      old.block = next
    }
    for (let j = tail.length; j < this.tail.length; j++) this.tail[j]?.el.remove()
    if (this.tail.length > tail.length) this.tail.length = tail.length
  }

  /** 把末尾光标挂到最后一个文本块上；`state` 为 null 时移除 */
  setCaret(state: 'on' | 'ending' | null): void {
    const host = state ? caretHost(this.root.lastElementChild) : null
    if (this.caret && this.caret !== host) delete this.caret.dataset.caret
    this.caret = host
    if (host && state) host.dataset.caret = state === 'on' ? 'true' : 'ending'
  }

  /** 清空 */
  reset(): void {
    this.root.textContent = ''
    this.tail = []
    this.caret = null
  }
}
