/**
 * 设置窗口各页的交互：目标语言对调、⌘W 关窗前删掉草稿 Key、子页里再点当前分组回到列表与紧凑标题、
 * 服务列表的徽标与重名编号、推理档位（含 low）、风格名称清空与温度开关、快捷键测试结果随绑定作废、术语表新行不抢焦点。
 */
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { getAppearance, setAppearance } from '../../shared/hooks/useAppearance'
import { boot } from '../../shared/ipc'
import type { ProviderTemplate } from '../../shared/ipc/types'
import { resetAdviceCache } from '../components/AdviceBanners'
import { resetHotkeyTest } from '../components/hotkeyTest'
import { resetStyleSaves } from '../lib/styleSave'
import { resetWrites } from '../lib/writes'
import { resetTestRuns } from '../providers/hooks'
import { resetTemplatesCache } from '../providers/templates'
import { SettingsApp } from '../SettingsApp'
import { resetSettingsStore, setQueueDelay } from '../store'
import { entry, installHarness, provider, snapshot, style } from './harness'

const appearance = getAppearance()

const TEMPLATES: ProviderTemplate[] = [
  {
    id: 'deepseek',
    name_key: 'settings.providers.template.deepseek',
    group: 'other',
    provider: provider('deepseek', { name: 'DeepSeek', base_url: 'https://api.deepseek.com' }),
    key_required: true,
    region_variants: [],
  },
]

const okReport = {
  models: { ok: true, status: 200, elapsed_ms: 120, url: 'u/models', error: null },
  stream: { ok: true, status: 200, elapsed_ms: 340, url: 'u/chat', error: null },
  overall: 'ok',
  model_used: 'chat',
  fetched_models: [],
}

function renderApp(section?: string) {
  boot.route = section ? { kind: 'settings', section } : null
  return render(<SettingsApp />)
}

function pressModW() {
  fireEvent.keyDown(window, {
    key: 'w',
    code: 'KeyW',
    ...(boot.os === 'macos' ? { metaKey: true } : { ctrlKey: true }),
  })
}

beforeEach(() => {
  resetSettingsStore()
  resetTemplatesCache()
  resetTestRuns()
  resetAdviceCache()
  resetHotkeyTest()
  resetStyleSaves()
  resetWrites()
  setQueueDelay(null)
  setAppearance({ ...appearance, os: 'macos', uiLanguage: 'zh-CN' })
  // jsdom 没有布局：模型列表的滚动容器按内联高度报告尺寸，虚拟列表才会渲染视口内的行
  const real = HTMLElement.prototype.getBoundingClientRect
  vi.spyOn(HTMLElement.prototype, 'getBoundingClientRect').mockImplementation(function (
    this: HTMLElement,
  ) {
    if (
      this.classList.contains('st-models-scroll') ||
      this.classList.contains('st-glossary-scroll')
    ) {
      const height = Number.parseFloat(this.style.height) || 0
      return {
        x: 0,
        y: 0,
        top: 0,
        left: 0,
        right: 560,
        bottom: height,
        width: 560,
        height,
        toJSON() {},
      }
    }
    return real.call(this)
  })
  const height = Object.getOwnPropertyDescriptor(HTMLElement.prototype, 'offsetHeight')?.get
  vi.spyOn(HTMLElement.prototype, 'offsetHeight', 'get').mockImplementation(function (
    this: HTMLElement,
  ) {
    if (
      this.classList.contains('st-models-scroll') ||
      this.classList.contains('st-glossary-scroll')
    )
      return Number.parseFloat(this.style.height) || 0
    return height?.call(this) ?? 0
  })
  vi.spyOn(HTMLElement.prototype, 'offsetWidth', 'get').mockImplementation(function (
    this: HTMLElement,
  ) {
    return this.classList.contains('st-models-scroll') ||
      this.classList.contains('st-glossary-scroll')
      ? 560
      : 0
  })
})

