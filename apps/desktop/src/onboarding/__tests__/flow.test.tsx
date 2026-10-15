/**
 * 引导窗口的流程细节：读取失败、步骤条只按真实状态打勾、试一试缺模型 / 权限的提示、换步骤后焦点与 Return 默认按钮、
 * 接入模型只有一个返回入口且草稿不丢、重新测试不改掉选好的默认模型、转发的划词翻译没有选中文字、重启后回到权限页。
 */
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { getAppearance, setAppearance } from '../../shared/hooks/useAppearance'
import { boot } from '../../shared/ipc'
import type { ProviderTemplate } from '../../shared/ipc/types'
import { entry, installHarness, provider, snapshot } from '../../settings/__tests__/harness'
import { resetAdviceCache } from '../../settings/components/AdviceBanners'
import { resetWrites } from '../../settings/lib/writes'
import { resetTestRuns } from '../../settings/providers/hooks'
import { resetTemplatesCache } from '../../settings/providers/templates'
import { resetSettingsStore } from '../../settings/store'
import { OnboardingApp } from '../OnboardingApp'

const appearance = getAppearance()

const TEMPLATES: ProviderTemplate[] = [
  'openai',
  'openai-responses',
  'anthropic',
  'gemini',
  'deepseek',
].map((id) => ({
  id,
  name_key: `settings.providers.template.${id}`,
  group: id.startsWith('openai')
    ? 'openai'
    : id === 'deepseek'
      ? 'other'
      : (id as 'anthropic' | 'gemini'),
  provider: provider(id, {
    protocol: id === 'openai-responses' ? 'openai_responses' : 'openai_chat',
    base_url: `https://api.${id}.com/v1`,
  }),
  key_required: true,
  region_variants: [],
}))

const okReport = {
  models: { ok: true, status: 200, elapsed_ms: 100, url: 'u', error: null },
  stream: { ok: true, status: 200, elapsed_ms: 200, url: 'u', error: null },
  overall: 'ok',
  model_used: 'a',
  fetched_models: [entry('model-a', 'yes'), entry('model-b', 'yes')],
}

function start(step: number | null, os: 'macos' | 'windows' = 'macos') {
  setAppearance({ ...appearance, os, uiLanguage: 'zh-CN' })
  boot.os = os
  boot.route = step === null ? null : { kind: 'onboarding', step }
  return render(<OnboardingApp />)
}

function pendingPermissions() {
  const s = snapshot()
  s.runtime.permissions = {
    accessibility: 'not_determined',
    screen_recording: 'not_determined',
    input_monitoring: 'not_determined',
    lost_after_update: false,
  }
  return s
}

beforeEach(() => {
  resetSettingsStore()
  resetTemplatesCache()
  resetTestRuns()
  resetAdviceCache()
  resetWrites()
  localStorage.clear()
})

afterEach(() => {
  boot.route = null
  boot.os = appearance.os
  setAppearance(appearance)
})

describe('读取失败', () => {
  it('settings_get 失败：说明 + 重试 + 稍后设置，不会一直停在骨架上', async () => {
    const h = installHarness()
    let fail = true
    h.handle('settings_get', () => {
      if (fail) throw { code: 'io', message: 'errors.ipc.io', llm: null }
      return snapshot()
    })
    start(null)
    expect(await screen.findByTestId('onboarding-load-error')).toBeInTheDocument()
    fail = false
    await userEvent.click(screen.getByTestId('onboarding-retry'))
    expect(await screen.findByTestId('onboarding-welcome')).toBeInTheDocument()
  })

  it('读取失败时「稍后设置」记为完成并关窗', async () => {
    const h = installHarness()
    h.handle('settings_get', () => {
      throw { code: 'io', message: 'errors.ipc.io', llm: null }
    })
    start(null)
    await userEvent.click(await screen.findByTestId('onboarding-load-error-skip'))
    await waitFor(() => expect(h.close).toHaveBeenCalled())
    expect(h.callsOf('onboarding_complete')).toHaveLength(1)
  })
})

