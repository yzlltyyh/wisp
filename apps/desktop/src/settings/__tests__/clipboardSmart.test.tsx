/**
 * 设置 › 剪贴板 › 智能识别：四类片段与验证码自动删除各一个开关，默认值来自 Rust 的 `Settings::default()`（证件与
 * 银行卡号默认关闭），切换时只写 `clipboard.smart` 下的那一项。
 */
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { getAppearance, setAppearance } from '../../shared/hooks/useAppearance'
import { boot } from '../../shared/ipc'
import { resetWrites } from '../lib/writes'
import { SettingsApp } from '../SettingsApp'
import { resetSettingsStore, setQueueDelay } from '../store'
import { installHarness } from './harness'

const appearance = getAppearance()

beforeEach(() => {
  resetSettingsStore()
  resetWrites()
  setQueueDelay(null)
  setAppearance({ ...appearance, os: 'macos', uiLanguage: 'zh-CN' })
})

afterEach(() => {
  setAppearance(appearance)
  boot.route = null
})

describe('剪贴板 › 智能识别', () => {
  it('默认：联系方式、验证码、单号与自动删除开；证件与银行卡号关（说明里提示敏感）', async () => {
    installHarness()
    boot.route = { kind: 'settings', section: 'clipboard' }
    render(<SettingsApp />)
    await screen.findByTestId('clip-smart-identity-switch')
    const sw = (key: string) => screen.getByTestId(`clip-smart-${key}-switch`)
    for (const key of ['contacts', 'codes', 'numbers', 'otp_auto_delete']) {
      expect(sw(key), key).toBeChecked()
    }
    expect(sw('identity')).not.toBeChecked()
    expect(screen.getByTestId('clip-smart-identity')).toHaveTextContent('敏感')
  })

  it('切换只写 clipboard.smart 下的那一项', async () => {
    const h = installHarness()
    boot.route = { kind: 'settings', section: 'clipboard' }
    render(<SettingsApp />)
    await userEvent.click(await screen.findByTestId('clip-smart-identity-switch'))
    await waitFor(() => expect(h.callsOf('settings_patch')).toHaveLength(1))
    expect(h.callsOf('settings_patch')[0]?.patch).toEqual({
      clipboard: { smart: { identity: true } },
    })
    await userEvent.click(screen.getByTestId('clip-smart-otp_auto_delete-switch'))
    await waitFor(() => expect(h.callsOf('settings_patch')).toHaveLength(2))
    expect(h.callsOf('settings_patch')[1]?.patch).toEqual({
      clipboard: { smart: { otp_auto_delete: false } },
    })
  })
})
