/**
 * 润色 / 校对输出的切分：与 Rust split_edit_output 共用 crates/wisp-prompts/tests/fixtures/edit_split.json。
 */
import { describe, expect, it } from 'vitest'
import fixture from '../../../../../../crates/wisp-prompts/tests/fixtures/edit_split.json'
import { splitEditOutput, streamingSplit } from '../editSplit'

describe('splitEditOutput（共用 fixture）', () => {
  for (const c of fixture.cases) {
    it(`${c.name}`, () => {
      expect(splitEditOutput(c.input)).toEqual({ body: c.body, changes: c.changes })
    })
  }
})

describe('streamingSplit', () => {
  it('分隔线之后出现「改动」才切换', () => {
    expect(streamingSplit('正文\n---\n')).toBeNull()
    expect(streamingSplit('正文\n---\n改')).toBeNull()
    expect(streamingSplit('正文\n---\n改动：x')).toEqual({ body: '正文', changes: '改动：x' })
  })
})
