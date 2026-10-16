/**
 * 服务商标志：每个模板（中转站除外）都登记了标志、登记表里没有多余的 id；ProviderLogo 的退路（没有模板 / 未知模板 →
 * 两字母头像，中转站 → 中性图标）；渐变 id 在同一页的多个相同标志之间不冲突；深色主题换色的标记。
 * 不比较路径数据本身（那是素材，不是逻辑）。
 */
import { render } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import {
  brandLogo,
  MONOGRAM_TEMPLATES,
  PROVIDER_LOGOS,
  ProviderLogo,
  RELAY_TEMPLATE,
  type BrandLogo,
} from '../logos'

/** crates/wisp-llm/src/templates.rs 里的模板 id（模板表的唯一来源） */
function rustTemplateIds(): string[] {
  const proc = (
    globalThis as unknown as {
      process: {
        getBuiltinModule(name: 'node:fs'): { readFileSync(p: string, e: 'utf8'): string }
        cwd(): string
      }
    }
  ).process
  const fs = proc.getBuiltinModule('node:fs')
  const rust = fs.readFileSync(`${proc.cwd()}/../../crates/wisp-llm/src/templates.rs`, 'utf8')
  const specs = /const SPECS: \[Spec; \d+\] = \[([^]*?)\n\];/.exec(rust)?.[1] ?? ''
  return [...specs.matchAll(/^\s+id: "([^"]+)",$/gm)].map((m) => m[1] as string)
}

describe('标志登记表', () => {
  it('除中转站与用两字母头像的模板外，每个模板都有标志，登记表里也没有模板表之外的 id', () => {
    const ids = rustTemplateIds()
    expect(ids.length).toBeGreaterThanOrEqual(16)
    expect(ids).toContain(RELAY_TEMPLATE)
    for (const id of MONOGRAM_TEMPLATES) expect(ids).toContain(id)
    const branded = ids.filter((id) => id !== RELAY_TEMPLATE && !MONOGRAM_TEMPLATES.includes(id))
    expect(Object.keys(PROVIDER_LOGOS).toSorted()).toEqual(branded.toSorted())
    for (const id of branded) expect(brandLogo(id), id).not.toBeNull()
    expect(brandLogo(RELAY_TEMPLATE)).toBeNull()
  })

  it('每段路径都有数据；渐变的序号都指向存在的渐变', () => {
    for (const [id, logo] of Object.entries(PROVIDER_LOGOS) as Array<[string, BrandLogo]>) {
      const gradients = logo.gradients?.length ?? 0
      for (const p of logo.paths) {
        expect(p.d.length, id).toBeGreaterThan(0)
        // 没有渐变的路径按 -1 计（总小于渐变数）
        expect(p.gradient ?? -1, id).toBeLessThan(gradients)
      }
    }
  })

  it('不在登记表里的 id（含原型链上的属性名）都没有标志', () => {
    expect(brandLogo(null)).toBeNull()
    expect(brandLogo(undefined)).toBeNull()
    expect(brandLogo('')).toBeNull()
    expect(brandLogo('custom-3f9a')).toBeNull()
    expect(brandLogo('toString')).toBeNull()
  })
})

describe('ProviderLogo', () => {
  it('有标志：画品牌标志，放在中性底上，读屏不念，不带字母', () => {
    const { container } = render(<ProviderLogo templateId="deepseek" name="DeepSeek" />)
    const tile = container.querySelector('.st-logo')
    expect(tile).toHaveAttribute('data-brand', 'deepseek')
    expect(tile).toHaveAttribute('data-tone', 'neutral')
    expect(tile).toHaveAttribute('aria-hidden', 'true')
    expect(tile?.textContent).toBe('')
    const svg = tile?.querySelector('svg.st-logo-mark')
    expect(svg).toHaveAttribute('width', '16')
    // 深色主题下换色的路径带标记与颜色变量
    const dark = svg?.querySelector('[data-dk]') as SVGElement | null
    expect(dark?.style.getPropertyValue('--dk')).toMatch(/^#[0-9A-F]{6}$/i)
  })

  it('没有模板、模板没有标志时退回两字母头像（按 tone 着色）', () => {
    const { container, rerender } = render(<ProviderLogo templateId={null} name="My Gateway" />)
    let tile = container.querySelector('.st-logo')
    expect(tile).toHaveTextContent('MG')
    expect(tile).toHaveAttribute('data-tone', 'accent')
    expect(tile).not.toHaveAttribute('data-brand')
    expect(tile?.querySelector('svg')).toBeNull()
    rerender(<ProviderLogo templateId="custom-3f9a" name="通义千问（百炼）" tone="neutral" />)
    tile = container.querySelector('.st-logo')
    expect(tile).toHaveTextContent('通')
    expect(tile).toHaveAttribute('data-tone', 'neutral')
  })

  it('中转站：中性图标，不是品牌标志也不是字母', () => {
    const { container } = render(
      <ProviderLogo templateId={RELAY_TEMPLATE} name="中转站" size={20} />,
    )
    const tile = container.querySelector('.st-logo')
    expect(tile).not.toHaveAttribute('data-brand')
    expect(tile).toHaveAttribute('data-tone', 'neutral')
    expect(tile?.textContent).toBe('')
    expect(tile?.querySelector('svg')).toHaveAttribute('width', '20')
    expect(tile?.querySelector('.st-logo-mark')).toBeNull()
  })

  it('同一页两个相同的渐变标志：渐变 id 不冲突，每个引用都指向自己 svg 里的渐变', () => {
    const { container } = render(
      <>
        <ProviderLogo templateId="gemini" name="Google Gemini" />
        <ProviderLogo templateId="gemini" name="Google Gemini" size={20} />
      </>,
    )
    const svgs = [...container.querySelectorAll('svg.st-logo-mark')]
    expect(svgs).toHaveLength(2)
    const ids = svgs.flatMap((svg) => [...svg.querySelectorAll('linearGradient')].map((g) => g.id))
    expect(new Set(ids).size).toBe(ids.length)
    for (const svg of svgs) {
      const refs = [...svg.querySelectorAll('path')]
        .map((p) => /^url\(#(.+)\)$/.exec(p.getAttribute('fill') ?? '')?.[1])
        .filter((x): x is string => x !== undefined)
      expect(refs.length).toBeGreaterThan(0)
      for (const ref of refs) expect(svg.querySelector(`[id="${ref}"]`)).not.toBeNull()
    }
  })
})
