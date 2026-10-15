/**
 * URL 的折行点（WebView2 在 URL 路径段中间断行）：插入位置、前缀稳定、复制剔除；plain 写入器与 mdlite
 * 渲染器都插、代码不插。
 */
import { describe, expect, it } from 'vitest'
import { MdRenderer, renderBlocks } from '../../markdown/render'
import { IncrementalMdLite } from '../../markdown/incremental'
import { URL_BREAK, UrlBreaker, breakUrls, stripUrlBreaks } from '../urlBreaks'
import { StreamWriter } from '../writer'

const Z = URL_BREAK
const URL = 'https://github.com/yzlltyyh/wisp/blob/main/apps/desktop/src/x.tsx?plain=1&a=b#L10'

describe('breakUrls', () => {
  it('在 / ? & # = 之后插零宽空格；scheme 的 // 不插；普通文字不动', () => {
    expect(breakUrls(URL)).toBe(
      `https://github.com/${Z}yzlltyyh/${Z}wisp/${Z}blob/${Z}main/${Z}apps/${Z}desktop/${Z}src/${Z}x.tsx?${Z}plain=${Z}1&${Z}a=${Z}b#${Z}L10`,
    )
    expect(breakUrls('a/b and c=d, 1/2')).toBe('a/b and c=d, 1/2')
    expect(breakUrls('见www.example.com/a/b')).toBe(`见www.example.com/${Z}a/${Z}b`)
    expect(breakUrls('打开 http://localhost:5173/settings')).toBe(
      `打开 http://localhost:5173/${Z}settings`,
    )
    expect(stripUrlBreaks(breakUrls(URL))).toBe(URL)
  })

  it('前缀稳定：任意切分喂给 UrlBreaker 的结果都与一次性相同，且每个前缀的结果都是全文结果的前缀', () => {
    const text = `前文 ${URL} 后文 https://a.b/c?d=e`
    const whole = breakUrls(text)
    for (let cut = 0; cut <= text.length; cut++) {
      const b = new UrlBreaker()
      expect(b.feed(text.slice(0, cut)) + b.feed(text.slice(cut))).toBe(whole)
      expect(whole.startsWith(breakUrls(text.slice(0, cut)))).toBe(true)
    }
  })
})

describe('写入器与渲染器', () => {
  it('plain：跨 append 的 URL 照样插；text 返回原文', () => {
    const el = document.createElement('div')
    const w = new StreamWriter(el, { format: 'plain', caret: false })
    w.append('see https://github.com/yzll')
    w.append('tyyh/wisp')
    w.end()
    expect(el.textContent).toBe(`see https://github.com/${Z}yzlltyyh/${Z}wisp`)
    expect(w.text).toBe('see https://github.com/yzlltyyh/wisp')
    w.destroy()
  })

  it('mdlite：行内文字插、行内代码与代码块不插；增量追加与一次性渲染一致', () => {
    const src = `打开 ${URL} 看 \`a/b?c=d\`\n\n\`\`\`\nhttps://x.y/z\n\`\`\``
    const once = document.createElement('div')
    const md = new IncrementalMdLite()
    md.append(src)
    renderBlocks([...md.takeFresh(), ...md.tail(false)], once)
    expect(once.querySelector('p')?.textContent).toBe(`打开 ${breakUrls(URL)} 看 a/b?c=d`)
    expect(once.querySelector('pre')?.textContent).toBe('https://x.y/z')
    // 逐字流式写入：最终 DOM 文字与一次性渲染相同
    const live = document.createElement('div')
    const inc = new IncrementalMdLite()
    const r = new MdRenderer(live, null)
    for (const ch of src) {
      inc.append(ch)
      r.update(inc.takeFresh(), inc.tail(true))
    }
    r.update(inc.takeFresh(), inc.tail(false))
    expect(live.textContent).toBe(once.textContent)
  })
})
