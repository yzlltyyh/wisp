/**
 * 强调色的派生令牌（文字对比度满足 WCAG AA）。
 *
 * 强调色可以是预设（Rust `app::appearance::preset_accent`，浅 / 深各一档）、自定义 `#RRGGBB` 或系统强调色，
 * `ipc/boot` 只把它写成 `<html>` 上的 `--accent`。放在它上面的文字与用它写的文字不能靠固定比例混色——
 * 浅色的橙 / 黄、深色的黄 / 绿上白字只有 2.7–2.9:1。这里按 WCAG 相对亮度为当前强调色求解：
 *
 * - `--accent-fg` + `--accent-fill` / `-hover` / `-pressed`（主按钮、开关、勾选框）：优先白字，直到三种状态的底色对白字都
 *   ≥ 4.5:1——浅色主题在 sRGB 里逐步混黑（强调色本身就是饱和的中间调）；**深色主题在 OKLCH 里只降亮度、色度保持并略微
 *   提高**（`DARK_FILL`；深色下的强调色是粉蓝这类高亮度色，混黑会连色度一起丢，主按钮 / 开关发灰发闷），
 *   填充色的色度不低于强调色的 80%，悬停 / 按下只调亮度。需要压暗太多（强调色本身很亮，压暗后会变成土黄 / 棕色、或色度
 *   保不住）时改用深色字，从强调色原值起按需提亮；
 * - `--accent-text`（链接、选中项、强调色文字）：从调校起点（浅色混 20% 黑、深色混 14% 白）逐步加深 / 提亮，
 *   直到对纯色表面、浮起表面、强调色淡底都 ≥ 4.5:1。
 *
 * 强调色为空串（系统强调色取不到）时删除内联值，沿用 tokens-ui.css 的默认值——它们就是本求解器对默认蓝色的解，
 * 由 `__tests__/contrast.test.ts` 核对（同时遍历 Rust 的全部预设与几种极端自定义色）。
 *
 * 只在 React 窗口里安装（MotionRoot 引用本模块）：Overlay 不用这些令牌，也不为它增加体积。
 */
import { getAppearance, subscribeAppearance } from '../hooks/useAppearance'
import type { ResolvedTheme } from '../ipc/types'
import { oklchToRgb, rgbToOklch, type Rgb } from './oklch'

export type { Rgb }

/** WCAG AA 正文对比度 */
export const MIN_CONTRAST = 4.5

/** tokens.css 的默认强调色（与 Rust 的 blue 预设相同） */
export const DEFAULT_ACCENT: Readonly<Record<ResolvedTheme, string>> = {
  light: '#3b82f6',
  dark: '#60a5fa',
}

/** 两种墨色：白字，或深色字（= 浅色主题的 --g12） */
export const INK = { light: '#ffffff', dark: '#1c1c21' } as const

/** 强调色文字可能出现的不透明底：纯色表面、浮起表面（与 tokens.css 的 --surface-solid / --surface-raised 同值） */
export const SURFACES: Readonly<Record<ResolvedTheme, readonly [string, string]>> = {
  light: ['#fbfbfc', '#ffffff'],
  dark: ['#1c1c1f', '#26262a'],
}

/** 叠在纯色表面上的强调色淡底比例（tokens-ui.css 的 --accent-soft、tokens.css 的 --fill-selected） */
export const SOFT_MIX: Readonly<Record<ResolvedTheme, readonly [number, number]>> = {
  light: [0.12, 0.14],
  dark: [0.18, 0.22],
}

/**
 * 浅色主题填充色的调校（shade：正数混黑、负数混白）：起点、悬停 / 按下相对填充色的偏移、白字最多额外压暗多少。
 * 与 tokens-ui.css 一致：悬停 / 按下逐级加深。深色主题见 DARK_FILL
 */
export const FILL_TUNING: Readonly<{
  light: { start: number; hover: number; pressed: number; maxExtra: number }
}> = {
  light: { start: 0, hover: 0.1, pressed: 0.2, maxExtra: 0.18 },
}

/**
 * 深色主题填充色的调校（OKLCH）：从强调色的亮度起每次降 `step`，色度取强调色的 `chromaBoost` 倍（超出 sRGB 色域时只降
 * 色度到边界），直到填充 / 悬停（亮 `hoverL`）/ 按下（暗 `pressedL`）三种状态对白字都 ≥ 4.5:1。白字的条件：亮度最多降
 * `maxDrop`，且填充色的色度不低于强调色的 `minChroma` 倍（强调色本身接近无彩色时不看色度）；否则改用深色字。
 * 与 tokens-ui.css 一致：深色悬停提亮、按下加深
 */
