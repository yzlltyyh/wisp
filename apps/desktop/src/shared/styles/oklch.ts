/**
 * sRGB ↔ OKLab / OKLCH 换算（Björn Ottosson 的矩阵，与 CSS Color 4 的 oklch() 同一定义），只用于强调色的派生令牌
 * （styles/accent.ts）：深色主题的填充色在 OKLCH 里只降亮度、保住色度，而不是在 sRGB 里混黑——从浅色主题的粉蓝混黑
 * 去凑白字对比度，亮度降了、色度也跟着丢，主按钮 / 开关会发灰发闷。
 */

export type Rgb = readonly [number, number, number]
/** [L 0–1, C ≥ 0, h 弧度] */
export type Lch = readonly [number, number, number]

const toLinear = (v: number) => {
  const s = v / 255
  return s <= 0.04045 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4
}
const toGamma = (v: number) => (v <= 0.0031308 ? 12.92 * v : 1.055 * v ** (1 / 2.4) - 0.055) * 255

export function rgbToOklch(c: Rgb): Lch {
  const r = toLinear(c[0])
  const g = toLinear(c[1])
  const b = toLinear(c[2])
  const l = Math.cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b)
  const m = Math.cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b)
  const s = Math.cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b)
  const L = 0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s
  const A = 1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s
  const B = 0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s
  return [L, Math.hypot(A, B), Math.atan2(B, A)]
}

/** OKLCH → 线性 sRGB（0–1，可能越界） */
function oklchToLinear(L: number, C: number, h: number): [number, number, number] {
  const A = C * Math.cos(h)
  const B = C * Math.sin(h)
  const l = (L + 0.3963377774 * A + 0.2158037573 * B) ** 3
  const m = (L - 0.1055613458 * A - 0.0638541728 * B) ** 3
  const s = (L - 0.0894841775 * A - 1.291485548 * B) ** 3
  return [
    4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
    -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
    -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
  ]
}

const EPS = 1e-6
const inGamut = (v: readonly number[]) => v.every((x) => x >= -EPS && x <= 1 + EPS)

/**
 * 亮度 L、色相 h 下最接近色度 C 的 sRGB 颜色：C 在 sRGB 色域外时按二分法降到色域边界（只降色度，亮度与色相不变）。
 * 返回取整后的颜色与实际用到的色度
 */
export function oklchToRgb(L: number, C: number, h: number): { rgb: Rgb; chroma: number } {
  let chroma = C
  let lin = oklchToLinear(L, chroma, h)
  if (!inGamut(lin)) {
    let lo = 0
    let hi = C
    for (let i = 0; i < 24; i++) {
      const mid = (lo + hi) / 2
      if (inGamut(oklchToLinear(L, mid, h))) lo = mid
      else hi = mid
    }
    chroma = lo
    lin = oklchToLinear(L, chroma, h)
  }
  const ch = (v: number) =>
    Math.round(Math.min(255, Math.max(0, toGamma(Math.min(1, Math.max(0, v))))))
  return { rgb: [ch(lin[0]), ch(lin[1]), ch(lin[2])], chroma }
}
