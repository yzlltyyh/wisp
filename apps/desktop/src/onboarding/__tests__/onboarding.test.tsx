/**
 * 首次引导：步骤（macOS 3 步，其他平台 2 步）、权限实时变绿与轮询只在可见时
 * 存在、接入模型（卡片 → 测试 → 选默认模型 → 写入 Provider 与 defaults）、试一试（onboarding:hotkey → getSelection →
 * trigger_action，自动打勾）、完成 / 跳过都调用 onboarding_complete 并关闭窗口。
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

function start(step: number | null, os: 'macos' | 'windows' = 'macos') {
  setAppearance({ ...appearance, os, uiLanguage: 'zh-CN' })
  boot.os = os
  boot.route = step === null ? null : { kind: 'onboarding', step }
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
  Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'visible' })
})

/** 底栏里当前步骤的按钮（换步骤时旧步骤的按钮组还在原位淡出 100 ms，带 data-leaving） */
function footButton(id: string): HTMLElement {
  const el = document.querySelector<HTMLElement>(
    `.ob-foot-inner:not([data-leaving]) [data-testid="${id}"]`,
  )
  if (!el) throw new Error(`底栏里没有 ${id}`)
  return el
}

describe('步骤', () => {
  it('macOS：欢迎 → 权限 → 接入模型 → 试一试；步骤条只列 3 步', async () => {
    installHarness()
    start(null)
    // 底栏是固定的外框：切换步骤时同一个元素，只换里面的按钮
    const foot = await screen.findByTestId('onboarding-foot')
    expect(within(foot).getByTestId('onboarding-start')).toBeInTheDocument()
    expect(screen.queryByTestId('onboarding-stepper')).toBeNull()
    await userEvent.click(await screen.findByTestId('onboarding-start'))
    const stepper = await screen.findByTestId('onboarding-stepper')
    expect(within(stepper).getAllByRole('button')).toHaveLength(3)
    expect(await screen.findByTestId('onboarding-permissions')).toBeInTheDocument()
    expect(screen.getByTestId('onboarding-foot')).toBe(foot)
    expect(within(foot).getByTestId('onboarding-next')).toBeInTheDocument()
    await userEvent.click(screen.getByTestId('onboarding-next'))
    expect(await screen.findByTestId('onboarding-model')).toBeInTheDocument()
    // 换步骤时旧按钮组在原位淡出（data-leaving、inert）：点当前步骤的那一个
    await userEvent.click(footButton('onboarding-next'))
    expect(await screen.findByTestId('onboarding-try')).toBeInTheDocument()
    await userEvent.click(footButton('onboarding-back'))
    expect(await screen.findByTestId('onboarding-model')).toBeInTheDocument()
  })

  it('换步骤：旧按钮组叠在原位淡出（不可点、读屏跳过），新按钮组同时淡入，不一帧硬切', async () => {
    installHarness()
    start(null)
    await userEvent.click(await screen.findByTestId('onboarding-start'))
    await screen.findByTestId('onboarding-permissions')
    // 还在淡出的旧按钮组（如果此刻还在）：读屏跳过、不可聚焦
    const leaving = [...document.querySelectorAll<HTMLElement>('.ob-foot-inner[data-leaving]')]
    expect(
      leaving.every((el) => el.getAttribute('aria-hidden') === 'true' && el.hasAttribute('inert')),
    ).toBe(true)
    // 淡出结束后只剩当前步骤的按钮组
    await waitFor(() => expect(document.querySelectorAll('.ob-foot-inner')).toHaveLength(1))
    expect(footButton('onboarding-next')).toBeInTheDocument()
  })

  it('Windows：没有权限步骤（2 步），route 1 落到接入模型', async () => {
    installHarness()
    start(1, 'windows')
    expect(await screen.findByTestId('onboarding-model')).toBeInTheDocument()
    expect(within(screen.getByTestId('onboarding-stepper')).getAllByRole('button')).toHaveLength(2)
  })

  it('跳过引导与完成都记为完成并关闭窗口', async () => {
    const h = installHarness()
    start(0)
    await userEvent.click(await screen.findByTestId('onboarding-skip-all'))
    await waitFor(() => expect(h.callsOf('onboarding_complete')).toHaveLength(1))
    await waitFor(() => expect(h.close).toHaveBeenCalled())
  })

  it('任何一步都转发被拦下的全局动作（引导窗口在前台时每个动作都转成 onboarding:hotkey）', async () => {
    const h = installHarness()
    start(0)
    await screen.findByTestId('onboarding-welcome')
    act(() => h.emit('onboarding:hotkey', { action: 'ocr' }))
    await waitFor(() =>
      expect(h.callsOf('trigger_action')).toEqual([
        { action: 'ocr', origin: 'onboarding', text: null },
      ]),
    )
  })

  it('window:navigate 跳到指定步骤；更新后授权失效时从权限开始', async () => {
    const s = snapshot()
    s.runtime.permissions = { ...s.runtime.permissions, lost_after_update: true }
    const h = installHarness(s)
    start(null)
    expect(await screen.findByTestId('permissions-lost')).toBeInTheDocument()
    act(() => h.emit('window:navigate', { kind: 'onboarding', step: 3 }))
    expect(await screen.findByTestId('onboarding-try')).toBeInTheDocument()
  })
})