export const DARK_FILL = {
  step: 0.0025,
  chromaBoost: 1.15,
  hoverL: 0.03,
  pressedL: -0.03,
  maxDrop: 0.2,
  minChroma: 0.8,
  /** 色度低于这个值的强调色（石墨灰等）视为无彩色，不检查色度比例 */
  neutralChroma: 0.03,
} as const

/** 强调色文字的起点（浅色混黑、深色混白） */
export const TEXT_START: Readonly<Record<ResolvedTheme, number>> = { light: 0.2, dark: -0.14 }

export interface AccentTokens {
  fg: string
  fill: string
  fillHover: string
  fillPressed: string
  text: string
}

/** 写到 `<html>` 上的属性与 AccentTokens 字段的对应 */
const PROPS: ReadonlyArray<readonly [string, keyof AccentTokens]> = [
  ['--accent-fg', 'fg'],
  ['--accent-fill', 'fill'],
  ['--accent-fill-hover', 'fillHover'],
  ['--accent-fill-pressed', 'fillPressed'],
  ['--accent-text', 'text'],
]

/** 求解的步长与步数（1% 一档，最多到纯黑 / 纯白） */
const STEPS = 100

export function parseHex(value: string): Rgb | null {
  const m = /^#([0-9a-f]{3}|[0-9a-f]{6})$/i.exec(value.trim())
  if (!m) return null
  const h = m[1] as string
  const full = h.length === 3 ? [...h].map((c) => c + c).join('') : h
  return [
    parseInt(full.slice(0, 2), 16),
    parseInt(full.slice(2, 4), 16),
    parseInt(full.slice(4, 6), 16),
  ]
}

export function toHex(c: Rgb): string {
  return `#${c
    .map((v) =>
      Math.round(Math.min(255, Math.max(0, v)))
        .toString(16)
        .padStart(2, '0'),
    )
    .join('')}`
}

/** 与 `color-mix(in srgb, c (1-s), #000)`（s > 0）/ `color-mix(in srgb, c (1+s), #fff)`（s < 0）相同，结果取整 */
export function shade(c: Rgb, s: number): Rgb {
  const k = Math.min(1, Math.max(-1, s))
  const mix = (v: number) => Math.round(k >= 0 ? v * (1 - k) : v + (255 - v) * -k)
  return [mix(c[0]), mix(c[1]), mix(c[2])]
}

/** 半透明的 c（不透明度 alpha）叠在不透明的 bg 上 */
export function over(c: Rgb, alpha: number, bg: Rgb): Rgb {
  return [
    c[0] * alpha + bg[0] * (1 - alpha),
    c[1] * alpha + bg[1] * (1 - alpha),
    c[2] * alpha + bg[2] * (1 - alpha),
  ]
}

export function luminance(c: Rgb): number {
  const lin = (v: number) => {
    const s = v / 255
    return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4
  }
  return 0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
}

export function contrast(a: Rgb, b: Rgb): number {
  const x = luminance(a)
  const y = luminance(b)
  return (Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05)
}

const hex = (value: string): Rgb => parseHex(value) as Rgb

/** 第 i 档的 shade 值（避免浮点累加误差） */
const at = (start: number, dir: 1 | -1, i: number) =>
  Math.round((start + (dir * i) / STEPS) * 100) / 100

type FillTokens = Pick<AccentTokens, 'fg' | 'fill' | 'fillHover' | 'fillPressed'>

const fillResult = (ink: string, states: readonly Rgb[]): FillTokens => {
  const [fill, hover, pressed] = states.map(toHex) as [string, string, string]
  return { fg: ink, fill, fillHover: hover, fillPressed: pressed }
}
const passesFor = (ink: Rgb, states: readonly Rgb[]) =>
  states.every((c) => contrast(ink, c) >= MIN_CONTRAST)

/** 深色字：从强调色原值起按需提亮（两种主题共用；很亮的强调色本身就适合放深色字）；hover / pressed 是相对填充色的 shade 偏移 */
function solveDarkInk(base: Rgb, hover: number, pressed: number): FillTokens | null {
  const dark = hex(INK.dark)
  for (let i = 0; i <= STEPS; i++) {
    const s = at(0, -1, i)
    const states = [s, s + hover, s + pressed].map((x) => shade(base, x))
    if (passesFor(dark, states)) return fillResult(INK.dark, states)
  }
  return null
}

