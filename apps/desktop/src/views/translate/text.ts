/**
 * 翻译视图用到的纯文本规则。都是纯函数，单测见 `__tests__/text.test.ts`。
 *
 * - `isDictionaryInput`：词典式触发判定，逐条移植 Rust `wisp_prompts::is_dictionary_input`，
 *   两边用同一张判定表测试。前端只在拿不到权威值时用它推断（Pinned 的 PinState 没有 dict 字段、换风格后 Meta
 *   到达之前的排版预判），真正走不走词典式由 Rust 决定并经 `Meta.dict_mode` 告知；
 * - `detectInput`：输入模式里实时显示「源语言 → 目标」的方向判定（逐条移植 Rust `detect_from_census`）；
 * - `planSwap`：交换方向是否有意义（自动检测出的源语言不能和目标相同）；
 * - `refineEligible`：提升译文的可用条件（原文 ≥ 40 词或 ≥ 60 字）；
 * - `estimateTokens`：长文本提示里的 token 估算（拉丁字符 ÷ 4、CJK × 1）。
 */
import type { Lang } from '../../shared/ipc/types'

type CharClass = 'han' | 'kana' | 'hangul' | 'latin' | 'other_letter' | 'digit' | 'symbol' | 'space'

const LETTER = /\p{L}/u
const WHITESPACE = /\s/u

/** 与 Rust `script::classify` 同一套区间 */
export function classify(ch: string): CharClass {
  const u = ch.codePointAt(0) ?? 0
  if (u < 0x80) {
    if ((u >= 0x41 && u <= 0x5a) || (u >= 0x61 && u <= 0x7a)) return 'latin'
    if (u >= 0x30 && u <= 0x39) return 'digit'
    if (u === 0x20 || (u >= 0x09 && u <= 0x0d)) return 'space'
    return 'symbol'
  }
  if (
    (u >= 0x3400 && u <= 0x4dbf) ||
    (u >= 0x4e00 && u <= 0x9fff) ||
    (u >= 0xf900 && u <= 0xfaff) ||
    u === 0x3007 ||
    (u >= 0x20000 && u <= 0x323af)
  )
    return 'han'
  if (
    (u >= 0x3041 && u <= 0x3096) ||
    (u >= 0x309d && u <= 0x309f) ||
    (u >= 0x30a1 && u <= 0x30fa) ||
    (u >= 0x30fd && u <= 0x30ff) ||
    (u >= 0x31f0 && u <= 0x31ff) ||
    (u >= 0xff66 && u <= 0xff6f) ||
    (u >= 0xff71 && u <= 0xff9d)
  )
    return 'kana'
  if (
    (u >= 0xac00 && u <= 0xd7a3) ||
    (u >= 0x1100 && u <= 0x11ff) ||
    (u >= 0x3131 && u <= 0x318e) ||
    (u >= 0xa960 && u <= 0xa97f) ||
    (u >= 0xd7b0 && u <= 0xd7ff)
  )
    return 'hangul'
  // 中日共用的中点与长音符按符号处理
  if (u === 0x30fb || u === 0x30fc) return 'symbol'
  if (u >= 0xff10 && u <= 0xff19) return 'digit'
  if ((u >= 0xff21 && u <= 0xff3a) || (u >= 0xff41 && u <= 0xff5a)) return 'latin'
  const letter = LETTER.test(ch)
  if (
    ((u >= 0x00c0 && u <= 0x024f) ||
      (u >= 0x0250 && u <= 0x02af) ||
      (u >= 0x1e00 && u <= 0x1eff)) &&
    letter
  )
    return 'latin'
  if (WHITESPACE.test(ch)) return 'space'
  if (letter) return 'other_letter'
  return 'symbol'
}

const isCjkClass = (c: CharClass) => c === 'han' || c === 'kana' || c === 'hangul'