afterEach(() => {
  vi.useRealTimers()
  boot.route = null
  setAppearance(appearance)
})

describe('目标语言', () => {
  it('目标语言改成当前的第二语言：两者对调、一次写入；两者相同（手改）时第二语言显示为待选并标红', async () => {
    const h = installHarness()
    renderApp('translate')
    await userEvent.click(await screen.findByTestId('translate-primary-select'))
    const menu = await screen.findByTestId('translate-primary-select-menu')
    await userEvent.click(within(menu).getByText('英语'))
    await waitFor(() => expect(h.callsOf('settings_patch')).toHaveLength(1))
    expect(h.callsOf('settings_patch')[0]?.patch).toEqual({
      translate: { primary_lang: 'en', secondary_lang: 'zh-CN' },
    })
    expect(screen.getByTestId('translate-secondary-select')).toHaveTextContent('中文')

    // 手改了 settings.json 让两者相同
    const same = snapshot()
    same.settings.translate = {
      ...same.settings.translate,
      primary_lang: 'en',
      secondary_lang: 'en',
    }
    act(() => h.emit('settings:changed', same))
    const row = screen.getByTestId('translate-secondary')
    await waitFor(() => expect(row).toHaveTextContent('与目标语言相同'))
    expect(screen.getByTestId('translate-secondary-select')).toHaveTextContent('选择语言')
  })
})

describe('草稿 Key 不经钥匙串', () => {
  it('新建服务测试 / 拉取模型带上草稿 Key；之后 ⌘W 直接关窗，凭据存储从头到尾没有被碰过', async () => {
    const h = installHarness()
    h.handle('provider_templates', () => TEMPLATES)
    h.handle('provider_new', () => ({ ...TEMPLATES[0]?.provider, id: 'deepseek' }))
    h.handle('provider_test', () => okReport)
    h.handle('provider_models', () => [entry('chat', 'no')])
    renderApp('providers')
    await userEvent.click(await screen.findByTestId('card-others'))
    await userEvent.click(await screen.findByTestId('template-deepseek'))
    await userEvent.type(await screen.findByTestId('key-input'), '  sk-draft  ')
    await userEvent.click(screen.getByTestId('provider-test'))
    await waitFor(() => expect(h.callsOf('provider_test')).toHaveLength(1))
    // 去掉首尾空白后交给 Rust
    expect(h.callsOf('provider_test')[0]?.key).toBe('sk-draft')
    pressModW()
    await waitFor(() => expect(h.close).toHaveBeenCalled())
    expect(h.callsOf('secret_set')).toHaveLength(0)
    expect(h.callsOf('secret_delete')).toHaveLength(0)
    expect(h.callsOf('settings_patch')).toHaveLength(0)
  })

  it('已保存的服务：输入框里还没保存的 Key 先写入，测试时 key = null（Rust 从钥匙串读）', async () => {
    const h = installHarness(snapshot({ providers: [provider('deepseek')] }))
    h.handle('provider_templates', () => TEMPLATES)
    h.handle('provider_test', () => okReport)
    renderApp('providers')
    await userEvent.click(await screen.findByTestId('provider-row-deepseek'))
    await userEvent.type(await screen.findByTestId('key-input'), 'sk-saved')
    await userEvent.click(screen.getByTestId('provider-test'))
    await waitFor(() => expect(h.callsOf('provider_test')).toHaveLength(1))
    expect(h.callsOf('secret_set')).toEqual([{ provider_id: 'deepseek', key: 'sk-saved' }])
    expect(h.callsOf('provider_test')[0]?.key).toBeNull()
  })
})