describe('步骤条与「试一试」的前置提示', () => {
  it('两项权限都没授权、也没有模型时跳到「试一试」：前两步是「已跳过」而不是对勾；任务上方提示并能回去', async () => {
    installHarness(pendingPermissions())
    start(1)
    await screen.findByTestId('onboarding-permissions')
    await userEvent.click(screen.getByTestId('onboarding-step-try'))
    expect(await screen.findByTestId('onboarding-try')).toBeInTheDocument()
    expect(screen.getByTestId('onboarding-step-permissions')).toHaveAttribute(
      'data-state',
      'skipped',
    )
    expect(screen.getByTestId('onboarding-step-model')).toHaveAttribute('data-state', 'skipped')
    expect(screen.getByTestId('onboarding-step-permissions')).toHaveTextContent('（已跳过）')
    expect(screen.getByTestId('try-no-model')).toHaveTextContent('还没接入模型')
    expect(screen.getByTestId('try-no-permission')).toBeInTheDocument()
    await userEvent.click(within(screen.getByTestId('try-no-model')).getByRole('button'))
    expect(await screen.findByTestId('onboarding-model')).toBeInTheDocument()
  })

  it('真的完成了才打勾：权限都已授权、已接入模型', async () => {
    const s = snapshot({
      providers: [provider('openai', { name: 'OpenAI', models: [entry('gpt-mini')] })],
      defaults: { translate: { provider_id: 'openai', model_id: 'gpt-mini' }, ocr: null },
    })
    s.runtime.permissions = {
      accessibility: 'granted',
      screen_recording: 'granted',
      input_monitoring: 'not_determined',
      lost_after_update: false,
    }
    installHarness(s)
    start(3)
    await screen.findByTestId('onboarding-try')
    expect(screen.getByTestId('onboarding-step-permissions')).toHaveAttribute('data-state', 'done')
    expect(screen.getByTestId('onboarding-step-model')).toHaveAttribute('data-state', 'done')
    expect(screen.getByTestId('onboarding-step-model')).toHaveTextContent('（已完成）')
    expect(screen.queryByTestId('try-no-model')).toBeNull()
    expect(screen.queryByTestId('try-no-permission')).toBeNull()
  })
})

describe('键盘', () => {
  it('欢迎页按 Return 触发「开始设置」；换步骤后焦点在新步骤的标题上（不会掉到 body）', async () => {
    installHarness(pendingPermissions())
    start(null)
    await screen.findByTestId('onboarding-welcome')
    expect(document.activeElement).toBe(document.body)
    fireEvent.keyDown(window, { key: 'Enter', code: 'Enter' })
    expect(await screen.findByTestId('onboarding-permissions')).toBeInTheDocument()
    await waitFor(() => expect(document.activeElement).toHaveTextContent('授予两项权限'))
    expect(document.activeElement).toHaveAttribute('data-step-title')
    // 焦点在按钮上时 Return 交给按钮自己（不会再触发默认按钮跳步）
    const skip = screen.getByTestId('onboarding-next')
    skip.focus()
    fireEvent.keyDown(window, { key: 'Enter', code: 'Enter' })
    expect(screen.getByTestId('onboarding-permissions')).toBeInTheDocument()
  })
})

describe('接入模型', () => {
  function modelHarness() {
    const h = installHarness()
    h.handle('provider_templates', () => TEMPLATES)
    h.handle('provider_new', ({ template_id }) => ({
      ...TEMPLATES.find((t) => t.id === template_id)?.provider,
      id: String(template_id),
    }))
    h.handle('provider_test', () => okReport)
    h.handle('provider_models', () => [entry('model-a', 'yes'), entry('model-b', 'yes')])
    return h
  }

  it('只有一个返回入口：表单阶段底栏的「上一步」回到卡片；再点同一张卡片回到填了一半的草稿', async () => {
    modelHarness()
    start(2, 'windows')
    await userEvent.click(await screen.findByTestId('card-anthropic'))
    await userEvent.type(await screen.findByTestId('key-input'), 'sk-half')
    expect(screen.queryByTestId('onboarding-phase-back')).toBeNull()
    // Windows：接入模型是第一步，但表单阶段照样有「上一步」（回到卡片）
    await userEvent.click(screen.getByTestId('onboarding-back'))
    expect(await screen.findByTestId('onboarding-cards')).toBeInTheDocument()
    expect(screen.queryByTestId('onboarding-back')).toBeNull()
    await userEvent.click(screen.getByTestId('card-anthropic'))
    expect(await screen.findByTestId('key-input')).toHaveValue('sk-half')
  })

  it('中转站 / 自定义兼容端点（Base URL 空着）：焦点先给 Base URL；预填了地址的模板焦点给 Key', async () => {
    const h = installHarness()
    const custom: ProviderTemplate = {
      id: 'custom',
      name_key: 'settings.providers.template.custom',
      group: 'other',
      provider: provider('custom', { base_url: '' }),
      key_required: true,
      region_variants: [],
    }
    h.handle('provider_templates', () => [...TEMPLATES, custom])
    h.handle('provider_new', ({ template_id }) => ({
      ...[...TEMPLATES, custom].find((t) => t.id === template_id)?.provider,
      id: String(template_id),
    }))
    start(2)
    await userEvent.click(await screen.findByTestId('card-others'))
    await userEvent.click(await screen.findByTestId('template-custom'))
    const base = await screen.findByTestId('onboarding-base-url')
    await waitFor(() => expect(document.activeElement).toBe(base))
    expect(document.activeElement).not.toBe(screen.getByTestId('key-input'))
  })

  it('选了 B 之后点「重新测试」：B 还是选中的（不被预选值覆盖），连接没变时不重新拉取', async () => {
    const h = modelHarness()
    start(2)
    await userEvent.click(await screen.findByTestId('card-anthropic'))
    await userEvent.type(await screen.findByTestId('key-input'), 'sk-1')
    await userEvent.click(screen.getByTestId('onboarding-test'))
    await screen.findByTestId('onboarding-defaults')
    expect(screen.getByTestId('onboarding-defaults-translate-select')).toHaveTextContent('model-a')
    await userEvent.click(screen.getByTestId('onboarding-defaults-translate-select'))
    const menu = await screen.findByTestId('onboarding-defaults-translate-select-menu')
    await userEvent.click(within(menu).getByText('model-b'))
    expect(screen.getByTestId('onboarding-defaults-translate-select')).toHaveTextContent('model-b')
    await userEvent.click(screen.getByTestId('onboarding-test'))
    await waitFor(() => expect(h.callsOf('provider_test')).toHaveLength(2))
    // 模型列表来自测试连接的 fetched_models：从头到尾不单独拉取
    expect(h.callsOf('provider_models')).toHaveLength(0)
    expect(screen.getByTestId('onboarding-defaults-translate-select')).toHaveTextContent('model-b')
    await userEvent.click(screen.getByTestId('onboarding-save'))
    await waitFor(() => expect(h.callsOf('settings_patch')).toHaveLength(1))
    const patch = h.callsOf('settings_patch')[0]?.patch as { defaults: { translate: unknown } }
    expect(patch.defaults.translate).toEqual({ provider_id: 'anthropic', model_id: 'model-b' })
  })

  it('跳过引导（稍后设置）：测试过的草稿 Key 从没写进凭据存储，关窗时没有要删的', async () => {
    const h = modelHarness()
    start(2)
    await userEvent.click(await screen.findByTestId('card-gemini'))
    await userEvent.type(await screen.findByTestId('key-input'), 'sk-g')
    await userEvent.click(screen.getByTestId('onboarding-test'))
    await waitFor(() => expect(h.callsOf('provider_test')).toHaveLength(1))
    expect(h.callsOf('provider_test')[0]?.key).toBe('sk-g')
    await userEvent.click(screen.getByTestId('onboarding-skip-all'))
    await waitFor(() => expect(h.close).toHaveBeenCalled())
    expect(h.callsOf('secret_set')).toHaveLength(0)
    expect(h.callsOf('secret_delete')).toHaveLength(0)
  })
})

