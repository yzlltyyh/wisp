/**
 * 界面语言相关的小格式化：列表拼接按当前界面语言（中文「A、B 和 C」，英文「A, B, and C」），不硬编码分隔符。
 *
 * 中文的连接词（「和」「或」）与两侧的西文之间补一个空格（「OpenAI、DeepSeek 和 Ollama」），与全站中西文混排的
 * 留白一致；Intl.ListFormat 本身不加。
 */
import { getLocale } from '../../shared/i18n/i18n'

const cache = new Map<string, Intl.ListFormat>()
/** 由汉字组成的连接词（「和」「或」；「、」是标点，不加空格） */
const HAN_WORD = /^\p{Script=Han}+$/u
/** 可见的 ASCII 字符（西文、数字、括号、花括号……） */
const ASCII_END = /[\x21-\x7e]$/
const ASCII_START = /^[\x21-\x7e]/

/** 把若干项拼成一句（`type: 'conjunction'`：「和」/「and」；`'unit'`：只用分隔符） */
export function formatList(
  items: readonly string[],
  type: 'conjunction' | 'unit' = 'conjunction',
): string {
  const locale = getLocale()
  const key = `${locale}|${type}`
  let f = cache.get(key)
  if (!f) {
    f = new Intl.ListFormat(locale, { style: 'long', type })
    cache.set(key, f)
  }
  const parts = f.formatToParts(items)
  return parts
    .map((part, i) => {
      if (part.type !== 'literal' || !HAN_WORD.test(part.value.trim())) return part.value
      const before = ASCII_END.test(parts[i - 1]?.value ?? '') ? ' ' : ''
      const after = ASCII_START.test(parts[i + 1]?.value ?? '') ? ' ' : ''
      return `${before}${part.value.trim()}${after}`
    })
    .join('')
}