/** 拉丁文本的最大词数 / 字符数，CJK 文本的最大非空白字符数（与 Rust dict.rs 一致） */
const MAX_WORDS = 3
const MAX_LATIN_CHARS = 30
const MAX_CJK_CHARS = 4
/** 超过这个字节数的输入不可能满足条件，直接返回（Rust 按 UTF-8 字节算，这里按 UTF-16 单元近似，同样只是快速拒绝） */
const FAST_REJECT_UNITS = 256

const ASCII_ALNUM = /^[0-9A-Za-z]$/

function hasSentencePunctuation(text: string): boolean {
  const chars = Array.from(text)
  for (let i = 0; i < chars.length; i++) {
    const c = chars[i] as string
    if (c === '!' || c === '?' || c === '。' || c === '！' || c === '？' || c === '｡') return true
    if (c === '.' || c === '．') {
      const prev = chars[i - 1]
      const next = chars[i + 1]
      const inner =
        prev !== undefined && ASCII_ALNUM.test(prev) && next !== undefined && ASCII_ALNUM.test(next)
      if (!inner) return true
    }
  }
  return false
}

function isUrl(text: string): boolean {
  if (text.slice(0, 4).toLowerCase() === 'www.') return true
  const pos = text.indexOf('://')
  if (pos < 0) return false
  return /^[A-Za-z][A-Za-z0-9+.-]*$/.test(text.slice(0, pos))
}

const PATH_PREFIXES = ['/', '~/', './', '../', '~\\', '.\\', '..\\']

function isPath(text: string): boolean {
  if (PATH_PREFIXES.some((p) => text.startsWith(p))) return true
  if (/^[A-Za-z]:[\\/]/.test(text)) return true
  let slashes = 0
  for (const c of text) if (c === '/' || c === '\\') slashes++
  return slashes >= 2
}

/** 输入是否像单词 / 短语（适合词典式）。规则与 Rust `is_dictionary_input` 完全一致 */
export function isDictionaryInput(text: string): boolean {
  const t = text.trim()
  if (t === '' || t.length > FAST_REJECT_UNITS) return false
  if (/[\n\r\u2028\u2029]/.test(t)) return false
  if (hasSentencePunctuation(t) || isUrl(t) || isPath(t)) return false
  let alpha = 0
  let cjk = 0
  let nonSpace = 0
  const chars = Array.from(t)
  for (const c of chars) {
    const cls = classify(c)
    if (cls === 'space') continue
    if (cls === 'latin' || cls === 'other_letter') alpha++
    else if (isCjkClass(cls)) cjk++
    nonSpace++
  }
  if (alpha > 0 && cjk > 0) return false
  if (alpha > 0)
    return chars.length <= MAX_LATIN_CHARS && t.split(/\s+/u).filter(Boolean).length <= MAX_WORDS
  if (cjk > 0) return nonSpace <= MAX_CJK_CHARS
  return false
}

/** 文本的拉丁词数与 CJK 字数（提升译文的门槛、长文本估算共用） */
export function measureText(text: string): {
  latinWords: number
  cjkChars: number
  latinChars: number
} {
  let latinWords = 0
  let cjkChars = 0
  let latinChars = 0
  let inWord = false
  for (const c of text) {
    const cls = classify(c)
    if (isCjkClass(cls)) cjkChars++
    else if (cls !== 'space') latinChars++
    const wordChar = cls === 'latin' || cls === 'digit' || cls === 'other_letter' || c === '_'
    if (wordChar && !inWord) latinWords++
    inWord = wordChar || (inWord && (c === "'" || c === '-' || c === '’'))
  }
  return { latinWords, cjkChars, latinChars }
}

/** 提升译文的门槛：原文 ≥ 40 词或 ≥ 60 字 */
export const REFINE_MIN_WORDS = 40
export const REFINE_MIN_CJK = 60

export function refineEligible(source: string): boolean {
  const m = measureText(source)
  return m.latinWords >= REFINE_MIN_WORDS || m.cjkChars >= REFINE_MIN_CJK
}

