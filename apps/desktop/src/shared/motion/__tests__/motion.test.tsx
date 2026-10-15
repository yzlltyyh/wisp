/**
 * 动效：
 * - 预设在「完整 / 减少动效」两种模式下的结果做快照；
 * - 关键组件在两种模式下的首帧样式快照：减少动效时没有位移、缩放、模糊；
 * - 「没有动效魔法数」：共享层与面板外壳的代码里，时长 / 曲线 / 弹簧参数只出现在 tokens.ts / tokens.css（check-contract 的规则）。
 */
import { render } from '@testing-library/react'
import { afterEach, describe, expect, it } from 'vitest'
import { getAppearance, setAppearance } from '../../hooks/useAppearance'
import type { LlmError } from '../../ipc/types'
import { Banner, CopyButton, EmptyState, ErrorCard } from '../../ui'
import { MotionRoot } from '../MotionRoot'
import { adaptPreset, presets, reducedPolicy, type PresetName } from '../presets'
import { duration, ease, spring } from '../tokens'

const original = getAppearance()
afterEach(() => setAppearance(original))

describe('令牌', () => {
  it('数值与动效规范一致', () => {
    expect(duration).toEqual({ instant: 0.08, fast: 0.14, base: 0.22, slow: 0.32 })
    expect(ease).toEqual({
      standard: [0.2, 0, 0, 1],
      exit: [0.4, 0, 1, 1],
      pulse: [0.42, 0, 0.58, 1],
    })
    expect(spring.snappy).toMatchObject({ stiffness: 520, damping: 38, mass: 1 })
    expect(spring.gentle).toMatchObject({ stiffness: 260, damping: 30, mass: 1 })
  })

  it('panelEnter：Css = opacity / scale 0.96 / y ±6，原点在锚点一侧；Native 只动内容 y', () => {
    expect(presets.panelEnter('below', 40, 'css')).toMatchObject({
      initial: { opacity: 0, scale: 0.96, y: 6 },
      transformOrigin: '40px top',
    })
    expect(presets.panelEnter('above', 40, 'css')).toMatchObject({
      initial: { y: -6 },
      transformOrigin: '40px bottom',
    })
    expect(presets.panelEnter('center', 40, 'css').transformOrigin).toBe('center')
    expect(presets.panelEnter('below', 40, 'native').initial).toEqual({ y: 6 })
  })

  it('listStagger：最后一行在 84 ms 开始', () => {
    const { staggerChildren, maxItems } = presets.listStagger
    expect(Math.round(staggerChildren * (maxItems - 1) * 1000)).toBe(84)
  })
})

describe('减少动效', () => {
  const names = Object.keys(reducedPolicy) as PresetName[]
  const reducedOf = (name: PresetName) => adaptPreset(presets[name], reducedPolicy[name], true)

  for (const name of names) {
    it(`${name}：完整 / 减少两种模式`, () => {
      const full = adaptPreset(presets[name], reducedPolicy[name], false)
      expect({ full, reduced: reducedOf(name) }).toMatchSnapshot()
    })
  }

  it('「直接出现」的预设：没有初始状态、过渡时长为 0', () => {
    for (const name of names.filter((n) => reducedPolicy[n] === 'none')) {
      expect(reducedOf(name).initial, name).toBe(false)
      expect(reducedOf(name).transition, name).toEqual({ duration: 0 })
    }
  })

  it('「只淡入淡出」的预设：没有位移、缩放、模糊', () => {
    for (const name of names.filter((n) => reducedPolicy[n] !== 'none')) {
      expect(JSON.stringify(reducedOf(name)), name).not.toMatch(/"(x|y|scale|filter)":/)
    }
  })

  const error: LlmError = {
    code: 'upstream',
    subcode: null,
    message: 'Bad gateway',
    retryable: true,
    hint: null,
    status: 502,
    url: null,
    retry_after_s: null,
    upstream_request_id: null,
  }

  function snapshotOf(reduced: boolean) {
    setAppearance({ ...original, reduceMotion: reduced })
    const { container, unmount } = render(
      <MotionRoot>
        <ErrorCard error={error} onRetry={() => {}} />
        <Banner tone="paused">paused</Banner>
        <EmptyState title="empty" />
        <CopyButton text="x" />
      </MotionRoot>,
    )
    const styles = [...container.querySelectorAll<HTMLElement>('[style]')].map((el) => ({
      cls: el.className,
      style: el.getAttribute('style'),
    }))
    unmount()
    return styles
  }

  it('关键组件的首帧样式：完整模式有 y 位移，减少动效时直接到位', () => {
    const full = snapshotOf(false)
    const reduced = snapshotOf(true)
    expect({ full, reduced }).toMatchSnapshot()
    expect(full.some((s) => /translateY/.test(s.style ?? ''))).toBe(true)
    expect(reduced.some((s) => /translateY\((?!0)/.test(s.style ?? ''))).toBe(false)
  })
})

// ---------------------------------------------------------------------------
// 动效魔法数（共享层与面板外壳）

const OWNED = import.meta.glob<string>(
  [
    '/src/shared/{styles,motion,ui,keys,i18n,markdown,stream,perf,hooks}/**/*.{ts,tsx}',
    '/src/panel/shell/**/*.{ts,tsx}',
    '!/src/**/__tests__/**',
  ],
  { query: '?raw', import: 'default', eager: true },
)

/** 允许出现字面量的唯一来源 */
const SOURCE_OF_TRUTH = '/src/shared/motion/tokens.ts'

const RULES: Array<[string, RegExp]> = [
  ['时长字面量', /\bduration:\s*(?!0\b)\d/],
  ['延迟字面量', /\bdelay:\s*\d/],
  ['弹簧参数', /\b(stiffness|damping):\s*\d/],
  ['缓动曲线', /\bease:\s*\[|cubic-bezier\(/],
  ['CSS 时长', /\b(transition|animation)[^;'"]*\b\d+m?s\b/],
  ['计时器字面量', /setTimeout\([^)]*,\s*\d{2,}\s*\)/],
]

describe('没有动效魔法数（check-contract 规则）', () => {
  it('共享层与面板外壳的代码只引用 shared/motion/tokens 或 --dur-* / --ease-*', () => {
    const hits: string[] = []
    for (const [file, text] of Object.entries(OWNED)) {
      if (file === SOURCE_OF_TRUTH) continue
      text.split('\n').forEach((line, i) => {
        if (/^\s*(\*|\/\/)/.test(line)) return
        for (const [rule, re] of RULES)
          if (re.test(line)) hits.push(`${file}:${i + 1} ${rule}: ${line.trim()}`)
      })
    }
    expect(hits).toEqual([])
    expect(Object.keys(OWNED).length).toBeGreaterThan(40)
  })
})
