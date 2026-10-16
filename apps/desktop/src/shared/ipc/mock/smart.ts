/**
 * 浏览器 mock 的「智能识别」替身：用几条简单的正则近似 Rust 的识别器与「粘贴为…」变换，只为让芯片条、
 * 「粘贴为…」子页与验证码场景在浏览器里有内容可看。规则远比 Rust 宽松（没有校验位、没有关键词以外的排除），
 * 不能拿来验证识别的正确性——那由 wisp-clipboard 的表驱动测试负责。
 */
import type { ClipInspect, ClipKind, Entity, EntityKind, TransformId } from '../types'

interface Rule {
  kind: EntityKind
  re: RegExp
  /** 取哪个捕获组作为值（默认整段） */
  group?: number
  /** 值的规范化（去掉空格、货币符号） */
  value?: (raw: string) => string
}

const RULES: readonly Rule[] = [
  // 验证码：关键词后面不远处的 4–8 位数字
  { kind: 'otp', re: /(?:验证码|校验码|动态码|code|otp)[^0-9]{0,12}(\d{4,8})(?!\d)/gi, group: 1 },
  { kind: 'email', re: /[\w.+-]+@[\w-]+(?:\.[\w-]+)+/g },
  { kind: 'url', re: /https?:\/\/[^\s"'<>，。]+/g },
  {
    kind: 'phone',
    re: /(?<!\d)1[3-9]\d[ -]?\d{4}[ -]?\d{4}(?!\d)/g,
    value: (s) => s.replace(/\D/g, ''),
  },
  { kind: 'ip', re: /(?<![\d.])(?:\d{1,3}\.){3}\d{1,3}(?![\d.])/g },
  { kind: 'tracking', re: /\b(?:SF\d{12}|YT\d{13}|JD[0-9A-Z]{13})\b/g },
  {
    kind: 'amount',
    re: /(?:[¥￥$€£]\s?\d[\d,]*(?:\.\d+)?|\d[\d,]*(?:\.\d+)?\s?元)/g,
    value: (s) => s.replace(/[^\d.]/g, ''),
  },
  { kind: 'datetime', re: /\b\d{4}-\d{2}-\d{2}(?:[ T]\d{2}:\d{2})?\b/g },
  { kind: 'bank_card', re: /(?<!\d)62\d{14,17}(?!\d)/g },
]

const SENSITIVE: ReadonlySet<EntityKind> = new Set(['id_card', 'bank_card'])

function recognize(text: string): Entity[] {
  const out: Entity[] = []
  for (const rule of RULES) {
    for (const m of text.matchAll(rule.re)) {
      const raw = rule.group ? (m[rule.group] ?? '') : m[0]
      const start = (m.index ?? 0) + (rule.group ? m[0].indexOf(raw) : 0)
      const end = start + raw.length
      // 与已有的片段重叠时跟随先登记的规则（验证码优先于电话号码与金额，与 Rust 的优先级一致）
      if (out.some((e) => start < e.end && end > e.start)) continue
      out.push({
        kind: rule.kind,
        value: rule.value ? rule.value(raw) : raw,
        start,
        end,
        sensitive: SENSITIVE.has(rule.kind),
      })
    }
  }
  return out.sort((a, b) => a.start - b.start).slice(0, 32)
}

function parsesAsJson(text: string): boolean {
  const s = text.trim()
  if (!s.startsWith('{') && !s.startsWith('[')) return false
  try {
    JSON.parse(s)
    return true
  } catch {
    return false
  }
}

const TRACKING = /[?&](?:utm_[a-z]+|fbclid|gclid|spm)=/i
const CASED = new Set<ClipKind>(['text', 'rich_text'])

function transforms(kind: ClipKind, text: string): TransformId[] {
  const out: TransformId[] = []
  if (/\n/.test(text.trim())) out.push('join_lines')
  if (text !== text.trim()) out.push('trim')
  if (CASED.has(kind) && /[a-z]/i.test(text)) out.push('upper', 'lower', 'title')
  if (CASED.has(kind) && /[！-～　]/.test(text)) out.push('half_width')
  if (CASED.has(kind) && /[!-~]/.test(text)) out.push('full_width')
  if (kind !== 'link' && parsesAsJson(text)) out.push('json_pretty', 'json_minify')
  if (TRACKING.test(text)) out.push('strip_tracking')
  return out
}

/** 一段文字的识别结果（图片、文件没有文字：空结果） */
export function mockInspect(kind: ClipKind, text: string | null): ClipInspect {
  if (text === null || kind === 'image' || kind === 'files') return { entities: [], transforms: [] }
  return { entities: recognize(text), transforms: transforms(kind, text) }
}
