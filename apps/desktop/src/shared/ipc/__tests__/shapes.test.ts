/**
 * shapes.ts 自身的测试：校验器能发现多键 / 少键 / 错误枚举，引用都能解析，
 * 并用 mock 的展示数据（同样受 types.ts 约束）做一次全量往返校验。
 */
import { describe, expect, it } from 'vitest'
import { createFixtures } from '../mock/fixtures'
import { registry, resolve, validate, type Spec } from './shapes'
import { boot } from '../boot'

describe('shapes 校验器', () => {
  it('所有类型引用都能解析', () => {
    const walk = (spec: Spec, seen: Set<Spec>): void => {
      if (seen.has(spec)) return
      seen.add(spec)
      switch (spec.t) {
        case 'ref':
          walk(spec.get(), seen)
          break
        case 'nullable':
        case 'optional':
        case 'array':
          walk(spec.of, seen)
          break
        case 'record':
          walk(spec.of, seen)
          if (spec.keys) walk(spec.keys, seen)
          break
        case 'object':
          for (const f of Object.values(spec.fields)) walk(f, seen)
          break
        case 'union':
          for (const v of Object.values(spec.variants))
            for (const f of Object.values(v)) walk(f, seen)
          break
        default:
      }
    }
    const seen = new Set<Spec>()
    for (const spec of Object.values(registry)) expect(() => walk(spec, seen)).not.toThrow()
  })

  it('发现多出、缺少的键与非法枚举值', () => {
    const r = validate(registry.PanelShown, { session_id: 1, focused: 'yes', extra: 1 })
    expect(r.errors.join('\n')).toMatch(/多出键 extra/)
    expect(r.errors.join('\n')).toMatch(/focused: 应为 boolean/)
    const u = validate(registry.PasteOutcome, { type: 'copied' })
    expect(u.errors.join('\n')).toMatch(/缺少键 reason/)
    const e = validate(registry.HotkeyTarget, { kind: 'action', action: 'nope' })
    expect(e.errors.join('\n')).toMatch(/HotkeyAction/)
  })

  it('可缺省的键：缺省合法，显式 null 只在允许时合法（StyleOverride 的双层 Option 语义）', () => {
    expect(validate(registry.StyleOverride, {}).errors).toEqual([])
    expect(
      validate(registry.StyleOverride, { temperature: null, hotkey: 'Alt+KeyN' }).errors,
    ).toEqual([])
    expect(validate(registry.StyleOverride, { output: null }).errors.join('\n')).toMatch(
      /OutputFormat/,
    )
    expect(validate(registry.StyleOverride, { extra: 1 }).errors.join('\n')).toMatch(/多出键 extra/)
  })

  it('mock 展示数据与契约一致', () => {
    const f = createFixtures(boot)
    const cases: Array<[keyof typeof registry, unknown]> = [
      ['Settings', f.defaultSettings()],
      ['RuntimeInfo', f.runtime()],
      ['AppMeta', f.appMeta()],
      ['AppearanceState', f.appearance()],
      ['PinState', f.pinState()],
      ['ClipPage', f.clipPage(f.clipItems())],
      ...f.styles().map((s) => ['Style', s] as [keyof typeof registry, unknown]),
      ...f
        .providerTemplates()
        .map((t) => ['ProviderTemplate', t] as [keyof typeof registry, unknown]),
      ...(['translate', 'ocr', 'clipboard', 'input', 'toast'] as const).map(
        (k) => ['PanelPayload', f.payload[k]()] as [keyof typeof registry, unknown],
      ),
    ]
    for (const [name, value] of cases) {
      expect(validate(registry[name], value).errors, name).toEqual([])
    }
    expect(resolve(registry.Settings).t).toBe('object')
  })
})
