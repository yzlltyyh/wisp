/**
 * 引导「接入模型」的同步入口：卡片下面一条安静的链接，打开设置的「同步」分组，并跳过这一步（进入「试一试」）；
 * 打不开设置窗口时就地提示、停在这一步。
 */
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { getAppearance, setAppearance } from '../../shared/hooks/useAppearance'
import { boot } from '../../shared/ipc'
import { installHarness, ipcError } from '../../settings/__tests__/harness'
import { resetAdviceCache } from '../../settings/components/AdviceBanners'
import { resetWrites } from '../../settings/lib/writes'
import { resetTestRuns } from '../../settings/providers/hooks'
import { resetTemplatesCache } from '../../settings/providers/templates'
import { resetSettingsStore, useSettingsStore } from '../../settings/store'
import { OnboardingApp } from '../OnboardingApp'

const appearance = getAppearance()

function start() {
  setAppearance({ ...appearance, os: 'windows', uiLanguage: 'zh-CN' })
  boot.os = 'windows'
  boot.route = { kind: 'onboarding', step: 2 }
  return render(<OnboardingApp />)
}

beforeEach(() => {
  resetSettingsStore()
  resetTemplatesCache()
  resetTestRuns()
  resetAdviceCache()
  resetWrites()
})

afterEach(() => {
  boot.route = null
  boot.os = appearance.os
  setAppearance(appearance)
})

describe('引导 · 连接同步', () => {
  it('「已在其他设备上使用 Wisp？连接同步」打开设置的同步分组并跳过这一步', async () => {
    const h = installHarness()
    h.handle('provider_templates', () => [])
    start()
    await screen.findByTestId('onboarding-cards')
    const link = screen.getByTestId('onboarding-sync-link')
    expect(link).toHaveTextContent('已在其他设备上使用 Wisp？连接同步')
    await userEvent.click(link)
    await waitFor(() =>
      expect(h.callsOf('window_open')).toEqual([{ target: { kind: 'settings', section: 'sync' } }]),
    )
    expect(await screen.findByTestId('onboarding-try')).toBeInTheDocument()
  })

  it('打不开设置窗口：提示原因，停在这一步', async () => {
    const h = installHarness()
    h.handle('provider_templates', () => [])
    h.handle('window_open', () => {
      throw ipcError('internal', 'window failed')
    })
    start()
    await userEvent.click(await screen.findByTestId('onboarding-sync-link'))
    await waitFor(() => expect(useSettingsStore.getState().notice?.tone).toBe('danger'))
    expect(screen.getByTestId('onboarding-model')).toBeInTheDocument()
    expect(screen.queryByTestId('onboarding-try')).toBeNull()
  })
})