/** token 估算：拉丁字符 ÷ 4、CJK 每字 1 个 */
const LATIN_CHARS_PER_TOKEN = 4

export function estimateTokens(text: string): number {
  const m = measureText(text)
  return Math.max(1, Math.round(m.cjkChars + m.latinChars / LATIN_CHARS_PER_TOKEN))
}

/** 超过这个字数时长文本提示额外强调「可能被截断」 */
export const LONG_TEXT_HARD_CHARS = 8000

/** 字数（按 Unicode 码位计，与 Rust 的 `chars().count()` 一致） */
export function charCount(text: string): number {
  let n = 0
  for (const _ of text) n++
  return n
}

/** 方向判定的结果（与 Rust `wisp_prompts::Detection` 同形） */
export interface LangDetection {
  readonly lang: Lang
  /** 混排比值落在阈值 ±30% 以内、或没有可判断的文字（语言条显示低置信点） */
  readonly lowConfidence: boolean
}

/** 汉字 / 拉丁词数的阈值与低置信度带，与 Rust `lang.rs` 一致 */
const HAN_RATIO = 0.5
const AMBIGUITY_BAND = 0.3
/** 只统计前这么多个字符：输入时不随长度变慢（超过长文本阈值的部分不影响方向，Rust 统计全文） */
const DETECT_SCAN_CHARS = 8000
/** 词内连接符（只有后面紧跟词字符时才算词内），与 Rust `script::is_word_joiner` 一致 */
const WORD_JOINERS = new Set(["'", '\u2019', '-', '\u2010', '\u2011'])

/**
 * 输入模式的实时方向判定：逐条移植 Rust `wisp_prompts::lang::detect_from_census`（含
 * `script::Census` 的统计口径），两边用同一张判定表测试（`__tests__/text.test.ts` ↔ `crates/wisp-prompts/tests/lang.rs`）：
 * 1. 有假名 / 谚文 → ja / ko（取多的一方）——日文里的汉字不会被当成中文；
 * 2. 有汉字：汉字数 ≥ 0.5 × 拉丁词数 → zh-CN，否则 en；比值落在阈值 ±30% 以内为低置信；
 * 3. 没有拉丁字母也没有汉字，或以其他文字（西里尔……）为主 → other（低置信）；
 * 4. 纯拉丁文本 → en。Rust 对较长的拉丁文本还会用 whatlang **只降低置信度**（语言仍是 en），前端不做这一步。
 * 只用于显示与交给翻译视图的初值，真正的方向以 Rust 的检测（Meta）为准。没有任何非空白字符时返回 null。
 */
export function detectInput(text: string): LangDetection | null {
  let han = 0
  let kana = 0
  let hangul = 0
  let latinLetters = 0
  let otherLetters = 0
  let latinWords = 0
  let nonSpace = 0
  // 当前词的状态（与 Census::of 一致）：是否在词内、词内是否出现过拉丁字母、上一个字符是否为挂起的连接符
  let inWord = false
  let wordHasLatin = false
  let pendingJoiner = false
  let scanned = 0
  for (const c of text) {
    if (++scanned > DETECT_SCAN_CHARS) break
    const cls = classify(c)
    if (cls !== 'space') nonSpace++
    if (cls === 'han') han++
    else if (cls === 'kana') kana++
    else if (cls === 'hangul') hangul++
    else if (cls === 'latin') latinLetters++
    else if (cls === 'other_letter') otherLetters++
    const wordChar = c === '_' || cls === 'latin' || cls === 'digit'
    if (wordChar) {
      if (!inWord) {
        inWord = true
        wordHasLatin = false
      }
      pendingJoiner = false
      if (cls === 'latin') wordHasLatin = true
    } else if (inWord && !pendingJoiner && WORD_JOINERS.has(c)) {
      pendingJoiner = true
    } else if (inWord) {
      if (wordHasLatin) latinWords++
      inWord = false
      pendingJoiner = false
    }
  }
  if (inWord && wordHasLatin) latinWords++
  if (nonSpace === 0) return null
  if (kana > 0 || hangul > 0) return { lang: hangul > kana ? 'ko' : 'ja', lowConfidence: false }
  if (han === 0 && latinLetters === 0) return { lang: 'other', lowConfidence: true }
  if (han === 0 && otherLetters > latinLetters) return { lang: 'other', lowConfidence: true }
  if (han > 0) {
    const threshold = HAN_RATIO * latinWords
    return {
      lang: han >= threshold ? 'zh-CN' : 'en',
      lowConfidence: latinWords > 0 && Math.abs(han - threshold) <= AMBIGUITY_BAND * threshold,
    }
  }
  return { lang: 'en', lowConfidence: false }
}

