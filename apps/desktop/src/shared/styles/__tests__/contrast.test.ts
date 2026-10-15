/**
 * 对比度：从 tokens.css 解析令牌，按 WCAG 2 公式计算。
 * - 正文 --fg 与次要文字 --fg-2 对纯色表面、浮起表面 ≥ 4.5:1（浅色 / 深色各算一遍）；
 * - 强调色文字、状态色文字（叠在对应淡底上）、tooltip 文字 ≥ 4.5:1；
 * - 主按钮 / 危险按钮的标签对三种状态的底色 ≥ 4.5:1（13 px 的按钮标签是文字，适用 WCAG 1.4.3，不是 1.4.11 的 3:1）；
 * - 强调色派生令牌（styles/accent.ts）：遍历 Rust 的全部预设色（浅 / 深各一组，从 `app/appearance.rs` 读取）、常见的系统
 *   强调色与几种极端自定义色，填充三态与强调色文字都 ≥ 4.5:1；默认蓝色的解与 tokens-ui.css 的默认值一致。
 * --fg-3 不用于传达信息的文字，不在此列（只要求作为图标 / 占位符 ≥ 3:1 的对纯色表面）。
 */
import { describe, expect, it } from 'vitest'
import {
  contrast as contrastRgb,
  DARK_FILL,
  DEFAULT_ACCENT,
  deriveAccentTokens,
  INK,
  MIN_CONTRAST,
  over as overRgb,
  parseHex,
  SOFT_MIX,
  SURFACES,
  type Rgb,
} from '../accent'
import { rgbToOklch } from '../oklch'

/**
 * vitest 配置了 `css: false`（CSS 导入一律为空串，`?raw` 也不例外），这里直接读文件。前端 tsconfig 不带 node 类型，
 * 用 Node 22+ 的 `process.getBuiltinModule` 取 fs，避免为测试引入 @types/node。
 */
interface NodeFs {
  readFileSync(path: string, encoding: 'utf8'): string
}
const proc = (
  globalThis as unknown as { process: { getBuiltinModule(name: 'node:fs'): NodeFs; cwd(): string } }
).process
// vitest 的根目录是 apps/desktop；基础令牌与组件派生令牌拼在一起解析（同名块按出现顺序合并）
const readRepo = (path: string) =>
  proc.getBuiltinModule('node:fs').readFileSync(`${proc.cwd()}/${path}`, 'utf8')
const read = (file: string) => readRepo(`src/shared/styles/${file}`)
const BASE_CSS = read('tokens.css')
const UI_CSS = read('tokens-ui.css')

type RGBA = [number, number, number, number]

