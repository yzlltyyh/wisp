/**
 * 识别种类与「粘贴为…」变换的取值契约：Rust 从 `EntityKind::ALL` / `TransformId::ALL` 生成
 * `fixtures/consts/EntityKinds.json` / `TransformIds.json`（`node scripts/gen-fixtures.mjs`），这里比对 types.ts 的联合类型。
 * 联合类型在运行期读不到，借 shapes.ts 的 `enumOf` 表：它在编译期与联合类型逐项对齐，运行期的取值集合就代表 types.ts。
 * 黄金样例只覆盖一两个取值，新增的种类不会出现在样例里，所以要拿完整清单单独比对。
 */
import { describe, expect, it } from 'vitest'
import rustEntityKinds from '../fixtures/consts/EntityKinds.json'
import rustTransformIds from '../fixtures/consts/TransformIds.json'
import { registry, resolve, type Spec } from './shapes'

function enumValues(spec: Spec): string[] {
  const s = resolve(spec)
  if (s.t !== 'enum') throw new Error(`期望枚举，实际是 ${s.t}`)
  return [...s.values].sort()
}

describe('识别种类与变换的取值（Rust ↔ types.ts）', () => {
  it.each([
    ['EntityKind', rustEntityKinds],
    ['TransformId', rustTransformIds],
  ] as const)('%s 与 Rust 的 ALL 完全一致', (name, rust) => {
    expect(rust.length, '清单不应为空').toBeGreaterThan(0)
    expect(new Set(rust).size, '清单里不应有重复的取值').toBe(rust.length)
    expect(enumValues(registry[name])).toEqual([...rust].sort())
  })
})