describe('子页导航', () => {
  it('编辑器里再点一次「模型与服务」回到列表；紧凑标题显示服务名而不是分组名', async () => {
    const h = installHarness(
      snapshot(
        { providers: [provider('deepseek', { name: 'DeepSeek' })] },
        { has_key: { deepseek: true } },
      ),
    )
    h.handle('provider_templates', () => TEMPLATES)
    renderApp('providers')
    await userEvent.click(await screen.findByTestId('provider-row-deepseek'))
    expect(await screen.findByTestId('provider-editor')).toBeInTheDocument()
    expect(document.querySelector('.st-topbar-title')).toHaveTextContent('DeepSeek')
    await userEvent.click(screen.getByTestId('settings-nav-providers'))
    expect(await screen.findByTestId('provider-list')).toBeInTheDocument()
    expect(screen.queryByTestId('provider-editor')).toBeNull()
    expect(document.querySelector('.st-topbar-title')).toHaveTextContent('模型与服务')
  })

  it('有改动的新建草稿：再点当前分组先原地确认放弃', async () => {
    const h = installHarness()
    h.handle('provider_templates', () => TEMPLATES)
    h.handle('provider_new', () => ({ ...TEMPLATES[0]?.provider, id: 'deepseek' }))
    renderApp('providers')
    await userEvent.click(await screen.findByTestId('card-others'))
    await userEvent.click(await screen.findByTestId('template-deepseek'))
    await userEvent.type(await screen.findByTestId('key-input'), 'sk-1')
    await userEvent.click(screen.getByTestId('settings-nav-providers'))
    expect(await screen.findByTestId('provider-discard-confirm')).toBeInTheDocument()
    expect(screen.getByTestId('provider-editor')).toBeInTheDocument()
  })
})

describe('服务列表', () => {
  it('从没测试过不显示「未测试」；头像是两字母且读屏不念；新建的同名服务自动加编号', async () => {
    const h = installHarness(
      snapshot(
        { providers: [provider('deepseek', { name: 'DeepSeek' })] },
        { has_key: { deepseek: true } },
      ),
    )
    h.handle('provider_templates', () => TEMPLATES)
    h.handle('provider_new', () => ({ ...TEMPLATES[0]?.provider, id: 'deepseek-2' }))
    renderApp('providers')
    const row = await screen.findByTestId('provider-row-deepseek')
    expect(row).not.toHaveTextContent('未测试')
    const logo = row.querySelector('.st-logo')
    expect(logo).toHaveTextContent('DS')
    expect(logo).toHaveAttribute('aria-hidden', 'true')
    await userEvent.click(screen.getByTestId('provider-add-open'))
    await userEvent.click(await screen.findByTestId('template-deepseek'))
    expect(await screen.findByTestId('provider-name')).toHaveValue('DeepSeek 2')
  })
})

describe('模型表', () => {
  it('推理档位在 ⋯ 菜单里：默认（low）/ low / minimal / none，显式的 low 会被勾选', async () => {
    installHarness(
      snapshot({
        providers: [
          provider('openai', {
            quirks: {
              thinking_off: 'openai_effort',
              merge_system_into_user: false,
              strip_think_tags: true,
              max_tokens_field: null,
              dual_auth_header: null,
              gemini_key_in_query: false,
            },
            models: [entry('o-mini', 'yes', { reasoning: true, reasoning_effort: 'low' })],
          }),
        ],
      }),
    )
    renderApp('providers')
    await userEvent.click(await screen.findByTestId('provider-row-openai'))
    const row = await screen.findByTestId('model-row-o-mini')
    // 非默认档位在 ID 下面的副行写出来
    expect(row).toHaveTextContent('档位：low')
    await userEvent.click(within(row).getByTestId('model-more-o-mini'))
    const items = await screen.findAllByRole('menuitemcheckbox')
    expect(items.map((x) => x.textContent)).toEqual(
      expect.arrayContaining(['默认（low）', 'low', 'minimal', 'none']),
    )
    expect(items.find((x) => x.textContent === 'low')).toHaveAttribute('aria-checked', 'true')
  })
})