function block(css: string, selector: string): Map<string, string> {
  const at = css.indexOf(`${selector} {`)
  if (at < 0) throw new Error(`令牌文件里找不到 ${selector}`)
  const body = css.slice(css.indexOf('{', at) + 1, css.indexOf('\n}', at))
  const out = new Map<string, string>()
  const re = /(--[\w-]+)\s*:\s*([^;]+);/g
  for (const m of body.replace(/\/\*[\s\S]*?\*\//g, '').matchAll(re))
    out.set(m[1] as string, (m[2] as string).trim().replace(/\s+/g, ' '))
  return out
}

const LIGHT = new Map([...block(BASE_CSS, ':root'), ...block(UI_CSS, ':root')])
const DARK = new Map([
  ...LIGHT,
  ...block(BASE_CSS, "[data-theme='dark']"),
  ...block(UI_CSS, "[data-theme='dark']"),
])

function parseColor(raw: string, vars: Map<string, string>): RGBA {
  // prettier 可能把长声明折行：去掉括号内侧的空白
  const v = raw.trim().replace(/\(\s+/g, '(').replace(/\s+\)/g, ')')
  const ref = /^var\((--[\w-]+)\)$/.exec(v)
  if (ref) return parseColor(vars.get(ref[1] as string) ?? '', vars)
  if (v === 'transparent') return [0, 0, 0, 0]
  const hex = /^#([0-9a-f]{3}|[0-9a-f]{6})$/i.exec(v)
  if (hex) {
    const h =
      (hex[1] as string).length === 3
        ? [...(hex[1] as string)].map((c) => c + c).join('')
        : (hex[1] as string)
    return [
      parseInt(h.slice(0, 2), 16),
      parseInt(h.slice(2, 4), 16),
      parseInt(h.slice(4, 6), 16),
      1,
    ]
  }
  const rgb = /^rgb\((\d+) (\d+) (\d+)(?: \/ ([\d.]+))?\)$/.exec(v)
  if (rgb)
    return [
      Number(rgb[1]),
      Number(rgb[2]),
      Number(rgb[3]),
      rgb[4] === undefined ? 1 : Number(rgb[4]),
    ]
  const mix = /^color-mix\(in srgb, (.+?) (\d+)%, (.+)\)$/.exec(v)
  if (mix) {
    const a = parseColor(mix[1] as string, vars)
    const b = parseColor(mix[3] as string, vars)
    const p = Number(mix[2]) / 100
    // srgb 预乘 alpha 混合（与 CSS Color 5 一致）
    const alpha = a[3] * p + b[3] * (1 - p)
    if (alpha === 0) return [0, 0, 0, 0]
    const ch = (i: number) =>
      ((a[i] as number) * a[3] * p + (b[i] as number) * b[3] * (1 - p)) / alpha
    return [ch(0), ch(1), ch(2), alpha]
  }
  throw new Error(`无法解析颜色：${v}`)
}

/** 半透明前景叠在不透明背景上 */
function over(fg: RGBA, bg: RGBA): RGBA {
  const a = fg[3]
  return [fg[0] * a + bg[0] * (1 - a), fg[1] * a + bg[1] * (1 - a), fg[2] * a + bg[2] * (1 - a), 1]
}

function luminance([r, g, b]: RGBA): number {
  const lin = (c: number) => {
    const s = c / 255
    return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4
  }
  return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

function contrast(a: RGBA, b: RGBA): number {
  const [x, y] = [luminance(a), luminance(b)].sort((m, n) => n - m) as [number, number]
  return (x + 0.05) / (y + 0.05)
}

function ratio(
  vars: Map<string, string>,
  fg: string,
  bg: string,
  base = '--surface-solid',
): number {
  const surface = parseColor(`var(${base})`, vars)
  const back = over(parseColor(`var(${bg})`, vars), surface)
  return contrast(over(parseColor(`var(${fg})`, vars), back), back)
}

describe.each([
  ['浅色', LIGHT],
  ['深色', DARK],
])('对比度 · %s', (_name, vars) => {
  it('正文与次要文字对纯色 / 浮起表面 ≥ 4.5:1', () => {
    for (const fg of ['--fg', '--fg-2'])
      for (const bg of ['--surface-solid', '--surface-raised']) {
        expect(ratio(vars, fg, bg), `${fg} on ${bg}`).toBeGreaterThanOrEqual(4.5)
      }
  })

  it('强调色文字、状态色文字（叠在淡底上）、tooltip 文字 ≥ 4.5:1', () => {
    expect(ratio(vars, '--accent-text', '--surface-solid')).toBeGreaterThanOrEqual(4.5)
    expect(ratio(vars, '--accent-text', '--accent-soft')).toBeGreaterThanOrEqual(4.5)
    expect(ratio(vars, '--danger-text', '--danger-soft')).toBeGreaterThanOrEqual(4.5)
    expect(ratio(vars, '--warning-text', '--warning-soft')).toBeGreaterThanOrEqual(4.5)
    expect(ratio(vars, '--success-text', '--success-soft')).toBeGreaterThanOrEqual(4.5)
    expect(ratio(vars, '--fg-2', '--fill-control')).toBeGreaterThanOrEqual(4.5)
    expect(ratio(vars, '--tooltip-fg', '--tooltip-bg')).toBeGreaterThanOrEqual(4.5)
  })

  it('按钮里的快捷键提示（Kbd inline）与提示行的链接色操作 ≥ 4.5:1（components.css「按钮里的快捷键提示」）', () => {
    // 主 / 危险按钮：与标签同色、不透明（不叠透明度），即标签色对三种状态的底色
    for (const bg of ['--accent-fill', '--accent-fill-hover', '--accent-fill-pressed'])
      expect(ratio(vars, '--accent-fg', bg), `kbd on ${bg}`).toBeGreaterThanOrEqual(4.5)
    // 次要 / 幽灵按钮：--fg-2 对控件填充的三种状态与透明底（叠在纯色 / 浮起表面上）
    for (const bg of [
      '--fill-control',
      '--fill-control-hover',
      '--fill-control-pressed',
      '--fill-hover',
      '--surface-raised',
    ])
      expect(ratio(vars, '--fg-2', bg), `kbd on ${bg}`).toBeGreaterThanOrEqual(4.5)
    // 翻译提示行的「调高上限」：强调色文字对透明底与悬停底（按下态只在点击的那 80 ms 里出现，不是要阅读的状态；浅色 4.4:1）
    for (const bg of ['--surface-solid', '--fill-hover'])
      expect(ratio(vars, '--accent-text', bg), `notice action on ${bg}`).toBeGreaterThanOrEqual(4.5)
  })

  it('按钮标签对三种状态的底色 ≥ 4.5:1；图标色 --fg-3 对纯色表面 ≥ 3:1', () => {
    for (const bg of ['--accent-fill', '--accent-fill-hover', '--accent-fill-pressed'])
      expect(ratio(vars, '--accent-fg', bg), bg).toBeGreaterThanOrEqual(4.5)
    // 危险按钮的悬停 / 按下在 components.css 里由 --danger-fill 再混入黑色，只会更深
    expect(ratio(vars, '--danger-fg', '--danger-fill')).toBeGreaterThanOrEqual(4.5)
    expect(ratio(vars, '--fg-3', '--surface-solid')).toBeGreaterThanOrEqual(3)
  })
})

// ---------------------------------------------------------------------------
// 强调色派生令牌（styles/accent.ts）

const rgb = (c: RGBA): Rgb => [c[0], c[1], c[2]]
const hexRgb = (h: string) => parseHex(h) as Rgb

/** Rust 的预设表：`"name" => ("#light", "#dark")` */
const PRESETS = [
  ...readRepo('src-tauri/src/app/appearance.rs').matchAll(
    /"(\w+)" => \("(#[0-9A-Fa-f]{6})", "(#[0-9A-Fa-f]{6})"\)/g,
  ),
].map((m) => ({ name: m[1] as string, light: m[2] as string, dark: m[3] as string }))

/** 系统强调色（macOS 各色、Windows 默认）与极端自定义色 */
const CUSTOM = [
  '#007AFF',
  '#953D96',
  '#F74F9E',
  '#E0383E',
  '#F7821B',
  '#FFC600',
  '#62BA46',
  '#989898',
  '#0078D4',
  '#000000',
  '#FFFFFF',
  '#FFFF00',
  '#00FFFF',
  '#808080',
  '#000080',
  '#FF0000',
]

describe('强调色派生令牌（styles/accent.ts）', () => {
  it('从 Rust 读到全部 8 个预设', () => {
    expect(PRESETS.map((p) => p.name)).toEqual([
      'blue',
      'purple',
      'pink',
      'red',
      'orange',
      'yellow',
      'green',
      'graphite',
    ])
  })

  it('求解器用的表面、淡底比例、默认强调色与令牌文件一致', () => {
    for (const [theme, vars] of [
      ['light', LIGHT],
      ['dark', DARK],
    ] as const) {
      const solid = rgb(parseColor('var(--surface-solid)', vars))
      const raised = rgb(parseColor('var(--surface-raised)', vars))
      expect([solid, raised]).toEqual(SURFACES[theme].map(hexRgb))
      expect(rgb(parseColor('var(--accent)', vars))).toEqual(hexRgb(DEFAULT_ACCENT[theme]))
      expect(rgb(parseColor('var(--accent-fg)', vars))).toEqual(hexRgb(INK.light))
      const soft = /(\d+)%/.exec(vars.get('--accent-soft') ?? '')?.[1]
      const selected = /(\d+)%/.exec(vars.get('--fill-selected') ?? '')?.[1]
      expect([Number(soft) / 100, Number(selected) / 100]).toEqual(SOFT_MIX[theme])
    }
  })

  it('默认蓝色的解与 tokens-ui.css 的默认值一致（强调色为空串时删除内联值不会变色）', () => {
    for (const [theme, vars] of [
      ['light', LIGHT],
      ['dark', DARK],
    ] as const) {
      const t = deriveAccentTokens('', theme)
      expect(t.fg).toBe(INK.light)
      for (const [prop, value] of [
        ['--accent-fill', t.fill],
        ['--accent-fill-hover', t.fillHover],
        ['--accent-fill-pressed', t.fillPressed],
        ['--accent-text', t.text],
      ] as const) {
        const css = rgb(parseColor(`var(${prop})`, vars))
        const solved = hexRgb(value)
        for (let i = 0; i < 3; i++)
          expect(
            Math.abs((css[i] as number) - (solved[i] as number)),
            `${theme} ${prop}`,
          ).toBeLessThanOrEqual(1)
      }
    }
  })

  const cases = [
    ...PRESETS.flatMap((p) => [
      { label: `${p.name} 浅色`, theme: 'light' as const, accent: p.light },
      { label: `${p.name} 深色`, theme: 'dark' as const, accent: p.dark },
    ]),
    ...CUSTOM.flatMap((accent) => [
      { label: `${accent} 浅色`, theme: 'light' as const, accent },
      { label: `${accent} 深色`, theme: 'dark' as const, accent },
    ]),
  ]

  it.each(cases)('$label：填充三态的标签、强调色文字都 ≥ 4.5:1', ({ theme, accent }) => {
    const t = deriveAccentTokens(accent, theme)
    const ink = hexRgb(t.fg)
    for (const fill of [t.fill, t.fillHover, t.fillPressed])
      expect(contrastRgb(ink, hexRgb(fill)), fill).toBeGreaterThanOrEqual(MIN_CONTRAST)
    const base = hexRgb(accent)
    const solid = hexRgb(SURFACES[theme][0])
    const text = hexRgb(t.text)
    for (const bg of [
      ...SURFACES[theme].map(hexRgb),
      ...SOFT_MIX[theme].map((alpha) => overRgb(base, alpha, solid)),
    ])
      expect(contrastRgb(text, bg)).toBeGreaterThanOrEqual(MIN_CONTRAST)
  })

  it('白字优先：常规色相保持白字，只有很亮的强调色改用深色字', () => {
    const ink = (accent: string, theme: 'light' | 'dark') => deriveAccentTokens(accent, theme).fg
    for (const p of PRESETS.filter((x) =>
      ['blue', 'purple', 'pink', 'red', 'graphite'].includes(x.name),
    )) {
      expect(ink(p.light, 'light'), p.name).toBe(INK.light)
      expect(ink(p.dark, 'dark'), p.name).toBe(INK.light)
    }
    const yellow = PRESETS.find((p) => p.name === 'yellow')
    expect(ink(yellow?.light ?? '', 'light')).toBe(INK.dark)
    expect(ink(yellow?.dark ?? '', 'dark')).toBe(INK.dark)
    expect(ink('#FFFFFF', 'light')).toBe(INK.dark)
    expect(ink('#000000', 'dark')).toBe(INK.light)
    // 白字时深色主题的填充比强调色本身暗，且对白字 ≥ 4.5:1
    const blue = deriveAccentTokens('#60A5FA', 'dark')
    expect(contrastRgb(hexRgb(INK.light), hexRgb(blue.fill))).toBeGreaterThanOrEqual(MIN_CONTRAST)
    expect(rgbToOklch(hexRgb(blue.fill))[0]).toBeLessThan(rgbToOklch(hexRgb('#60A5FA'))[0])
  })

  it('深色主题的白字填充保住色度：OKLCH 色度不低于强调色的 80%，悬停 / 按下只调亮度（深色主按钮发灰）', () => {
    const darkCases = [...PRESETS.map((p) => p.dark), ...CUSTOM]
    let checked = 0
    for (const accent of darkCases) {
      const t = deriveAccentTokens(accent, 'dark')
      if (t.fg !== INK.light) continue
      const [, c0, h0] = rgbToOklch(hexRgb(accent))
      if (c0 < DARK_FILL.neutralChroma) continue
      checked++
      const [lf, cf, hf] = rgbToOklch(hexRgb(t.fill))
      expect(cf, `${accent} 填充 ${t.fill} 的色度`).toBeGreaterThanOrEqual(c0 * DARK_FILL.minChroma)
      // 色相不漂（色域边界处取整带来的误差以内）
      const dh = Math.abs(Math.atan2(Math.sin(hf - h0), Math.cos(hf - h0)))
      expect(dh, `${accent} 色相`).toBeLessThan(0.12)
      const [lh] = rgbToOklch(hexRgb(t.fillHover))
      const [lp] = rgbToOklch(hexRgb(t.fillPressed))
      expect(lh).toBeGreaterThan(lf)
      expect(lp).toBeLessThan(lf)
    }
    expect(checked).toBeGreaterThanOrEqual(6)
    // 默认蓝色：不是混黑得到的 rgb(63 109 165)（HSL 饱和度约 45%），而是饱和的中蓝
    const blue = hexRgb(deriveAccentTokens('#60A5FA', 'dark').fill)
    const max = Math.max(...blue) / 255
    const min = Math.min(...blue) / 255
    const sat = (max - min) / (1 - Math.abs(max + min - 1))
    expect(sat).toBeGreaterThan(0.75)
  })
})

describe('平台 / 材质覆盖', () => {
  const components = read('components.css')

  it('Linux 的 95% 着色不压过「降低透明度」/ 纯色材质', () => {
    expect(BASE_CSS).toContain("[data-os='linux']:not([data-material='solid']) {")
    expect(BASE_CSS).not.toMatch(/\[data-os='linux'\]\s*\{/)
  })

  it('窗口不能透明（shadow_margin = 0）时卡片是纯色直角，Css 卡片的描边是 1 px --hairline', () => {
    const opaque =
      /\.panel-frame\[data-opaque\] \.panel-card \{([^}]*)\}/.exec(components)?.[1] ?? ''
    expect(opaque).toContain('background: var(--surface-solid)')
    expect(opaque).toContain('border-radius: 0')
    const ring = /\.panel-card\[data-frame='css'\]::after \{([^}]*)\}/.exec(components)?.[1] ?? ''
    expect(ring).toContain('inset 0 0 0 1px var(--hairline)')
    // 投影里不再带 0.5 px 的描边
    for (const sel of ["[data-frame='css']", "[data-frame='css'][data-theme='dark']"])
      expect(block(BASE_CSS, sel).get('--shadow-popover')).not.toMatch(/0 0 0 0\.5px/)
  })

  it('窗口不能透明时页面底色是实色，且规则不分层（要压过 panel.html 内联的不分层透明底色）', () => {
    const base = read('base.css')
    const panelHtml = readRepo('panel.html')
    expect(panelHtml).toMatch(/html,\s*body\s*\{[^}]*background: transparent/)
    const at = base.indexOf('html[data-opaque] {')
    expect(at).toBeGreaterThan(-1)
    expect(base.slice(at)).toMatch(/^html\[data-opaque\] \{\s*background: var\(--surface-solid\);/)
    // 在所有 @layer 块之外：之前的花括号全部配平
    const before = base.slice(0, at).replace(/\/\*[\s\S]*?\*\//g, '')
    expect(before.split('{').length).toBe(before.split('}').length)
  })
})
