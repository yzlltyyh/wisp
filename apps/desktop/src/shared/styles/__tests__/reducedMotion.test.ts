/**
 * 减少动效的 CSS 静态检查（开启后过渡 ≤ 100 ms）：
 * - 全部 CSS 里的 `transition` / `transition-duration` / `transition-delay` 只能引用「减少动效时会被缩到 ≤ 100 ms」的时长令牌
 *   （按 motion/tokens.css 与 tokens-ui.css 里 `:root` 的取值与 `:root[data-reduced-motion='true']` 的覆盖计算），或者 ≤ 100 ms 的字面量；
 * - CSS 动画（animation）不在此列：base.css 在减少动效时把 animation-duration 一律置 0（这里同样核对那条规则还在）。
 * 新增时长令牌时，忘了在减少动效块里覆盖，这里就会失败。
 */
import { describe, expect, it } from 'vitest'

interface NodeFs {
  readFileSync(path: string, encoding: 'utf8'): string
  readdirSync(path: string, options: { recursive: true }): string[]
}
const proc = (
  globalThis as unknown as { process: { getBuiltinModule(name: 'node:fs'): NodeFs; cwd(): string } }
).process
const fs = proc.getBuiltinModule('node:fs')
const SRC = `${proc.cwd()}/src`
const read = (rel: string) => fs.readFileSync(`${SRC}/${rel}`, 'utf8')
const strip = (css: string) => css.replace(/\/\*[\s\S]*?\*\//g, '')

/** 毫秒；不是时长时返回 NaN */
function toMs(v: string): number {
  const m = /^(-?[\d.]+)(ms|s)$/.exec(v.trim())
  if (!m) return Number.NaN
  return Number(m[1]) * (m[2] === 's' ? 1000 : 1)
}

/** 取一个选择器块里的自定义属性（同名块按出现顺序合并） */
function vars(css: string, selector: string): Map<string, string> {
  const out = new Map<string, string>()
  const text = strip(css)
  let from = 0
  for (;;) {
    const at = text.indexOf(`${selector} {`, from)
    if (at < 0) break
    const body = text.slice(text.indexOf('{', at) + 1, text.indexOf('}', at))
    for (const m of body.matchAll(/(--[\w-]+)\s*:\s*([^;]+);/g))
      out.set(m[1] as string, (m[2] as string).trim())
    from = at + 1
  }
  return out
}

const MOTION = read('shared/motion/tokens.css')
const UI = read('shared/styles/tokens-ui.css')
const REDUCED = ":root[data-reduced-motion='true']"
const normal = new Map([...vars(MOTION, ':root'), ...vars(UI, ':root')])
const reduced = new Map([...normal, ...vars(MOTION, REDUCED), ...vars(UI, REDUCED)])

const cssFiles = fs
  .readdirSync(SRC, { recursive: true })
  .filter((f) => f.endsWith('.css') && !f.includes('node_modules'))

describe('减少动效：CSS 过渡 ≤ 100 ms', () => {
  it('时长令牌在减少动效时都 ≤ 100 ms', () => {
    const durations = [...normal.keys()].filter((k) => k.startsWith('--dur-'))
    expect(durations.length).toBeGreaterThan(5)
    for (const k of durations) {
      const ms = toMs(reduced.get(k) ?? '')
      expect.soft(ms, `${k} 减少动效时为 ${reduced.get(k)}`).toBeLessThanOrEqual(100)
    }
  })

  it('全部 CSS 的 transition 只引用会被缩短的时长令牌或 ≤ 100 ms 的字面量', () => {
    const bad: string[] = []
    let checked = 0
    for (const file of cssFiles) {
      const text = strip(read(file))
      for (const m of text.matchAll(
        /(?<![\w-])transition(?:-duration|-delay)?\s*:\s*([^;}]+)[;}]/g,
      )) {
        const value = m[1] as string
        checked++
        for (const v of value.matchAll(/var\((--[\w-]+)\)/g)) {
          const name = v[1] as string
          if (name.startsWith('--ease-')) continue
          const ms = toMs(reduced.get(name) ?? '')
          if (!(ms <= 100))
            bad.push(`${file}: ${name}（减少动效时 ${reduced.get(name) ?? '未定义'}）`)
        }
        for (const lit of value
          .replace(/var\([^)]*\)|cubic-bezier\([^)]*\)/g, '')
          .matchAll(/(?<![\w-])[\d.]+m?s\b/g)) {
          if (toMs(lit[0]) > 100) bad.push(`${file}: 字面量 ${lit[0]}`)
        }
      }
    }
    expect(checked).toBeGreaterThan(50)
    expect(bad).toEqual([])
  })

  it('CSS 动画在减少动效时一律归零（base.css）', () => {
    const base = strip(read('shared/styles/base.css'))
    expect(base).toMatch(
      /html\[data-reduced-motion='true'\] \*[\s\S]*?animation-duration: 0s !important/,
    )
  })
})
