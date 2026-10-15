/**
 * 增量渲染：逐字符输入与一次性输入渲染出的 DOM 一致；已提交的块只解析一次。
 */
import { describe, expect, it } from 'vitest'
import { ChunkTracker } from '../../stream/chunks'
import { IncrementalMdLite } from '../incremental'
import { parseMdLite } from '../mdlite'
import { MdRenderer, renderBlocks } from '../render'

const DOCS: Record<string, string> = {
  learn: `**译文**
它在后台安静待命，几乎不占用 CPU。

**词汇**
- **idle** /ˈaɪdl/ *v.* 空转、待机
- **wake** /weɪk/ *v.* 唤醒

- **stream** *v.* 逐段输出

**语法**
\`stays out of your way\` 是固定搭配。`,
  code: `看这段代码：

\`\`\`ts
const a = 1

const b = **not bold**
\`\`\`

结束。`,
  setext: `Heading
=======
Some *em* and **strong** text with \`code\`.
Sub
---
- a
1. b
2. c

| x | y |
|---|---|

***`,
  tricky: `a **b *c* d** e
snake_case and 5 * 3
\`unclosed code
**unclosed bold`,
  ordered: `3. three
4. four

5. five
plain after`,
}

function oneShot(text: string): string {
  const root = document.createElement('div')
  renderBlocks(parseMdLite(text), root)
  return root.innerHTML
}

/** 按给定切片增量写入；每片之后更新一次（相当于一帧） */
function incremental(text: string, sizes: (i: number) => number, animate: boolean): string {
  const root = document.createElement('div')
  const chunks = new ChunkTracker(() => animate)
  const md = new IncrementalMdLite()
  const renderer = new MdRenderer(root, chunks)
  let i = 0
  let frame = 0
  while (i < text.length) {
    const n = Math.max(1, sizes(frame++))
    md.append(text.slice(i, i + n))
    i += n
    renderer.update(md.takeFresh(), md.tail(true))
    renderer.setCaret('on')
  }
  renderer.update(md.takeFresh(), md.tail(false))
  chunks.mergeAll()
  renderer.setCaret(null)
  return root.innerHTML
}

describe('MdLite 增量渲染', () => {
  for (const [name, text] of Object.entries(DOCS)) {
    it(`${name}：逐字符输入与一次性输入的 DOM 一致`, () => {
      expect(incremental(text, () => 1, true)).toBe(oneShot(text))
    })
    it(`${name}：不规则分块（含跨行）与一次性输入一致`, () => {
      const pattern = [3, 1, 7, 2, 13, 5, 1, 1, 9]
      expect(incremental(text, (f) => pattern[f % pattern.length] ?? 1, true)).toBe(oneShot(text))
    })
    it(`${name}：减少动效（无 span）同样一致`, () => {
      expect(incremental(text, () => 2, false)).toBe(oneShot(text))
    })
  }

  it('已提交的块只解析一次：尾部始终只有最后一段', () => {
    const md = new IncrementalMdLite()
    const paras = Array.from({ length: 50 }, (_, i) => `第 ${i} 段文字。`).join('\n\n')
    for (const ch of paras) md.append(ch)
    expect(md.committed().length).toBe(49)
    expect(md.tailLength).toBeLessThan(20)
  })

  it('列表之后的空行挂起提交，直到下一行不是列表项', () => {
    const md = new IncrementalMdLite()
    md.append('- a\n\n')
    expect(md.committed()).toHaveLength(0)
    md.append('- b\n\n')
    expect(md.committed()).toHaveLength(0)
    md.append('text\n')
    expect(md.committed()).toMatchObject([{ kind: 'list' }])
  })

  it('流式中乐观渲染的块在最终解析不同时重建，不残留样式', () => {
    const root = document.createElement('div')
    const md = new IncrementalMdLite()
    const r = new MdRenderer(root, null)
    md.append('a **b')
    r.update(md.takeFresh(), md.tail(true))
    expect(root.querySelector('strong')?.textContent).toBe('b')
    r.update(md.takeFresh(), md.tail(false))
    expect(root.querySelector('strong')).toBeNull()
    expect(root.textContent).toBe('a **b')
  })

  it('光标挂在最后一个文本块（列表取最后一项）', () => {
    const root = document.createElement('div')
    const md = new IncrementalMdLite()
    const r = new MdRenderer(root, null)
    md.append('para\n\n- x\n- y')
    r.update(md.takeFresh(), md.tail(true))
    r.setCaret('on')
    expect(root.querySelector('[data-caret]')?.textContent).toBe('y')
    r.setCaret(null)
    expect(root.querySelector('[data-caret]')).toBeNull()
  })

  it('不使用 innerHTML 写入：HTML 标记原样显示为文字', () => {
    const html = '<img src=x onerror=alert(1)> **<b>x</b>**'
    const root = document.createElement('div')
    renderBlocks(parseMdLite(html), root)
    expect(root.querySelector('img')).toBeNull()
    expect(root.textContent).toContain('<img src=x onerror=alert(1)>')
  })
})
