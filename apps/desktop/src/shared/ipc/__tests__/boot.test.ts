import { describe, expect, it } from 'vitest'
import {
  applyAppearance,
  applyBootAttributes,
  applyDeveloperMode,
  boot,
  overlayIndexOf,
  pinnedIdOf,
  windowKindOf,
} from '../boot'

describe('boot', () => {
  it('label → 窗口种类 / pinned id / overlay 序号', () => {
    expect(windowKindOf('panel')).toBe('panel')
    expect(windowKindOf('settings')).toBe('settings')
    expect(windowKindOf('onboarding')).toBe('onboarding')
    expect(windowKindOf('pinned-3fa9')).toBe('pinned')
    expect(windowKindOf('overlay-1')).toBe('overlay')
    expect(pinnedIdOf('pinned-3fa9')).toBe('3fa9')
    expect(pinnedIdOf('panel')).toBeNull()
    expect(overlayIndexOf('overlay-2')).toBe(2)
    expect(overlayIndexOf('overlay-x')).toBeNull()
  })

  it('把 boot 写到 <html>（data-* / lang / --accent）', () => {
    applyBootAttributes({
      ...boot,
      label: 'pinned-1',
      os: 'windows',
      theme: 'dark',
      frame_mode: 'css',
      material: 'solid',
      reduce_motion: true,
      accent: '#ff6b6b',
      ui_language: 'en-US',
    })
    const root = document.documentElement
    expect(root.dataset).toMatchObject({
      os: 'windows',
      theme: 'dark',
      frame: 'css',
      material: 'solid',
      reducedMotion: 'true',
      window: 'pinned',
    })
    expect(root.lang).toBe('en-US')
    expect(root.style.getPropertyValue('--accent')).toBe('#ff6b6b')
  })

  it('阴影边距写到 CSS 变量 --shadow-margin（唯一来源是 Boot.shadow_margin）', () => {
    applyBootAttributes({ ...boot, frame_mode: 'css', shadow_margin: 24 })
    expect(document.documentElement.style.getPropertyValue('--shadow-margin')).toBe('24px')
    applyBootAttributes({ ...boot, frame_mode: 'native', shadow_margin: 0 })
    expect(document.documentElement.style.getPropertyValue('--shadow-margin')).toBe('0px')
  })

  it('开发者模式：dev = developer_mode || debug_build', () => {
    const saved = { dev: boot.dev, debug_build: boot.debug_build }
    boot.debug_build = false
    applyDeveloperMode(true)
    expect(boot.dev).toBe(true)
    applyDeveloperMode(false)
    expect(boot.dev).toBe(false)
    boot.debug_build = true
    applyDeveloperMode(false)
    expect(boot.dev).toBe(true)
    Object.assign(boot, saved)
  })

  it('appearance:changed 原地更新 boot', () => {
    applyAppearance({
      theme: 'light',
      accent: '#30a46c',
      reduce_motion: false,
      reduce_transparency: true,
      material: 'solid',
      font_size: 15,
      os: 'macos',
      ui_language: 'zh-CN',
    })
    expect(boot).toMatchObject({
      theme: 'light',
      accent: '#30a46c',
      font_size: 15,
      material: 'solid',
    })
    expect(document.documentElement.dataset.theme).toBe('light')
  })
})
