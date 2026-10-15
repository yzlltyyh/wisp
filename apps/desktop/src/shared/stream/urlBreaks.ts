/**
 * 流式正文里 URL 的折行点：Chromium 内核（Windows 的 WebView2）配合 `overflow-wrap: anywhere` 会在 URL
 * 的路径段中间断行（「…/apps/d｜esktop/…」），WebKit 则在 `/` 处断。在 URL 的 `/ ? & # =` 之后插入零宽空格（U+200B），
 * 两种引擎都优先在分隔符处折行；`anywhere` 只作为超长单段的兜底。
 *
 * - 用零宽空格而不是 `<wbr>`：流式写入把新文字放进动画 span、之后按 textContent 合并成文本节点（chunks.ts），元素会在
 *   合并时丢掉，字符不会；
 * - **前缀稳定**：某个位置插不插只取决于它前面的文字，`breakUrls(a)` 一定是 `breakUrls(a + b)` 的前缀——增量渲染
 *   （writer 的 plain、render.ts 的行内 run）可以只追加新增部分；
 * - URL 的认定：从空白或非 ASCII 字符之后开始的一段 ASCII 文字，以 `http://`、`https://` 或 `www.` 开头；`scheme://`
 *   里的两个斜杠之后不插；
 * - 复制时剔除（`stripUrlBreaks`）：StreamView 的 copy 事件、视图自己的 ⌘C（读选区）都经过它，复制出去的文字与原文一致。
 */

export const URL_BREAK = '​'
const SEPARATORS = '/?&#='
const URL_HEAD = /^(?:https?:\/\/|www\.)/i
/** 判断 URL 只需要看一段文字的开头这么多个字符（`https://`） */
const HEAD_CHARS = 8

/** 增量版本：按顺序喂入文字，返回插好折行点的文字；状态跨调用保留（流式写入用） */
export class UrlBreaker {
  private head = ''
  private prev1 = ''
  private prev2 = ''

  feed(text: string): string {
    let out = ''
    for (const ch of text) {
      const code = ch.codePointAt(0) ?? 0
      // 空白、控制字符与非 ASCII（中文、全角标点）都是一段的边界：「见https://…」里的 URL 从 h 开始
      if (code <= 0x20 || code > 0x7e) {
        this.head = ''
        this.prev1 = ''
        this.prev2 = ''
        out += ch
        continue
      }
      if (this.head.length < HEAD_CHARS) this.head += ch
      out += ch
      const scheme =
        ch === '/' && (this.prev1 === ':' || (this.prev1 === '/' && this.prev2 === ':'))
      if (SEPARATORS.includes(ch) && !scheme && URL_HEAD.test(this.head)) out += URL_BREAK
      this.prev2 = this.prev1
      this.prev1 = ch
    }
    return out
  }
}

/** 一次性版本（纯函数，前缀稳定） */
export function breakUrls(text: string): string {
  // 没有分隔符就不会插任何折行点（也就不必逐字扫描）
  return /[/?&#=]/.test(text) ? new UrlBreaker().feed(text) : text
}

/** 去掉折行点（复制、读选区时） */
export function stripUrlBreaks(text: string): string {
  return text.includes(URL_BREAK) ? text.replaceAll(URL_BREAK, '') : text
}