describe('风格编辑器', () => {
  function styles() {
    return [
      style('natural'),
      style('custom-1', { builtin: false, name: '我的风格', temperature: 0.4 }),
    ]
  }

  it('清空自定义风格的名称：输入框回到原名（不会停在空白）', async () => {
    const h = installHarness(snapshot({}, {}, styles()))
    h.handle('style_save', ({ style: st }) =>
      styles().map((x) => (x.id === (st as { id: string }).id ? st : x)),
    )
    renderApp('translate')
    await userEvent.click(await screen.findByTestId('style-open-custom-1'))
    const name = await screen.findByTestId('style-name')
    await userEvent.clear(name)
    fireEvent.blur(name)
    await waitFor(() => expect(name).toHaveValue('我的风格'))
    expect(h.callsOf('style_save')).toHaveLength(0)
  })

  it('拖完温度滑块 250 ms 内关掉「发送温度参数」：停顿中的值不会再把它打开', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true })
    const h = installHarness(snapshot({}, {}, styles()))
    h.handle('style_save', ({ style: st }) =>
      styles().map((x) => (x.id === (st as { id: string }).id ? st : x)),
    )
    renderApp('translate')
    fireEvent.click(await screen.findByTestId('style-open-custom-1'))
    const slider = await screen.findByTestId('style-temperature')
    const input = slider.querySelector('input') ?? slider
    fireEvent.change(input, { target: { value: '0.8' } })
    fireEvent.click(screen.getByTestId('style-temperature-switch'))
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1000)
    })
    const saved = h
      .callsOf('style_save')
      .map((a) => (a.style as { temperature: number | null }).temperature)
    expect(saved.at(-1)).toBeNull()
    expect(saved).not.toContain(0.8)
    expect(screen.getByTestId('style-temperature-switch')).toHaveAttribute('aria-checked', 'false')
  })
})

describe('快捷键测试', () => {
  it('测得「已收到」后重新录制：结果不再挂在新组合旁边', async () => {
    const h = installHarness()
    h.handle('hotkey_set', ({ combo }) => {
      const list = snapshot().runtime.hotkeys.map((x, i) =>
        i === 0 ? { ...x, binding: 'custom' as const, effective: String(combo) } : x,
      )
      return list
    })
    renderApp('hotkeys')
    const row = await screen.findByTestId('hotkey-translate_selection')
    await userEvent.click(within(row).getByTestId('hotkey-translate_selection-test'))
    await waitFor(() => expect(h.callsOf('hotkey_test_begin')).toHaveLength(1))
    act(() =>
      h.emit('hotkey:test_received', { target: { kind: 'action', action: 'translate_selection' } }),
    )
    expect(await within(row).findByText('已收到')).toBeInTheDocument()
    act(() =>
      h.emit(
        'hotkeys:changed',
        snapshot().runtime.hotkeys.map((x, i) =>
          i === 0 ? { ...x, binding: 'custom' as const, effective: 'Alt+Shift+KeyK' } : x,
        ),
      ),
    )
    await waitFor(() => expect(within(row).queryByText('已收到')).toBeNull())
  })
})

describe('术语表', () => {
  it('新加的行只自动聚焦一次：之后在搜索框里输入、清空，焦点不会被它抢走', async () => {
    const h = installHarness()
    h.handle('glossary_get', () =>
      Array.from({ length: 10 }, (_, i) => ({ source: `term${i}`, target: `词${i}`, note: null })),
    )
    renderApp('translate')
    await userEvent.click(await screen.findByTestId('glossary-add'))
    const sources = await screen.findAllByTestId('glossary-source')
    const added = sources.at(-1) as HTMLElement
    await waitFor(() => expect(added).toHaveFocus())
    await userEvent.type(added, 'new')
    const search = screen.getByRole('searchbox')
    await userEvent.click(search)
    await userEvent.type(search, 'term1')
    await userEvent.clear(search)
    expect(search).toHaveFocus()
  })
})
