/**
 * 展示注册表与 Rust 取值的契约。`satisfies Record<EntityKind, …>` 只保证注册表与 types.ts 的联合类型一致：Rust 新增
 * 一种识别或变换而 types.ts 没跟上时，编译照样通过，运行时却找不到它的图标与文案。这里拿 Rust 从 `ALL` 生成的清单
 * （`shared/ipc/fixtures/consts/`）比对注册表的键集合，任何一边多或少都会失败。
 */
import { describe, expect, it } from 'vitest'
import rustEntityKinds from '../../ipc/fixtures/consts/EntityKinds.json'
import rustTransformIds from '../../ipc/fixtures/consts/TransformIds.json'
import { ENTITY_SPECS, TRANSFORM_SPECS } from '../specs'

describe('智能识别的展示注册表（Rust ↔ TS）', () => {
  it('ENTITY_SPECS 的键与 EntityKind::ALL 完全一致', () => {
    expect(Object.keys(ENTITY_SPECS).sort()).toEqual([...rustEntityKinds].sort())
  })

  it('TRANSFORM_SPECS 的键与 TransformId::ALL 完全一致', () => {
    expect(Object.keys(TRANSFORM_SPECS).sort()).toEqual([...rustTransformIds].sort())
  })
})
