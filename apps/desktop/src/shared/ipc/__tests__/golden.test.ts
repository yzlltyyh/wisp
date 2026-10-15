/**
 * 黄金样例契约测试。
 *
 * `crates/wisp-types/tests/golden.rs` 把 IPC 与配置文档中每个类型的代表性样例写成
 * `fixtures/<Type>[.<variant>].json`（`UPDATE_GOLDEN=1` 时重写），并同时生成 `fixtures/index.ts`
 * （`satisfies` 做编译期检查）。这里在运行期逐个校验：
 * 1. 每个样例都能对应到 shapes.ts 里的类型，键集合完全一致（不多不少），枚举值合法；
 * 2. IPC 与配置文档的每个对象 / 联合类型至少有一个样例，联合类型的每个变体都出现过；
 * 3. `fixtures/index.ts` 的导出与 JSON 样例一一对应（防止只更新了一边）。
 *
 * `fixtures/desktop/*.json` 是 desktop crate（`src-tauri`）自己的黄金样例（目前是 `IpcError`，由
 * `cargo test -p wisp-desktop --lib error::` 生成 / 校验），参与 1、2 两项检查，不进入 index.ts。
 */
import { describe, expect, it } from 'vitest'
import { registry, resolve, RUST_INTERNAL, validate, type Spec, type TypeName } from './shapes'

const jsonFiles = import.meta.glob<unknown>('../fixtures/*.json', {
  eager: true,
  import: 'default',
})
const desktopJsonFiles = import.meta.glob<unknown>('../fixtures/desktop/*.json', {
  eager: true,
  import: 'default',
})
const indexModules = import.meta.glob<Record<string, unknown>>('../fixtures/index.ts', {
  eager: true,
})

function toFixtures(files: Record<string, unknown>, prefix: string) {
  return Object.entries(files)
    .map(([path, value]) => {
      const name = path.slice(path.lastIndexOf('/') + 1, -'.json'.length)
      const [type = '', ...rest] = name.split('.')
      return { file: `${prefix}${name}`, type, variant: rest.join('.') || null, value }
    })
    .sort((a, b) => a.file.localeCompare(b.file))
}

/** wisp-types 生成的样例（与 index.ts 一一对应） */
const fixtures = toFixtures(jsonFiles, '')
/** wisp-types + desktop 的全部样例（键集合与覆盖率检查） */
const allFixtures = [...fixtures, ...toFixtures(desktopJsonFiles, 'desktop/')]

const NOT_GENERATED =
  '黄金样例尚未生成：在仓库根目录运行 `UPDATE_GOLDEN=1 cargo test -p wisp-types --test golden`'

describe('golden fixtures（Rust ↔ TS 契约）', () => {
  it('已由 wisp-types 生成', () => {
    expect(fixtures.length, NOT_GENERATED).toBeGreaterThan(0)
  })

  it('desktop 的样例已生成（IpcError）', () => {
    expect(
      allFixtures.some((f) => f.file === 'desktop/IpcError'),
      '运行 `UPDATE_GOLDEN=1 cargo test -p wisp-desktop --lib error::` 生成 fixtures/desktop/IpcError.json',
    ).toBe(true)
  })

  it.each(allFixtures.map((f) => [f.file, f] as const))('%s 与 TS 类型的键集合完全一致', (_, f) => {
    const spec: Spec | undefined = (registry as Record<string, Spec>)[f.type]
    expect(
      spec,
      `fixtures/${f.file}.json：TS 侧没有类型 ${f.type}（types.ts / shapes.ts 需要补齐）`,
    ).toBeDefined()
    if (!spec) return
    const target = resolve(spec)
    const values = Array.isArray(f.value) && target.t !== 'array' ? f.value : [f.value]
    const result = { errors: [] as string[], variants: new Set<string>() }
    values.forEach((v, i) => validate(spec, v, values.length > 1 ? `$[${i}]` : '$', result))
    expect(result.errors).toEqual([])
    // 文件名里的变体后缀与标签值一致（后缀不是标签值时不检查，例如 Settings.default）
    const tagMismatch =
      f.variant && target.t === 'union' && f.variant in target.variants
        ? values
            .map((v) => (v as Record<string, unknown>)[target.tag])
            .filter((tag) => tag !== f.variant)
        : []
    expect(tagMismatch, `${f.file}：文件名后缀与标签值不一致`).toEqual([])
  })

  it('IPC 与配置文档的每个对象与联合类型都有样例，联合类型覆盖全部变体', () => {
    if (fixtures.length === 0) return // 第一条用例已报告未生成
    const present = new Set(allFixtures.map((f) => f.type))
    const seen = new Set<string>()
    for (const f of allFixtures) {
      const spec = (registry as Record<string, Spec>)[f.type]
      if (!spec) continue
      const values = Array.isArray(f.value) && resolve(spec).t !== 'array' ? f.value : [f.value]
      for (const v of values) for (const name of validate(spec, v).variants) seen.add(name)
    }
    const missingTypes: string[] = []
    const missingVariants: string[] = []
    for (const [name, spec] of Object.entries(registry) as [TypeName, Spec][]) {
      if (spec.t !== 'object' && spec.t !== 'union') continue
      if (!present.has(name) && !RUST_INTERNAL.has(name)) missingTypes.push(name)
      if (spec.t === 'union') {
        for (const v of Object.keys(spec.variants)) {
          if (!seen.has(`${spec.name}.${v}`)) missingVariants.push(`${spec.name}.${v}`)
        }
      }
    }
    expect({ missingTypes, missingVariants }).toEqual({ missingTypes: [], missingVariants: [] })
  })

  it('fixtures/index.ts 与 JSON 样例一一对应', () => {
    if (fixtures.length === 0) return
    const mod = Object.values(indexModules)[0]
    expect(mod, 'fixtures/index.ts 不存在（golden.rs 应同时生成它）').toBeDefined()
    if (!mod) return
    const exported = Object.values(mod).map((v) => JSON.stringify(canonical(v)))
    const json = fixtures.map((f) => JSON.stringify(canonical(f.value)))
    expect(exported.length, 'index.ts 导出数量与 JSON 样例数量不一致').toBe(json.length)
    const pool = new Map<string, number>()
    for (const s of exported) pool.set(s, (pool.get(s) ?? 0) + 1)
    const unmatched = fixtures
      .filter((_, i) => {
        const key = json[i] as string
        const n = pool.get(key) ?? 0
        if (n === 0) return true
        pool.set(key, n - 1)
        return false
      })
      .map((f) => f.file)
    expect(unmatched, '这些 JSON 样例在 index.ts 中没有逐值相同的导出').toEqual([])
  })
})

/** 键排序后的规范形式（比较时忽略键顺序） */
function canonical(v: unknown): unknown {
  if (Array.isArray(v)) return v.map(canonical)
  if (v && typeof v === 'object') {
    return Object.fromEntries(
      Object.keys(v)
        .sort()
        .map((k) => [k, canonical((v as Record<string, unknown>)[k])]),
    )
  }
  return v
}