describe('转发的动作', () => {
  it('欢迎页按划词翻译键、没有选中文字：打开输入翻译，而不是让 Rust 对引导窗口做系统取词', async () => {
    const h = installHarness()
    start(0)
    await screen.findByTestId('onboarding-welcome')
    window.getSelection()?.removeAllRanges()
    act(() => h.emit('onboarding:hotkey', { action: 'translate_selection' }))
    await waitFor(() =>
      expect(h.callsOf('trigger_action')).toEqual([
        { action: 'translate_input', origin: 'onboarding', text: null },
      ]),
    )
  })

  it('「试一试」里按划词翻译键、没有选中文字：译示例段落', async () => {
    const h = installHarness()
    start(3)
    const sample = await screen.findByTestId('onboarding-sample')
    window.getSelection()?.removeAllRanges()
    act(() => h.emit('onboarding:hotkey', { action: 'translate_selection' }))
    await waitFor(() => expect(h.callsOf('trigger_action')).toHaveLength(1))
    expect(h.callsOf('trigger_action')[0]).toEqual({
      action: 'translate_selection',
      origin: 'onboarding',
      text: sample.textContent,
    })
  })
})

describe('重启后回到权限页', () => {
  it('点过「前往系统设置」后「重新启动 Wisp」：重启时带上这一步（app_relaunch(resume)）；Rust 带回的路由直接打开权限页', async () => {
    const s = pendingPermissions()
    const h = installHarness(s)
    h.handle('permissions_request', () => s.runtime.permissions)
    const first = start(1)
    await userEvent.click(await screen.findByTestId('permission-screen_recording-grant'))
    await userEvent.click(await screen.findByTestId('permission-relaunch'))
    await waitFor(() => expect(h.callsOf('app_relaunch')).toHaveLength(1))
    expect(h.callsOf('app_relaunch')[0]).toEqual({ resume: { kind: 'onboarding', step: 1 } })
    first.unmount()
    resetSettingsStore()
    installHarness(pendingPermissions())
    // 重启后 Rust 按 state.json 里的续接目标打开引导：Boot.route = { onboarding, step: 1 }
    start(1)
    expect(await screen.findByTestId('onboarding-permissions')).toBeInTheDocument()
  })

  it('不再有前端的 localStorage 交接：重启时不留标记，没有 Rust 带回的路由时从欢迎页开始', async () => {
    const s = pendingPermissions()
    const h = installHarness(s)
    h.handle('permissions_request', () => s.runtime.permissions)
    const first = start(1)
    await userEvent.click(await screen.findByTestId('permission-screen_recording-grant'))
    await userEvent.click(await screen.findByTestId('permission-relaunch'))
    await waitFor(() => expect(h.callsOf('app_relaunch')).toHaveLength(1))
    expect(localStorage.length).toBe(0)
    first.unmount()
    resetSettingsStore()
    installHarness(pendingPermissions())
    start(null)
    expect(await screen.findByTestId('onboarding-welcome')).toBeInTheDocument()
  })
})