/** 只要语言（没有文字时为 null） */
export function detectInputLang(text: string): Lang | null {
  return detectInput(text)?.lang ?? null
}

/**
 * 自动方向：源是主语言（v1 为中文）→ 目标是第二语言（英文）；其他任何语言 → 主语言。
 * `primary` / `secondary` 取设置 → 翻译。
 */
export function autoTarget(source: Lang | null, primary: Lang, secondary: Lang): Lang {
  return source === primary ? secondary : primary
}

/** 语言条的「交换方向」：在主语言与第二语言之间切换目标 */
export function swapTarget(target: Lang, primary: Lang, secondary: Lang): Lang {
  return target === primary ? secondary : primary
}

/** 交换方向的结果：新目标与（需要时）强制显示的源语言；不可交换时给出原因 */
export type SwapPlan =
  | { readonly ok: true; readonly target: Lang; readonly forceSource: Lang | null }
  | { readonly ok: false }

/**
 * 交换方向是否有意义。源语言是自动检测出来的：交换后目标若等于检测到的源语言（例如英文原文
 * 「EN → 中文」交换成「EN → 英语」），输出只会是原文本身——
 * - 检测本身不确定（中英混排落在阈值附近、低置信）：交换有意义，表示「把原文当作另一种语言」，源语言强制显示为
 *   交换前的目标（「中文 → EN」交换为「EN → 中文」）；
 * - 否则没有可交换的方向，返回 `ok: false`（语言条禁用交换并说明原因）。
 * `forced`：上一次交换强制显示的源语言（再次交换即换回检测结果）。
 */
export function planSwap(opts: {
  detected: Lang | null
  forced: Lang | null
  target: Lang
  lowConfidence: boolean
  primary: Lang
  secondary: Lang
}): SwapPlan {
  const { detected, forced, target, lowConfidence, primary, secondary } = opts
  const next = swapTarget(target, primary, secondary)
  // 已经强制过源语言：换回检测结果（目标回到交换前）
  if (forced !== null) return { ok: true, target: next, forceSource: null }
  if (detected === null || next !== detected) return { ok: true, target: next, forceSource: null }
  return lowConfidence ? { ok: true, target: next, forceSource: target } : { ok: false }
}

const isCjkLang = (l: Lang | null) => l === 'zh-CN' || l === 'ja' || l === 'ko'

/**
 * 首字前骨架屏的行数：按原文长度估计译文大致有几行（替换为正文时高度变化最小）。按目标语言修正长度：
 * CJK → 拉丁文字的译文通常是原文字数的约 2 倍，拉丁文字 → CJK 约 0.6 倍；词典式固定 3 行
 */
export function skeletonLines(source: string, target: Lang | null, dict: boolean): number {
  if (dict) return 3
  const n = source.length
  const m = measureText(
    source.length > DETECT_SCAN_CHARS ? source.slice(0, DETECT_SCAN_CHARS) : source,
  )
  const cjkSource = m.cjkChars > m.latinWords
  const factor = cjkSource && !isCjkLang(target) ? 1.9 : !cjkSource && isCjkLang(target) ? 0.6 : 1
  const est = n * factor
  return est <= 24 ? 1 : est <= 96 ? 2 : 3
}