describe('权限', () => {
  it('授权后实时变绿；轮询只在权限步骤可见时开启', async () => {
    const s = snapshot()
    s.runtime.permissions = {
      accessibility: 'granted',
      screen_recording: 'not_determined',
      input_monitoring: 'not_determined',
      lost_after_update: false,
    }
    const h = installHarness(s)
    h.handle('permissions_status', () => s.runtime.permissions)
    h.handle('permissions_request', () => s.runtime.permissions)
    start(1)
    const card = await screen.findByTestId('permission-screen_recording')
    expect(card).not.toHaveAttribute('data-granted')
    // 还没就绪：底栏只有次级的「跳过这一步」，没有主按钮
    expect(screen.getByTestId('onboarding-next')).toHaveTextContent('跳过这一步')
    expect(screen.getByTestId('onboarding-next')).toHaveAttribute('data-variant', 'secondary')
    await waitFor(() => expect(h.callsOf('permissions_watch')).toEqual([{ active: true }]))
    await userEvent.click(within(card).getByTestId('permission-screen_recording-grant'))
    expect(h.callsOf('permissions_request')).toEqual([{ kind: 'screen_recording' }])
    // 授权后需要重启才能生效的提示
    expect(await screen.findByTestId('permission-relaunch')).toBeInTheDocument()
    act(() =>
      h.emit('permissions:changed', { ...s.runtime.permissions, screen_recording: 'granted' }),
    )
    await waitFor(() =>
      expect(screen.getByTestId('permission-screen_recording')).toHaveAttribute('data-granted'),
    )
    // 两项都就绪：同一位置交叉淡入成主按钮「下一步」（「跳过这一步」变成 inert，不能再点）
    const ready = screen.getByTestId('onboarding-next-ready')
    expect(ready).toHaveTextContent('下一步')
    expect(ready).toHaveAttribute('data-variant', 'primary')
    expect(ready.closest('.ob-swap')).toHaveAttribute('data-active', 'b')
    expect(screen.getByTestId('onboarding-next').closest('[inert]')).not.toBeNull()
    // 窗口隐藏：停止轮询
    Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'hidden' })
    act(() => {
      document.dispatchEvent(new Event('visibilitychange'))
    })
    await waitFor(() => expect(h.callsOf('permissions_watch').at(-1)).toEqual({ active: false }))
    // 回来后重新开启；离开权限步骤再停
    Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'visible' })
    act(() => {
      document.dispatchEvent(new Event('visibilitychange'))
    })
    await waitFor(() => expect(h.callsOf('permissions_watch').at(-1)).toEqual({ active: true }))
    await userEvent.click(screen.getByTestId('onboarding-next-ready'))
    await waitFor(() => expect(h.callsOf('permissions_watch').at(-1)).toEqual({ active: false }))
  })
})