function solveFillLight(base: Rgb): FillTokens {
  const tune = FILL_TUNING.light
  const states = (s: number) => [s, s + tune.hover, s + tune.pressed].map((x) => shade(base, x))
  const white = hex(INK.light)
  let light: number | null = null
  for (let i = 0; i <= STEPS; i++) {
    const s = at(tune.start, 1, i)
    if (s > 1) break
    if (passesFor(white, states(s))) {
      light = s
      break
    }
  }
  if (light !== null && light - tune.start <= tune.maxExtra + 1e-9)
    return fillResult(INK.light, states(light))
  return solveDarkInk(base, tune.hover, tune.pressed) ?? fillResult(INK.light, states(light ?? 1))
}

/** 深色主题：OKLCH 里只降亮度、保住色度（DARK_FILL） */
function solveFillDark(base: Rgb): FillTokens {
  const t = DARK_FILL
  const [l0, c0, h] = rgbToOklch(base)
  const chroma = c0 * t.chromaBoost
  const white = hex(INK.light)
  let found: { drop: number; states: Rgb[]; ratio: number } | null = null
  for (let i = 0; i * t.step <= l0; i++) {
    const l = l0 - i * t.step
    const fill = oklchToRgb(l, chroma, h)
    const states = [
      fill.rgb,
      oklchToRgb(l + t.hoverL, chroma, h).rgb,
      oklchToRgb(l + t.pressedL, chroma, h).rgb,
    ]
    if (passesFor(white, states)) {
      found = { drop: l0 - l, states, ratio: c0 > 0 ? fill.chroma / c0 : 1 }
      break
    }
  }
  const keepsChroma = (r: number) => c0 < t.neutralChroma || r >= t.minChroma - 1e-9
  if (found && found.drop <= t.maxDrop + 1e-9 && keepsChroma(found.ratio))
    return fillResult(INK.light, found.states)
  // 深色字：悬停提亮、按下加深（sRGB 里与强调色原值同一色相的轻微调整，不涉及色度损失）
  return (
    solveDarkInk(base, -0.07, 0.07) ??
    fillResult(INK.light, found?.states ?? [shade(base, 1), shade(base, 1), shade(base, 1)])
  )
}

function solveFill(base: Rgb, theme: ResolvedTheme): FillTokens {
  return theme === 'light' ? solveFillLight(base) : solveFillDark(base)
}

function solveText(base: Rgb, theme: ResolvedTheme): string {
  const solid = hex(SURFACES[theme][0])
  const backs: Rgb[] = [
    ...SURFACES[theme].map(hex),
    ...SOFT_MIX[theme].map((alpha) => over(base, alpha, solid)),
  ]
  const dir = theme === 'light' ? 1 : -1
  for (let i = 0; i <= STEPS; i++) {
    const c = shade(base, at(TEXT_START[theme], dir, i))
    if (backs.every((b) => contrast(c, b) >= MIN_CONTRAST)) return toHex(c)
  }
  return theme === 'light' ? '#000000' : '#ffffff'
}

/** 为强调色求解派生令牌；空串或无法解析时按默认强调色 */
export function deriveAccentTokens(accent: string, theme: ResolvedTheme): AccentTokens {
  const base = parseHex(accent) ?? hex(DEFAULT_ACCENT[theme])
  return { ...solveFill(base, theme), text: solveText(base, theme) }
}

/** 把派生令牌写到 `<html>`（空串：删除内联值，用 tokens-ui.css 的默认值） */
export function applyAccentTokens(accent: string, theme: ResolvedTheme): void {
  if (typeof document === 'undefined') return
  const style = document.documentElement.style
  if (!parseHex(accent)) {
    for (const [prop] of PROPS) style.removeProperty(prop)
    return
  }
  const tokens = deriveAccentTokens(accent, theme)
  for (const [prop, key] of PROPS) style.setProperty(prop, tokens[key])
}

let installed = false
let applied = ''

/** 按当前外观（boot / 最近一次 appearance:changed）写入派生令牌；不碰 IPC，模块加载时即可调用 */
export function applyCurrentAccent(): void {
  const { accent, theme } = getAppearance()
  const key = `${theme}|${accent}`
  if (key === applied) return
  applied = key
  applyAccentTokens(accent, theme)
}

/**
 * 写入一次，并随 `appearance:changed` 更新（幂等；监听在事件到达的同一任务里同步执行，与 `ipc/boot` 改写
 * `--accent` / `data-theme` 落在同一帧，不会有一帧旧的派生色）。窗口生命周期内不卸载。
 * 订阅会经传输层注册事件监听，所以在组件挂载时（传输层已就绪）调用，不在模块加载时调用。
 */
export function installAccentTokens(): void {
  if (installed) return
  installed = true
  applyCurrentAccent()
  subscribeAppearance(applyCurrentAccent)
}
