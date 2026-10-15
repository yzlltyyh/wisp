/**
 * 设置分组 id 的契约：TS 的 SETTINGS_SECTIONS 与 Rust `app::sections::ALL` 逐项一致（含顺序）；
 * SETTINGS_ANCHORS 与 `app::sections::ANCHORS` 一致。样例由 `UPDATE_GOLDEN=1 cargo test -p wisp-desktop --lib app::` 生成。
 */
import { describe, expect, it } from 'vitest'
import rustAnchors from '../fixtures/consts/SettingsAnchors.json'
import rust from '../fixtures/consts/SettingsSections.json'
import {
  isSettingsAnchor,
  isSettingsSection,
  SETTINGS_ANCHORS,
  SETTINGS_SECTIONS,
} from '../sections'

describe('SETTINGS_SECTIONS（Rust ↔ TS）', () => {
  it('与 app::sections::ALL 逐项一致', () => {
    expect([...SETTINGS_SECTIONS]).toEqual(rust)
  })

  it('SETTINGS_ANCHORS 与 app::sections::ANCHORS 逐项一致', () => {
    expect(SETTINGS_ANCHORS).toEqual(rustAnchors)
    expect(isSettingsAnchor('ocr', 'ocr_history')).toBe(true)
    expect(isSettingsAnchor('translate', 'ocr_history')).toBe(false)
    expect(isSettingsAnchor('ocr', null)).toBe(false)
  })

  it('isSettingsSection 只认已知分组', () => {
    expect(isSettingsSection('providers')).toBe(true)
    expect(isSettingsSection('provider')).toBe(false)
    expect(isSettingsSection(null)).toBe(false)
  })
})