describe('权限按钮层级', () => {
  it('两项都未授权：只有第一张卡片是主按钮；已拒绝的卡片写明状态', async () => {
    const s = snapshot()
    s.runtime.permissions = {
      accessibility: 'not_determined',
      screen_recording: 'denied',
      input_monitoring: 'not_determined',
      lost_after_update: false,
    }
    installHarness(s)
    start(1)
    const first = await screen.findByTestId('permission-accessibility-grant')
    expect(first).toHaveAttribute('data-variant', 'primary')
    expect(screen.getByTestId('permission-screen_recording-grant')).toHaveAttribute(
      'data-variant',
      'secondary',
    )
    expect(screen.getByTestId('permission-screen_recording-denied')).toHaveTextContent('已拒绝')
    expect(screen.getByTestId('permission-accessibility')).toHaveTextContent('未授权')
    // 底栏里交叉淡入备用的「下一步」此时是 inert（看不见、不能点），不算
    expect(
      screen
        .getAllByRole('button')
        .filter((b) => b.dataset.variant === 'primary' && !b.closest('[inert]')),
    ).toHaveLength(1)
  })
})

describe('接入模型', () => {
  it('卡片 → 填 Key → 测试（草稿 Key 直接交给 Rust）→ 选默认 → 保存并继续时才写 Key', async () => {
    const h = installHarness()
    h.handle('provider_templates', () => TEMPLATES)
    h.handle('provider_new', ({ template_id }) => ({
      ...TEMPLATES.find((t) => t.id === template_id)?.provider,
      id: 'openai',
    }))
    h.handle('provider_test', () => ({
      models: { ok: true, status: 200, elapsed_ms: 100, url: 'u', error: null },
      stream: { ok: true, status: 200, elapsed_ms: 200, url: 'u', error: null },
      overall: 'ok',
      model_used: 'gpt-mini',
      // 测试连接顺带返回合并好的模型列表：直接采用，不再另调 provider_models（多一次 GET /models 与钥匙串读取）
      fetched_models: [entry('gpt-mini', 'yes'), entry('text', 'no')],
    }))
    h.handle('provider_models', () => [entry('gpt-mini', 'yes'), entry('text', 'no')])
    start(2)
    await userEvent.click(await screen.findByTestId('card-openai-responses'))
    await screen.findByTestId('onboarding-setup')
    // 表单阶段：服务头部就是这一屏的标题（不再叠一个「接入一个模型」）
    expect(screen.getAllByRole('heading', { level: 1 }).map((x) => x.textContent)).toEqual([
      'OpenAI',
    ])
    // 只输入、不按回车：测试时草稿 Key 随命令带上，不经钥匙串
    await userEvent.type(screen.getByTestId('key-input'), 'sk-1')
    await userEvent.click(screen.getByTestId('onboarding-test'))
    await waitFor(() => expect(h.callsOf('provider_test')).toHaveLength(1))
    expect(h.callsOf('provider_test')[0]?.key).toBe('sk-1')
    expect(h.callsOf('secret_set')).toHaveLength(0)
    expect(await screen.findByTestId('onboarding-defaults')).toBeInTheDocument()
    expect(h.callsOf('provider_models')).toHaveLength(0)
    // 测试之后又改了 Key：「保存并继续」写入新的 Key，再写设置
    await userEvent.type(screen.getByTestId('key-input'), '-new')
    await userEvent.click(await screen.findByTestId('onboarding-save'))
    await waitFor(() => expect(h.callsOf('settings_patch')).toHaveLength(1))
    expect(h.callsOf('secret_set')).toEqual([{ provider_id: 'openai', key: 'sk-1-new' }])
    // 先写 Key，再按 id 只加这一个服务，最后只写默认模型（不把整个服务列表发回去）
    const order = h.calls.map(([c]) => c)
    expect(order.lastIndexOf('secret_set')).toBeLessThan(order.indexOf('provider_add'))
    expect(order.indexOf('provider_add')).toBeLessThan(order.indexOf('settings_patch'))
    // 保存了：离开这一步不删 Key
    expect(h.callsOf('secret_delete')).toHaveLength(0)
    expect(h.callsOf('provider_add')).toHaveLength(1)
    const added = h.callsOf('provider_add')[0]?.provider as {
      id: string
      protocol: string
      models: Array<{ id: string }>
    }
    expect(added).toMatchObject({ id: 'openai', protocol: 'openai_responses' })
    expect(added.models.map((m) => m.id)).toEqual(['gpt-mini', 'text'])
    const patch = h.callsOf('settings_patch')[0]?.patch as Record<string, unknown>
    expect(patch).toEqual({
      defaults: {
        translate: { provider_id: 'openai', model_id: 'gpt-mini' },
        ocr: { provider_id: 'openai', model_id: 'gpt-mini' },
      },
    })
    expect(await screen.findByTestId('onboarding-try')).toBeInTheDocument()
    expect(h.callsOf('secret_delete')).toHaveLength(0)
  })

  it('OpenAI 卡片可以切换为 Chat；无效 Key 只在 Key 字段就地提示；跳过这一步保留草稿；凭据存储从头到尾没有被碰过', async () => {
    const h = installHarness()
    h.handle('provider_templates', () => TEMPLATES)
    h.handle('provider_new', () => ({ ...TEMPLATES[1]?.provider, id: 'openai' }))
    h.handle('provider_test', () => ({
      models: { ok: false, status: 401, elapsed_ms: 10, url: 'u', error: null },
      stream: {
        ok: false,
        status: 401,
        elapsed_ms: 10,
        url: 'u',
        error: {
          code: 'auth',
          subcode: null,
          message: 'bad key',
          retryable: false,
          hint: null,
          status: 401,
          url: 'u',
          retry_after_s: null,
          upstream_request_id: null,
        },
      },
      overall: 'failed',
      model_used: null,
      fetched_models: [],
    }))
    start(2)
    await userEvent.click(await screen.findByTestId('card-openai-responses'))
    await userEvent.type(await screen.findByTestId('key-input'), 'sk-bad')
    await userEvent.click(await screen.findByRole('radio', { name: 'Chat' }))
    // 切换协议保留草稿 id 与输入的 Key
    expect(screen.getByTestId('key-input')).toHaveValue('sk-bad')
    await userEvent.click(screen.getByTestId('onboarding-test'))
    expect(await screen.findByTestId('key-rejected')).toHaveTextContent('服务拒绝了这个 Key')
    expect(screen.getByTestId('test-report')).toHaveTextContent('HTTP 401')
    expect(screen.getByTestId('test-report')).not.toHaveTextContent('API Key 无效或没有权限')
    const sent = h.callsOf('provider_test')[0]?.provider as {
      protocol: string
      template_id: string
    }
    expect(sent).toMatchObject({ protocol: 'openai_chat', template_id: 'openai' })
    expect(h.callsOf('provider_test')[0]?.key).toBe('sk-bad')
    // 跳过这一步：草稿留在引导窗口里（回来时还在）
    await userEvent.click(screen.getByTestId('onboarding-next'))
    expect(await screen.findByTestId('onboarding-try')).toBeInTheDocument()
    await userEvent.click(screen.getByTestId('onboarding-step-model'))
    expect(await screen.findByTestId('key-input')).toHaveValue('sk-bad')
    await userEvent.click(screen.getByTestId('onboarding-step-try'))
    await userEvent.click(await screen.findByTestId('onboarding-finish'))
    await waitFor(() => expect(h.close).toHaveBeenCalled())
    // 草稿 Key 只随测试命令发出过：没有写入，也就没有要删的
    expect(h.callsOf('secret_set')).toHaveLength(0)
    expect(h.callsOf('secret_delete')).toHaveLength(0)
  })

  it('没写过 Key 就跳过：什么都不删', async () => {
    const h = installHarness()
    h.handle('provider_templates', () => TEMPLATES)
    h.handle('provider_new', () => ({ ...TEMPLATES[2]?.provider, id: 'anthropic' }))
    start(2)
    await userEvent.click(await screen.findByTestId('card-anthropic'))
    await userEvent.type(await screen.findByTestId('key-input'), 'sk-never-written')
    await userEvent.click(screen.getByTestId('onboarding-next'))
    expect(await screen.findByTestId('onboarding-try')).toBeInTheDocument()
    expect(h.callsOf('secret_set')).toHaveLength(0)
    expect(h.callsOf('secret_delete')).toHaveLength(0)
  })

  it('已经接入过服务：显示摘要（服务名按界面语言拼接）', async () => {
    installHarness(
      snapshot({
        providers: [
          provider('openai', { name: 'OpenAI', models: [entry('gpt-mini')] }),
          provider('deepseek', { name: 'DeepSeek' }),
        ],
        defaults: { translate: { provider_id: 'openai', model_id: 'gpt-mini' }, ocr: null },
      }),
    )
    start(2)
    expect(await screen.findByTestId('onboarding-summary')).toHaveTextContent(
      // 中文的「和」与两侧的西文之间留空格
      '已接入：OpenAI 和 DeepSeek',
    )
    act(() => setAppearance({ ...appearance, os: 'macos', uiLanguage: 'en-US' }))
    expect(screen.getByTestId('onboarding-summary')).toHaveTextContent(
      'Connected: OpenAI and DeepSeek',
    )
    expect(screen.getByTestId('onboarding-add-another')).toBeInTheDocument()
  })
})

describe('试一试', () => {
  it('引导窗口里按热键：用 getSelection 取文本后 trigger_action，并打勾', async () => {
    const h = installHarness()
    start(3)
    const sample = await screen.findByTestId('onboarding-sample')
    const range = document.createRange()
    range.selectNodeContents(sample)
    const sel = window.getSelection()
    sel?.removeAllRanges()
    sel?.addRange(range)
    act(() => h.emit('onboarding:hotkey', { action: 'translate_selection' }))
    await waitFor(() => expect(h.callsOf('trigger_action')).toHaveLength(1))
    expect(h.callsOf('trigger_action')[0]).toEqual({
      action: 'translate_selection',
      origin: 'onboarding',
      text: sample.textContent,
    })
    await waitFor(() =>
      expect(screen.getByTestId('task-translate_selection')).toHaveAttribute('data-done'),
    )
    act(() => h.emit('onboarding:hotkey', { action: 'clipboard' }))
    await waitFor(() => expect(screen.getByTestId('task-clipboard')).toHaveAttribute('data-done'))
    // 其他动作原样转发
    act(() => h.emit('onboarding:hotkey', { action: 'ocr' }))
    await waitFor(() =>
      expect(h.callsOf('trigger_action').at(-1)).toEqual({
        action: 'ocr',
        origin: 'onboarding',
        text: null,
      }),
    )
    await userEvent.click(screen.getByTestId('onboarding-finish'))
    await waitFor(() => expect(h.callsOf('onboarding_complete')).toHaveLength(1))
  })

  it('快捷键注册失败：先在这里换一个组合；「直接试试」没有选中文字时用示例段落', async () => {
    const s = snapshot()
    s.runtime.hotkeys = s.runtime.hotkeys.map((x, i) =>
      i === 0
        ? { ...x, effective: null, state: 'failed', reason_key: 'hotkeys.conflict.registration' }
        : x,
    )
    const h = installHarness(s)
    start(3)
    const task = await screen.findByTestId('task-translate_selection')
    expect(within(task).getByTestId('hotkey-translate_selection')).toBeInTheDocument()
    // 没有组合键时任务是完整的句子；原因只说一行；录制器显示没注册上的组合；引导里不放「测试」
    expect(task).toHaveTextContent('选中上面的文字，再按划词翻译的快捷键')
    expect(task).toHaveTextContent('⌥D：')
    expect(task).toHaveTextContent('在下面换一个组合即可')
    expect(within(task).getByTestId('hotkey-translate_selection-recorder')).toHaveTextContent('⌥D')
    expect(within(task).queryByTestId('hotkey-translate_selection-test')).toBeNull()
    expect(within(task).getByTestId('hotkey-translate_selection')).toHaveTextContent('划词翻译')
    expect(within(task).queryByText('注册失败')).toBeNull()
    window.getSelection()?.removeAllRanges()
    fireEvent.click(within(task).getByTestId('task-translate_selection-try'))
    await waitFor(() => expect(h.callsOf('trigger_action')).toHaveLength(1))
    expect(h.callsOf('trigger_action')[0]?.text).toBe(
      screen.getByTestId('onboarding-sample').textContent,
    )
  })
})
