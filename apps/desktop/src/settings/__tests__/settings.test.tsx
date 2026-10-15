/**
 * 设置窗口：
 * 分组导航与深链、改动即时写入、危险操作二次确认、Provider 编辑器的完整流程（模板 → Key → 实时 URL → 测试 → 模型 →
 * 默认模型）、快捷键测试按钮、扩展触发授权入口、风格排序 / 新建 / 还原、术语表写入、关于页许可分组。
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
import { pendingWriteCount, resetWrites } from '../lib/writes'
import { resetTestRuns } from '../providers/hooks'
import { resetTemplatesCache } from '../providers/templates'
import { SettingsApp } from '../SettingsApp'
import { resetSettingsStore, setQueueDelay, useSettingsStore } from '../store'
import { entry, hotkeys, installHarness, provider, snapshot, style } from './harness'

const appearance = getAppearance()

const TEMPLATES: ProviderTemplate[] = [
  {
    id: 'openai',
    name_key: 'settings.providers.template.openai',
    group: 'openai',
    provider: provider('openai', { name: 'OpenAI', base_url: 'https://api.openai.com/v1' }),
    key_required: true,
    region_variants: [],
  },
  {
    id: 'qwen',
    name_key: 'settings.providers.template.qwen',
    group: 'other',
    provider: provider('qwen', {
      name: '通义',
      base_url: 'https://dashscope.aliyuncs.com/compatible-mode/v1',
    }),
    key_required: true,
    region_variants: [
      {
        label_key: 'settings.providers.region.cn',
        base_url: 'https://dashscope.aliyuncs.com/compatible-mode/v1',
      },
      {
        label_key: 'settings.providers.region.workspace',
        base_url: 'https://{WorkspaceId}.cn-beijing.maas.aliyuncs.com/compatible-mode/v1',
      },
    ],
  },
]

function renderApp(section?: string) {
  boot.route = section ? { kind: 'settings', section } : null
  return render(<SettingsApp />)
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
    if (this.classList.contains('st-models-scroll')) {
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
    if (this.classList.contains('st-models-scroll'))
      return Number.parseFloat(this.style.height) || 0
    return height?.call(this) ?? 0
  })
  vi.spyOn(HTMLElement.prototype, 'offsetWidth', 'get').mockImplementation(function (
    this: HTMLElement,
  ) {
    return this.classList.contains('st-models-scroll') ? 560 : 0
  })
})

afterEach(() => {
  vi.useRealTimers()
  boot.route = null
  setAppearance(appearance)
})

describe('窗口框架', () => {
  it('导航按 SETTINGS_SECTIONS 排列；boot.route 决定首个分组；window:navigate 切换分组', async () => {
    const h = installHarness()
    renderApp('ocr')
    const nav = screen.getByRole('tablist')
    expect(
      within(nav)
        .getAllByRole('tab')
        .map((t) => t.dataset.testid),
    ).toEqual([
      'settings-nav-general',
      'settings-nav-hotkeys',
      'settings-nav-providers',
      'settings-nav-translate',
      'settings-nav-ocr',
      'settings-nav-clipboard',
      'settings-nav-appearance',
      'settings-nav-about',
    ])
    expect(await screen.findByTestId('ocr-mode')).toBeInTheDocument()
    expect(screen.getByTestId('settings-nav-ocr')).toHaveAttribute('aria-selected', 'true')
    act(() => h.emit('window:navigate', { kind: 'settings', section: 'clipboard' }))
    expect(await screen.findByTestId('clip-enabled')).toBeInTheDocument()
    // 未知分组退回「通用」
    act(() => h.emit('window:navigate', { kind: 'settings', section: 'nope' }))
    expect(await screen.findByTestId('general-launch')).toBeInTheDocument()
  })

  it('↑↓ 在导航项之间移动并切换', async () => {
    installHarness()
    renderApp()
    await screen.findByTestId('general-launch')
    const general = screen.getByTestId('settings-nav-general')
    general.focus()
    fireEvent.keyDown(general, { key: 'ArrowDown' })
    expect(await screen.findByTestId('settings-nav-hotkeys')).toHaveAttribute(
      'aria-selected',
      'true',
    )
    expect(screen.getByTestId('settings-nav-hotkeys')).toHaveFocus()
  })

  it('连按方向键不丢键：按焦点所在的项计算下一项（section 是延后更新的）', async () => {
    installHarness()
    renderApp()
    await screen.findByTestId('general-launch')
    const general = screen.getByTestId('settings-nav-general')
    general.focus()
    for (let i = 0; i < 5; i++)
      fireEvent.keyDown(document.activeElement as HTMLElement, { key: 'ArrowDown' })
    expect(screen.getByTestId('settings-nav-clipboard')).toHaveFocus()
    await waitFor(() =>
      expect(screen.getByTestId('settings-nav-clipboard')).toHaveAttribute('aria-selected', 'true'),
    )
    // tab 与内容区关联（读屏能把分组名和内容对上）
    const panel = screen.getByRole('tabpanel')
    expect(panel).toHaveAttribute('aria-labelledby', 'st-tab-clipboard')
    expect(screen.getByTestId('settings-nav-clipboard')).toHaveAttribute('aria-controls', panel.id)
  })

  it('⌘W：先提交还在停顿中的输入与合并写入，等写入返回后才关窗', async () => {
    const h = installHarness(snapshot({ providers: [provider('openai', { name: 'OpenAI' })] }))
    let release: () => void = () => {}
    h.handle(
      'settings_patch',
      ({ patch }) =>
        new Promise((resolve) => {
          release = () => {
            h.state.snap = {
              ...h.state.snap,
              settings: { ...h.state.snap.settings, ...(patch as object) },
            }
            resolve(structuredClone(h.state.snap))
          }
        }),
    )
    h.handle('provider_templates', () => TEMPLATES)
    renderApp('providers')
    await userEvent.click(await screen.findByTestId('provider-row-openai'))
    const name = await screen.findByTestId('provider-name')
    await userEvent.clear(name)
    await userEvent.type(name, 'Work')
    // 还在 450 ms 的停顿里：什么都没发
    expect(h.callsOf('settings_patch')).toHaveLength(0)
    fireEvent.keyDown(window, {
      key: 'w',
      code: 'KeyW',
      ...(boot.os === 'macos' ? { metaKey: true } : { ctrlKey: true }),
    })
    await waitFor(() => expect(h.callsOf('settings_patch')).toHaveLength(1))
    const sent = h.callsOf('settings_patch')[0]?.patch as { providers: Array<{ name: string }> }
    expect(sent.providers[0]?.name).toBe('Work')
    // 写入还没返回：不关窗
    expect(h.close).not.toHaveBeenCalled()
    expect(pendingWriteCount()).toBe(1)
    release()
    await waitFor(() => expect(h.close).toHaveBeenCalled())
  })

  it('原生关闭按钮（window:close_requested）：与 ⌘W 同样先提交停顿中的输入、等写入返回再关窗', async () => {
    const h = installHarness()
    renderApp('translate')
    let release: () => void = () => {}
    h.handle(
      'style_save',
      ({ style: st }) =>
        new Promise((resolve) => {
          release = () =>
            resolve(snapshot().styles.map((x) => (x.id === (st as { id: string }).id ? st : x)))
        }),
    )
    await userEvent.click(await screen.findByTestId('style-open-natural'))
    fireEvent.change(await screen.findByTestId('style-system'), { target: { value: 'changed' } })
    expect(h.callsOf('style_save')).toHaveLength(0)
    act(() => h.emit('window:close_requested', null))
    await waitFor(() => expect(h.callsOf('style_save')).toHaveLength(1))
    expect(h.close).not.toHaveBeenCalled()
    release()
    await waitFor(() => expect(h.close).toHaveBeenCalled())
  })

  it('窗口失焦时立即提交停顿中的输入（原生关闭按钮没有前端钩子）', async () => {
    const h = installHarness()
    renderApp('translate')
    h.handle('style_save', ({ style: st }) =>
      snapshot().styles.map((x) => (x.id === (st as { id: string }).id ? st : x)),
    )
    await userEvent.click(await screen.findByTestId('style-open-natural'))
    fireEvent.change(await screen.findByTestId('style-system'), { target: { value: 'changed' } })
    expect(h.callsOf('style_save')).toHaveLength(0)
    act(() => {
      window.dispatchEvent(new Event('blur'))
    })
    await waitFor(() => expect(h.callsOf('style_save')).toHaveLength(1))
    const saved = h.callsOf('style_save')[0]?.style as { system_template: string } | undefined
    expect(saved?.system_template).toBe('changed')
  })

  it('开机自启没能写进系统（App Translocation）：开关下方说明原因与处理办法', async () => {
    const s = snapshot()
    s.runtime.autostart_issue = 'settings.general.autostart_issue.translocated'
    installHarness(s)
    renderApp('general')
    const row = await screen.findByTestId('general-launch')
    expect(row).toHaveTextContent('「应用程序」')
  })

  it('改动即时写入（没有保存按钮）；settings:changed（外部手改）后界面同步', async () => {
    const h = installHarness()
    renderApp('general')
    const sw = await screen.findByTestId('general-launch-switch')
    expect(sw).toHaveAttribute('aria-checked', 'true')
    await userEvent.click(sw)
    expect(h.callsOf('settings_patch')).toEqual([
      { patch: { general: { launch_at_login: false } } },
    ])
    await waitFor(() => expect(sw).toHaveAttribute('aria-checked', 'false'))
    const external = snapshot()
    external.settings.general.launch_at_login = true
    act(() => h.emit('settings:changed', external))
    await waitFor(() => expect(sw).toHaveAttribute('aria-checked', 'true'))
  })

  it('读取失败显示重试；配置文件损坏 / 来自新版本时顶部提示', async () => {
    const h = installHarness()
    h.handle('settings_get', () => {
      throw { code: 'io', message: 'boom', llm: null }
    })
    h.handle('app_info', () => ({
      version: '0.1.0',
      platform: snapshot().runtime.platform,
      config_dir: '/cfg',
      data_dir: '/data',
      log_dir: '/log',
      dev_mode: false,
    }))
    renderApp()
    expect(await screen.findByTestId('settings-load-error')).toBeInTheDocument()
    h.handle('settings_get', () =>
      snapshot(
        {},
        {
          settings_issue: {
            kind: 'invalid_json',
            file: 'settings.json',
            backup_path: null,
            at_ms: 1,
          },
        },
      ),
    )
    // 读取失败：导航不可用；给出配置目录与「打开配置目录」
    expect(screen.getByTestId('settings-nav-hotkeys')).toBeDisabled()
    expect(await screen.findByTestId('settings-load-error-path')).toHaveTextContent('/cfg')
    await userEvent.click(screen.getByTestId('settings-open-config'))
    expect(h.callsOf('open_path')).toEqual([{ kind: 'config' }])
    await userEvent.click(screen.getByRole('button', { name: '重试' }))
    const banner = await screen.findByTestId('settings-issue-banner')
    expect(banner).toHaveTextContent('配置文件 settings.json 无法读取。')
    expect(banner).toHaveTextContent('已保留上一次有效的设置')
    // 本次会话关掉之后，切换分组不会重新出现
    await userEvent.click(within(banner).getByRole('button', { name: '关闭提示' }))
    await waitFor(() => expect(screen.queryByTestId('settings-issue-banner')).toBeNull())
    await userEvent.click(screen.getByTestId('settings-nav-ocr'))
    await screen.findByTestId('ocr-mode')
    expect(screen.queryByTestId('settings-issue-banner')).toBeNull()
  })
})

describe('模型与服务', () => {
  const okReport = {
    models: {
      ok: true,
      status: 200,
      elapsed_ms: 120,
      url: 'https://api.openai.com/v1/models',
      error: null,
    },
    stream: {
      ok: true,
      status: 200,
      elapsed_ms: 340,
      url: 'https://api.openai.com/v1/responses',
      error: null,
    },
    overall: 'ok',
    model_used: 'gpt-mini',
    fetched_models: [entry('gpt-mini', 'yes'), entry('text-only', 'no')],
  }

  function newProviderHarness() {
    const h = installHarness()
    h.handle('provider_templates', () => TEMPLATES)
    h.handle('provider_new', ({ template_id }) => ({
      ...TEMPLATES.find((t) => t.id === template_id)?.provider,
      id: String(template_id),
    }))
    h.handle('llm_preview_url', ({ base_url }) => ({
      method: 'POST',
      url: `${String(base_url).replace(/#$/, '')}/responses`,
      version_appended: false,
      forced: String(base_url).endsWith('#'),
    }))
    h.handle('provider_test', async () => okReport)
    h.handle('provider_models', () => [entry('gpt-mini', 'yes'), entry('text-only', 'no')])
    return h
  }

  it('完整流程：选模板 → 填 Key（不用回车）→ 实时 URL → 测试（草稿 Key 随命令带上）→ 拉取模型 → 添加时写 Key 并补上默认模型', async () => {
    const h = newProviderHarness()
    renderApp('providers')
    expect(await screen.findByTestId('provider-empty')).toBeInTheDocument()
    // 空状态是 2 × 2 模板卡片；第四张「其他服务」进入完整的模板列表
    await userEvent.click(screen.getByTestId('card-others'))
    await userEvent.click(await screen.findByTestId('template-openai'))
    const editor = await screen.findByTestId('provider-editor')
    expect(editor).toHaveAttribute('data-new')

    // 草稿：Key 只是草稿的一部分，输入时不写凭据存储
    await userEvent.type(within(editor).getByTestId('key-input'), 'sk-test')
    expect(screen.getByTestId('key-draft-hint')).toHaveTextContent('添加服务时')
    expect(h.callsOf('secret_set')).toHaveLength(0)

    expect(await screen.findByTestId('url-preview-text')).toHaveTextContent(
      'https://api.openai.com/v1/responses',
    )
    const url = screen.getByTestId('provider-base-url')
    await userEvent.type(url, '#')
    await waitFor(() => expect(screen.getByTestId('url-preview')).toHaveTextContent('不追加版本段'))

    // 测试：草稿 Key 随命令带上，只用于这一次请求，不读也不写凭据存储
    await userEvent.click(screen.getByTestId('provider-test'))
    expect(await screen.findByTestId('test-verdict')).toHaveTextContent('连接正常')
    expect(h.callsOf('provider_test')[0]?.key).toBe('sk-test')
    expect(h.callsOf('secret_set')).toHaveLength(0)
    expect(screen.getByTestId('test-report')).toHaveTextContent('获取模型列表')
    expect(screen.getByTestId('test-report')).toHaveTextContent('流式请求')
    // 第一次测试成功、还没有模型：直接采用测试顺带返回的列表，不再多发一次 GET /models
    expect(await screen.findByTestId('model-row-gpt-mini')).toBeInTheDocument()
    expect(h.callsOf('provider_models')).toHaveLength(0)

    await userEvent.click(screen.getByTestId('provider-add'))
    await waitFor(() => expect(h.callsOf('settings_patch')).toHaveLength(1))
    // 添加时才写入凭据存储，且在写设置之前
    expect(h.callsOf('secret_set')).toEqual([{ provider_id: 'openai', key: 'sk-test' }])
    const order = h.calls.map(([c]) => c)
    expect(order.indexOf('secret_set')).toBeLessThan(order.indexOf('settings_patch'))
    const patch = h.callsOf('settings_patch')[0]?.patch as {
      providers: Array<{ id: string; base_url: string; models: unknown[] }>
      defaults: unknown
    }
    expect(patch.providers.map((p) => p.id)).toEqual(['openai'])
    expect(patch.providers[0]?.base_url).toBe('https://api.openai.com/v1#')
    expect(patch.defaults).toEqual({
      translate: { provider_id: 'openai', model_id: 'gpt-mini' },
      ocr: { provider_id: 'openai', model_id: 'gpt-mini' },
    })
    expect(await screen.findByTestId('settings-notice')).toHaveTextContent('已添加')
    // 原地变成已保存的服务：同一个编辑器（没有重新挂载），Key 显示为已保存
    await waitFor(() =>
      expect(screen.getByTestId('provider-editor')).not.toHaveAttribute('data-new'),
    )
    expect(screen.getByTestId('provider-editor')).toBe(editor)
    expect(screen.getByTestId('key-saved')).toHaveTextContent('已保存在钥匙串')
    // 返回列表；离开时不删 Key
    await userEvent.click(screen.getByTestId('provider-back'))
    expect(await screen.findByTestId('provider-row-openai')).toBeInTheDocument()
    expect(h.callsOf('secret_delete')).toHaveLength(0)
  })

  it('只输入 Key、不回车就点「添加服务」：Key 照样写入，且在写设置之前', async () => {
    const h = newProviderHarness()
    renderApp('providers')
    await userEvent.click(await screen.findByTestId('card-others'))
    await userEvent.click(await screen.findByTestId('template-openai'))
    await userEvent.type(await screen.findByTestId('key-input'), 'sk-direct')
    await userEvent.click(screen.getByTestId('provider-add'))
    await waitFor(() => expect(h.callsOf('settings_patch')).toHaveLength(1))
    expect(h.callsOf('secret_set')).toEqual([{ provider_id: 'openai', key: 'sk-direct' }])
    const order = h.calls.map(([c]) => c)
    expect(order.indexOf('secret_set')).toBeLessThan(order.indexOf('settings_patch'))
    expect(await screen.findByTestId('key-saved')).toBeInTheDocument()
  })

  it('Key 写入失败：就地报错，不添加服务', async () => {
    const h = newProviderHarness()
    h.handle('secret_set', () => {
      throw { code: 'platform', message: 'errors.ipc.platform', llm: null }
    })
    renderApp('providers')
    await userEvent.click(await screen.findByTestId('card-others'))
    await userEvent.click(await screen.findByTestId('template-openai'))
    await userEvent.type(await screen.findByTestId('key-input'), 'sk-x')
    await userEvent.click(screen.getByTestId('provider-add'))
    expect(await within(screen.getByTestId('key-field')).findByRole('alert')).toBeInTheDocument()
    expect(h.callsOf('settings_patch')).toHaveLength(0)
  })

  it('取消新建 / 测试后切走分组：草稿 Key 从没写进凭据存储，什么都不用删', async () => {
    const h = newProviderHarness()
    renderApp('providers')
    await userEvent.click(await screen.findByTestId('card-others'))
    await userEvent.click(await screen.findByTestId('template-qwen'))
    await screen.findByTestId('provider-editor')
    expect(screen.getByTestId('provider-region')).toBeInTheDocument()
    await userEvent.type(screen.getByTestId('key-input'), 'sk-1')
    await userEvent.click(screen.getByTestId('provider-back'))
    expect(await screen.findByTestId('template-picker')).toBeInTheDocument()
    expect(h.callsOf('secret_set')).toHaveLength(0)
    expect(h.callsOf('secret_delete')).toHaveLength(0)

    // 再来一次：测试（草稿 Key 随命令带上），然后切到别的分组（卸载）
    await userEvent.click(await screen.findByTestId('template-qwen'))
    await userEvent.type(await screen.findByTestId('key-input'), 'sk-2')
    await userEvent.click(screen.getByTestId('provider-test'))
    await waitFor(() => expect(h.callsOf('provider_test')).toHaveLength(1))
    expect(h.callsOf('provider_test')[0]?.key).toBe('sk-2')
    await userEvent.click(screen.getByTestId('settings-nav-general'))
    await screen.findByTestId('general-launch')
    expect(h.callsOf('secret_set')).toHaveLength(0)
    expect(h.callsOf('secret_delete')).toHaveLength(0)
  })

  it('Esc：输入框里只退出输入；草稿有改动时先在底栏确认，Esc 取消确认', async () => {
    newProviderHarness()
    renderApp('providers')
    await userEvent.click(await screen.findByTestId('card-others'))
    await userEvent.click(await screen.findByTestId('template-openai'))
    const input = await screen.findByTestId('key-input')
    await userEvent.type(input, 'sk-1')
    await userEvent.keyboard('{Escape}')
    // 只退出输入：草稿还在
    expect(input).not.toHaveFocus()
    expect(screen.getByTestId('provider-editor')).toBeInTheDocument()
    // 再按 Esc：有改动，原地确认
    await userEvent.keyboard('{Escape}')
    const confirm = await screen.findByTestId('provider-discard-confirm')
    expect(within(confirm).getByRole('button', { name: '继续编辑' })).toHaveFocus()
    await userEvent.keyboard('{Escape}')
    await waitFor(() => expect(screen.queryByTestId('provider-discard-confirm')).toBeNull())
    expect(screen.getByTestId('provider-editor')).toBeInTheDocument()
    await userEvent.keyboard('{Escape}')
    await userEvent.click(await screen.findByTestId('provider-discard-ok'))
    expect(await screen.findByTestId('template-picker')).toBeInTheDocument()
  })

  it('Key 无效：就地提示（不再重复错误卡片）；改了 base_url 后结论标为过期', async () => {
    const h = installHarness(
      snapshot(
        { providers: [provider('deepseek', { models: [entry('chat', 'no')] })] },
        { has_key: { deepseek: true } },
      ),
    )
    h.handle('provider_templates', () => TEMPLATES)
    const err = {
      code: 'auth',
      subcode: null,
      message: 'invalid key',
      retryable: false,
      hint: 'errors.hint.check_key',
      status: 401,
      url: 'https://deepseek.example.com/v1/chat/completions',
      retry_after_s: null,
      upstream_request_id: null,
    }
    h.handle('provider_test', () => ({
      models: { ok: false, status: 401, elapsed_ms: 10, url: 'x', error: null },
      stream: { ok: false, status: 401, elapsed_ms: 12, url: err.url, error: err },
      overall: 'failed',
      model_used: 'chat',
      fetched_models: [],
    }))
    renderApp('providers')
    await userEvent.click(await screen.findByTestId('provider-row-deepseek'))
    await userEvent.click(await screen.findByTestId('provider-test'))
    expect(await screen.findByTestId('test-verdict')).toHaveTextContent('连接失败')
    // Key 无效不再用错误卡片重复一遍：步骤行的状态码 + Key 字段的就地提示
    expect(screen.getByTestId('test-report')).toHaveTextContent('HTTP 401')
    expect(screen.getByTestId('test-report')).not.toHaveTextContent('API Key 无效或没有权限')
    expect(h.callsOf('provider_models')).toHaveLength(0)
    // 已保存的 Key 被拒绝：保存状态写明「被拒绝」，下面提示，「更换」是主要操作
    expect(screen.getByTestId('key-saved')).toHaveTextContent('已保存在钥匙串 · 被拒绝')
    expect(screen.getByTestId('key-rejected')).toHaveTextContent('服务拒绝了这个 Key')
    expect(screen.getByTestId('key-change')).toHaveAttribute('data-variant', 'secondary')
    await userEvent.click(screen.getByTestId('key-change'))
    expect(screen.getByTestId('key-rejected')).toBeInTheDocument()
    // 改了 base_url：旧结论过期
    fireEvent.change(screen.getByTestId('provider-base-url'), {
      target: { value: 'https://other.example.com/v1' },
    })
    fireEvent.blur(screen.getByTestId('provider-base-url'))
    expect(await screen.findByTestId('test-stale')).toBeInTheDocument()
    expect(screen.queryByTestId('test-verdict')).toBeNull()
    // 列表里显示「需重新测试」
    await userEvent.click(screen.getByTestId('provider-back'))
    expect(await screen.findByTestId('provider-row-deepseek')).toHaveTextContent('需重新测试')
  })

  /** 新服务还没有模型、/models 又失败：探针没发出去（model_used 为空），stream 只是 not_configured 占位 */
  const skippedProbe = (modelsError: Record<string, unknown>) => ({
    models: {
      ok: false,
      status: modelsError.status,
      elapsed_ms: 30,
      url: 'https://relay.example.com/v1/models',
      error: modelsError,
    },
    stream: {
      ok: false,
      status: null,
      elapsed_ms: 0,
      url: 'https://relay.example.com/v1/chat/completions',
      error: {
        code: 'not_configured',
        subcode: null,
        message: 'no model is available to test streaming; add a model id manually',
        retryable: false,
        hint: null,
        status: null,
        url: null,
        retry_after_s: null,
        upstream_request_id: null,
      },
    },
    overall: 'failed',
    model_used: null,
    fetched_models: [],
  })
  const llmError = (code: string, status: number, hint: string | null = null) => ({
    code,
    subcode: null,
    message: `HTTP ${status}`,
    retryable: false,
    hint,
    status,
    url: 'https://relay.example.com/v1/models',
    retry_after_s: null,
    upstream_request_id: null,
  })

  it('还没有模型时 Key 被拒（/models 401、探针没发）：判为 auth，Key 字段标红，流式一步显示「未执行」', async () => {
    const h = installHarness(snapshot({ providers: [provider('relay')] }))
    h.handle('provider_templates', () => TEMPLATES)
    h.handle('provider_test', () => skippedProbe(llmError('auth', 401, 'errors.hint.check_key')))
    renderApp('providers')
    await userEvent.click(await screen.findByTestId('provider-row-relay'))
    await userEvent.click(await screen.findByTestId('provider-test'))
    expect(await screen.findByTestId('test-verdict')).toHaveTextContent('连接失败')
    // 判为 auth：Key 字段就地标红（这里没有填 Key，提示先填写）
    expect(screen.getByTestId('key-rejected')).toHaveTextContent('服务要求 API Key')
    expect(screen.getByTestId('key-input')).toHaveAttribute('aria-invalid', 'true')
    expect(screen.getByTestId('test-report')).toHaveTextContent('未执行')
    // 不再显示「还没有接入模型」的占位卡片
    expect(screen.getByTestId('test-report')).not.toHaveTextContent('还没有接入模型')
  })

  it('还没有模型时 Base URL 写错（/models 404）：错误卡片给出「添加或去掉 /v1」', async () => {
    const h = installHarness(snapshot({ providers: [provider('relay')] }))
    h.handle('provider_templates', () => TEMPLATES)
    h.handle('provider_test', () => skippedProbe(llmError('not_found', 404, 'errors.hint.try_v1')))
    renderApp('providers')
    await userEvent.click(await screen.findByTestId('provider-row-relay'))
    await userEvent.click(await screen.findByTestId('provider-test'))
    const report = await screen.findByTestId('test-report')
    const card = await within(report).findByTestId('error-card')
    expect(card).toHaveAttribute('data-code', 'not_found')
    expect(card).toHaveTextContent('添加或去掉 /v1')
    expect(screen.queryByTestId('key-rejected')).toBeNull()
  })

  it('已保存的服务：输入框里没保存的 Key 在测试前自动写入', async () => {
    const h = installHarness(snapshot({ providers: [provider('deepseek')] }))
    h.handle('provider_templates', () => TEMPLATES)
    h.handle('provider_test', () => okReport)
    h.handle('provider_models', () => [entry('chat', 'no')])
    renderApp('providers')
    await userEvent.click(await screen.findByTestId('provider-row-deepseek'))
    await userEvent.type(await screen.findByTestId('key-input'), 'sk-new')
    await userEvent.click(screen.getByTestId('provider-test'))
    await waitFor(() => expect(h.callsOf('provider_test')).toHaveLength(1))
    expect(h.callsOf('secret_set')).toEqual([{ provider_id: 'deepseek', key: 'sk-new' }])
    const order = h.calls.map(([c]) => c)
    expect(order.indexOf('secret_set')).toBeLessThan(order.indexOf('provider_test'))
    expect(await screen.findByTestId('key-saved')).toBeInTheDocument()
  })

  it('已保存的服务：改动合并写入；vision 三态手动设置记为 user，方向键切换；删除原地二次确认', async () => {
    const h = installHarness(
      snapshot({
        providers: [
          provider('openai', { models: [entry('gpt-mini', 'unknown'), entry('o4', 'yes')] }),
        ],
        defaults: { translate: { provider_id: 'openai', model_id: 'gpt-mini' }, ocr: null },
      }),
    )
    h.handle('provider_templates', () => TEMPLATES)
    h.handle('provider_delete', () => snapshot())
    renderApp('providers')
    await userEvent.click(await screen.findByTestId('provider-row-openai'))
    const vision = await screen.findByTestId('model-vision-gpt-mini')
    // roving tabindex：只有选中的那一项可以 Tab 到
    const radios = within(vision).getAllByRole('radio')
    expect(radios.map((r) => r.tabIndex)).toEqual([-1, 0, -1])
    await userEvent.click(within(vision).getByRole('radio', { name: '支持图片' }))
    await waitFor(() => expect(h.callsOf('settings_patch')).toHaveLength(1), { timeout: 2000 })
    const first = h.callsOf('settings_patch')[0] as {
      patch: { providers: Array<{ models: Array<{ vision: string; vision_source: string }> }> }
    }
    const providers = first.patch.providers
    expect(providers[0]?.models[0]).toMatchObject({ vision: 'yes', vision_source: 'user' })
    fireEvent.keyDown(within(vision).getByRole('radio', { name: '支持图片' }), { key: 'End' })
    await waitFor(() =>
      expect(within(vision).getByRole('radio', { name: '不支持图片' })).toHaveAttribute(
        'aria-checked',
        'true',
      ),
    )
    expect(within(vision).getByRole('radio', { name: '不支持图片' })).toHaveFocus()

    // 删除：原地变成确认条，焦点在「取消」，Esc 取消
    await userEvent.click(screen.getByTestId('provider-delete'))
    expect(h.callsOf('provider_delete')).toHaveLength(0)
    const confirm = await screen.findByTestId('provider-delete-confirm')
    expect(within(confirm).getByRole('button', { name: '取消' })).toHaveFocus()
    expect(screen.queryByTestId('provider-delete')).toBeNull()
    await userEvent.keyboard('{Escape}')
    await waitFor(() => expect(screen.queryByTestId('provider-delete-confirm')).toBeNull())
    expect(screen.getByTestId('provider-delete')).toHaveFocus()
    await userEvent.click(screen.getByTestId('provider-delete'))
    await userEvent.click(await screen.findByTestId('provider-delete-ok'))
    await waitFor(() => expect(h.callsOf('provider_delete')).toEqual([{ provider_id: 'openai' }]))
    expect(await screen.findByTestId('provider-list')).toBeInTheDocument()
  })

  it('删除前丢掉这个服务还没发出的改动：不会把删掉的服务写回来', async () => {
    // 合并写入的停顿放长：点击再慢也不会让排队的补丁在删除之前「合法地」发出去（不依赖墙钟时间）
    setQueueDelay(60_000)
    const h = installHarness(snapshot({ providers: [provider('openai'), provider('deepseek')] }))
    h.handle('provider_templates', () => TEMPLATES)
    h.handle('provider_delete', ({ provider_id }) => {
      h.state.snap = {
        ...h.state.snap,
        settings: {
          ...h.state.snap.settings,
          providers: h.state.snap.settings.providers.filter((p) => p.id !== provider_id),
        },
      }
      return structuredClone(h.state.snap)
    })
    renderApp('providers')
    await userEvent.click(await screen.findByTestId('provider-row-openai'))
    // 开关类改动排进 300 ms 的合并写入
    await userEvent.click(
      within(await screen.findByTestId('provider-advanced')).getByRole('button'),
    )
    const switches = screen.getAllByRole('switch')
    await userEvent.click(switches[0] as HTMLElement)
    expect(h.callsOf('settings_patch')).toHaveLength(0)
    await userEvent.click(screen.getByTestId('provider-delete'))
    await userEvent.click(await screen.findByTestId('provider-delete-ok'))
    await waitFor(() => expect(h.callsOf('provider_delete')).toHaveLength(1))
    // 断言行为本身：删除之后没有任何补丁再把 openai 写回来；最终只剩 deepseek
    const deletedAt = h.calls.findIndex(([c]) => c === 'provider_delete')
    const after = h.calls.slice(deletedAt + 1).filter(([c]) => c === 'settings_patch')
    for (const [, args] of after) expect(JSON.stringify(args)).not.toContain('"openai"')
    expect(h.callsOf('settings_patch')).toHaveLength(0)
    expect(useSettingsStore.getState().snap?.settings.providers.map((p) => p.id)).toEqual([
      'deepseek',
    ])
    expect(await screen.findByTestId('provider-list')).not.toHaveTextContent('openai.example.com')
  })

  it('「全部停用」保留被默认模型引用的模型', async () => {
    const h = installHarness(
      snapshot({
        providers: [provider('openai', { models: [entry('gpt-mini'), entry('o4')] })],
        defaults: { translate: { provider_id: 'openai', model_id: 'gpt-mini' }, ocr: null },
      }),
    )
    h.handle('provider_templates', () => TEMPLATES)
    renderApp('providers')
    await userEvent.click(await screen.findByTestId('provider-row-openai'))
    const table = await screen.findByTestId('model-table')
    await userEvent.click(within(table).getAllByRole('button', { name: '更多' })[0] as HTMLElement)
    await userEvent.click(await screen.findByRole('menuitem', { name: '全部停用' }))
    await waitFor(() => expect(h.callsOf('settings_patch')).toHaveLength(1), { timeout: 2000 })
    const sent = h.callsOf('settings_patch')[0]?.patch as {
      providers: Array<{ models: Array<{ id: string; enabled: boolean }> }>
    }
    expect(sent.providers[0]?.models).toEqual([
      expect.objectContaining({ id: 'gpt-mini', enabled: true }),
      expect.objectContaining({ id: 'o4', enabled: false }),
    ])
    expect(await screen.findByTestId('settings-notice')).toHaveTextContent('保持启用')
  })

  it('子视图：进入编辑器回到顶部、标题获得焦点；返回时恢复列表滚动并把焦点还给那一行', async () => {
    installHarness(snapshot({ providers: [provider('openai'), provider('deepseek')] }))
    renderApp('providers')
    const row = await screen.findByTestId('provider-row-deepseek')
    const scroller = screen.getByTestId('settings-page-providers')
    scroller.scrollTop = 200
    row.focus()
    await userEvent.keyboard('{Enter}')
    const editor = await screen.findByTestId('provider-editor')
    expect(scroller.scrollTop).toBe(0)
    expect(editor.querySelector('[data-subview-title]')).toHaveFocus()
    await userEvent.click(screen.getByTestId('provider-back'))
    await waitFor(() => expect(screen.getByTestId('provider-row-deepseek')).toHaveFocus())
    expect(scroller.scrollTop).toBe(200)
  })

  it('OCR 默认模型只列 vision 为 Yes / Unknown 的已启用模型', async () => {
    installHarness(
      snapshot({
        providers: [
          provider('a', {
            models: [entry('vis'), entry('blind', 'no'), entry('maybe', 'unknown')],
          }),
        ],
      }),
    )
    renderApp('providers')
    await userEvent.click(await screen.findByTestId('defaults-ocr-select'))
    const menu = await screen.findByTestId('defaults-ocr-select-menu')
    const items = within(menu)
      .getAllByRole('menuitemcheckbox')
      .map((x) => x.textContent ?? '')
    expect(items.some((x) => x.includes('vis'))).toBe(true)
    expect(items.some((x) => x.includes('maybe') && x.includes('未确认支持图片'))).toBe(true)
    expect(items.some((x) => x.includes('blind'))).toBe(false)
  })

  it('孤立 Key 与凭据存储状态的提示', async () => {
    installHarness(snapshot({}, { orphan_keys: 2, keychain: 'session_only' }))
    renderApp('providers')
    expect(await screen.findByTestId('orphan-keys-banner')).toHaveTextContent(
      '有 2 个 Key 暂未从系统凭据存储删除',
    )
    expect(screen.getByTestId('keychain-banner')).toHaveTextContent('只保存在本次运行中')
  })
})

describe('快捷键', () => {
  it('测试按钮：开始测试 → 收到事件显示「已收到」→ 结束测试', async () => {
    const h = installHarness()
    renderApp('hotkeys')
    const row = await screen.findByTestId('hotkey-translate_selection')
    await userEvent.click(within(row).getByTestId('hotkey-translate_selection-test'))
    await waitFor(() =>
      expect(h.callsOf('hotkey_test_begin')).toEqual([
        { target: { kind: 'action', action: 'translate_selection' } },
      ]),
    )
    expect(row).toHaveTextContent('请按下')
    act(() =>
      h.emit('hotkey:test_received', { target: { kind: 'action', action: 'translate_selection' } }),
    )
    await waitFor(() => expect(row).toHaveTextContent('已收到'))
    expect(h.callsOf('hotkey_test_end').length).toBeGreaterThan(0)
  })

  it('测试 10 s 内没收到：提示可能被占用', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true })
    installHarness()
    renderApp('hotkeys')
    const row = await screen.findByTestId('hotkey-ocr')
    fireEvent.click(within(row).getByTestId('hotkey-ocr-test'))
    await act(async () => {
      await vi.advanceTimersByTimeAsync(10_500)
    })
    expect(row).toHaveTextContent('没有收到按键')
  })

  it('默认首选键测试超时：建议改用备选，一键换成这个动作的备选键', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true })
    const h = installHarness()
    h.handle('hotkey_set', () => hotkeys())
    renderApp('hotkeys')
    const row = await screen.findByTestId('hotkey-ocr')
    fireEvent.click(within(row).getByTestId('hotkey-ocr-test'))
    await act(async () => {
      await vi.advanceTimersByTimeAsync(10_500)
    })
    expect(row).toHaveTextContent('建议改用备选组合')
    const use = within(row).getByTestId('hotkey-ocr-use-fallback')
    expect(use).toHaveTextContent('⌃⌥S')
    fireEvent.click(use)
    await waitFor(() =>
      expect(h.callsOf('hotkey_set')).toEqual([
        { target: { kind: 'action', action: 'ocr' }, binding: 'Control+Alt+KeyS', force: false },
      ]),
    )
  })

  it('「使用备选」被 Rust 以冲突拒绝：重新检测，完整报告在录制器原位显示，「仍然使用」以 force 重设', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true })
    const h = installHarness()
    h.handle('hotkey_set', ({ force }) => {
      if (!force) throw { code: 'conflict', message: 'hotkeys.conflict.system_dynamic', llm: null }
      return hotkeys()
    })
    h.handle('hotkey_check', ({ combo }) => ({
      combo,
      display: '⌃⌥S',
      acceptable: true,
      conflicts: [
        {
          level: 'warning',
          source: 'system',
          message_key: 'hotkeys.conflict.system_dynamic',
          params: { combo: '⌃⌥S' },
        },
      ],
    }))
    renderApp('hotkeys')
    const row = await screen.findByTestId('hotkey-ocr')
    fireEvent.click(within(row).getByTestId('hotkey-ocr-test'))
    await act(async () => {
      await vi.advanceTimersByTimeAsync(10_500)
    })
    fireEvent.click(within(row).getByTestId('hotkey-ocr-use-fallback'))
    await waitFor(() => expect(h.callsOf('hotkey_check')).toHaveLength(1))
    const force = await within(row).findByRole('button', { name: '仍然使用' })
    fireEvent.click(force)
    await waitFor(() => expect(h.callsOf('hotkey_set').at(-1)).toMatchObject({ force: true }))
  })

  it('macOS 不显示「Alt + Shift 用于切换输入法」：⌥⇧ 不切换输入源（Windows 照常提示）', async () => {
    const h = installHarness()
    h.handle('neighbor_apps', () => [])
    h.handle('keyboard_advice', () => ({
      has_altgr: false,
      multi_input_lang: true,
      recommend_family: null,
      qq_running: false,
    }))
    const { unmount } = renderApp('hotkeys')
    await screen.findByTestId('hotkey-ocr')
    await act(async () => {
      await new Promise((r) => setTimeout(r, 20))
    })
    expect(screen.queryByTestId('hotkeys-advice')).toBeNull()
    unmount()
    setAppearance({ ...appearance, os: 'windows', uiLanguage: 'zh-CN' })
    renderApp('hotkeys')
    expect(await screen.findByTestId('hotkeys-advice')).toHaveTextContent('Alt+Shift')
  })

  it('同一时刻只有一个目标在测试：开始 B 时 A 回到空闲，A 的计时器不会结束 B 的测试', async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true })
    const h = installHarness()
    renderApp('hotkeys')
    const a = await screen.findByTestId('hotkey-ocr')
    const b = screen.getByTestId('hotkey-clipboard')
    fireEvent.click(within(a).getByTestId('hotkey-ocr-test'))
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000)
    })
    expect(a).toHaveTextContent('请按下')
    fireEvent.click(within(b).getByTestId('hotkey-clipboard-test'))
    await act(async () => {
      await vi.advanceTimersByTimeAsync(100)
    })
    expect(a).not.toHaveTextContent('请按下')
    expect(b).toHaveTextContent('请按下')
    const ends = h.callsOf('hotkey_test_end').length
    // A 原本会在第 10 s 超时：现在不会去结束 B 的测试
    await act(async () => {
      await vi.advanceTimersByTimeAsync(7500)
    })
    expect(h.callsOf('hotkey_test_end')).toHaveLength(ends)
    expect(b).toHaveTextContent('请按下')
    act(() => h.emit('hotkey:test_received', { target: { kind: 'action', action: 'clipboard' } }))
    await waitFor(() => expect(b).toHaveTextContent('已收到'))
    expect(h.callsOf('hotkey_test_end')).toHaveLength(ends + 1)
  })

  it('改用备选键 / 注册失败：写明原来的组合与原因', async () => {
    const s = snapshot()
    s.runtime.hotkeys = s.runtime.hotkeys.map((x, i) =>
      i === 0
        ? {
            ...x,
            effective: 'Control+Alt+KeyD',
            state: 'fallback',
            reason_key: 'hotkeys.reason.taken',
          }
        : i === 1
          ? {
              ...x,
              effective: null,
              state: 'failed',
              reason_key: 'hotkeys.reason.both_unavailable',
            }
          : x,
    )
    installHarness(s)
    renderApp('hotkeys')
    const fallback = await screen.findByTestId('hotkey-translate_selection')
    expect(fallback).toHaveTextContent('⌥D：已被其他程序占用，已改用 ⌃⌥D')
    const failed = screen.getByTestId('hotkey-translate_input')
    expect(failed).toHaveTextContent('⌥⇧D：首选键与备选键都不可用')
    // 没注册上：录制器显示 Rust 尝试的组合（标红），而不是「未设置」
    expect(screen.getByTestId('hotkey-translate_input-recorder')).toHaveTextContent('⌥⇧D')
  })

  it('单项恢复默认、全部恢复默认（二次确认）', async () => {
    const s = snapshot()
    s.runtime.hotkeys = s.runtime.hotkeys.map((x, i) =>
      i === 2 ? { ...x, binding: 'Alt+KeyX', effective: 'Alt+KeyX' } : x,
    )
    const h = installHarness(s)
    h.handle('hotkey_set', () => s.runtime.hotkeys)
    h.handle('hotkey_reset_defaults', () => s.runtime.hotkeys)
    renderApp('hotkeys')
    const restore = await screen.findByTestId('hotkey-ocr-restore')
    expect(screen.getByTestId('hotkey-clipboard-restore')).toBeDisabled()
    await userEvent.click(restore)
    expect(h.callsOf('hotkey_set')).toEqual([
      { target: { kind: 'action', action: 'ocr' }, binding: 'default', force: false },
    ])
    // 「全部恢复默认」：确认就地出现在标题右侧
    await userEvent.click(screen.getByTestId('hotkeys-reset-all'))
    expect(h.callsOf('hotkey_reset_defaults')).toHaveLength(0)
    const confirm = await screen.findByTestId('hotkeys-reset-all-confirm')
    expect(confirm).toHaveTextContent('恢复全部默认？')
    expect(within(confirm).getByRole('button', { name: '取消' })).toHaveFocus()
    await userEvent.click(within(confirm).getByRole('button', { name: '恢复' }))
    await waitFor(() => expect(h.callsOf('hotkey_reset_defaults')).toHaveLength(1))
  })

  it('扩展触发待授权：显示授权入口；Linux X11 中键置灰并写明原因', async () => {
    const s = snapshot(
      {},
      {
        ext_triggers: {
          bobber: 'off',
          double_tap: 'waiting_permission',
          mouse: 'off',
          reason_key: null,
        },
        permissions: {
          accessibility: 'granted',
          screen_recording: 'granted',
          input_monitoring: 'denied',
          lost_after_update: false,
        },
      },
    )
    s.settings.hotkeys.double_tap.enabled = true
    const h = installHarness(s)
    renderApp('hotkeys')
    // 提示出现在受影响的那一项（双击修饰键）下面
    const note = await screen.findByTestId('hotkeys-ext-permission-double_tap')
    expect(note).toHaveTextContent('需要「输入监控」权限')
    expect(screen.queryByTestId('hotkeys-ext-permission-bobber')).toBeNull()
    await userEvent.click(within(note).getByRole('button', { name: '授权输入监控' }))
    expect(h.callsOf('permissions_open')).toEqual([{ kind: 'input_monitoring' }])
  })

  it('X11 上中键不能绑定', async () => {
    setAppearance({ ...appearance, os: 'linux', uiLanguage: 'zh-CN' })
    const s = snapshot({}, {})
    s.runtime.platform = { ...s.runtime.platform, os: 'linux', session: 'x11' }
    s.settings.hotkeys.mouse.enabled = true
    installHarness(s)
    renderApp('hotkeys')
    const middle = await screen.findByTestId('ext-mouse-middle')
    expect(middle).toBeDisabled()
    expect(screen.getByTestId('hotkeys-ext')).toHaveTextContent('X11 下中键用于粘贴选中的文字')
  })
})

describe('翻译', () => {
  function translateSnapshot() {
    return snapshot({ providers: [provider('openai', { models: [entry('gpt-mini')] })] }, {}, [
      style('natural'),
      style('academic', { modified: true, temperature: 0.1 }),
      style('custom-1', {
        builtin: false,
        name: 'Release',
        model_ref: { provider_id: 'gone', model_id: 'x' },
        hotkey: 'Control+Alt+KeyR',
      }),
    ])
  }

  it('风格排序：把手获得焦点时 ↓ 移动并写入 styles_reorder', async () => {
    const h = installHarness(translateSnapshot())
    h.handle('styles_reorder', ({ ids }) =>
      (ids as string[]).map((id) => translateSnapshot().styles.find((s) => s.id === id)),
    )
    renderApp('translate')
    const grip = await screen.findByTestId('style-grip-natural')
    grip.focus()
    fireEvent.keyDown(grip, { key: 'ArrowDown' })
    await waitFor(() =>
      expect(h.callsOf('styles_reorder')).toEqual([{ ids: ['academic', 'natural', 'custom-1'] }]),
    )
    // 绑定的模型不可用：列表里提示回退
    expect(screen.getByTestId('style-row-custom-1')).toHaveTextContent('暂时使用默认模型')
  })

  it('新建风格：不带 temperature / dictionary_mode（由 Rust 按自定义默认值补齐），模板从「自然」开始', async () => {
    const h = installHarness(translateSnapshot())
    h.handle('style_save', ({ style: st }) => [
      ...translateSnapshot().styles,
      { ...(st as object), id: 'custom-new', temperature: 0.3, dictionary_mode: true },
    ])
    renderApp('translate')
    await userEvent.click(await screen.findByTestId('style-create'))
    await waitFor(() => expect(h.callsOf('style_save')).toHaveLength(1))
    const sent = h.callsOf('style_save')[0]?.style as Record<string, unknown>
    expect(sent).not.toHaveProperty('temperature')
    expect(sent).not.toHaveProperty('dictionary_mode')
    expect(sent.id).toBe('')
    expect(sent.system_template).toBe(translateSnapshot().styles[0]?.system_template)
    expect(await screen.findByTestId('style-editor')).toBeInTheDocument()
  })

  it('内置风格还原需要二次确认；未知变量提示', async () => {
    const h = installHarness(translateSnapshot())
    h.handle('style_reset', () => translateSnapshot().styles)
    renderApp('translate')
    await userEvent.click(await screen.findByTestId('style-open-academic'))
    const editor = await screen.findByTestId('style-editor')
    await userEvent.click(within(editor).getByTestId('style-reset'))
    expect(h.callsOf('style_reset')).toHaveLength(0)
    await userEvent.click(await screen.findByTestId('style-reset-ok'))
    await waitFor(() => expect(h.callsOf('style_reset')).toEqual([{ id: 'academic' }]))
    fireEvent.change(screen.getByTestId('style-system'), {
      target: { value: 'Hi {{product}} {{team}}' },
    })
    // 列表分隔符按界面语言（中文顿号与「和」；英文逗号与 and），英文按数量区分单复数
    expect(await screen.findByTestId('style-unknown-vars')).toHaveTextContent(
      // 中文的「和」与两侧的西文之间留空格（与全站中西文混排一致）
      '{{product}} 和 {{team}} 不是可用变量',
    )
    act(() => setAppearance({ ...appearance, os: 'macos', uiLanguage: 'en-US' }))
    expect(screen.getByTestId('style-unknown-vars')).toHaveTextContent(
      '{{product}} and {{team}} aren’t known variables',
    )
  })

  it('风格的写入串行，以最近一次返回的风格为底：模板提交后紧接着点开关，不会把模板改回去', async () => {
    const h = installHarness(translateSnapshot())
    let server = translateSnapshot().styles
    const pending: Array<() => void> = []
    h.handle(
      'style_save',
      ({ style: st }) =>
        new Promise((resolve) => {
          pending.push(() => {
            server = server.map((x) => (x.id === (st as { id: string }).id ? (st as typeof x) : x))
            resolve(server)
          })
        }),
    )
    renderApp('translate')
    await userEvent.click(await screen.findByTestId('style-open-natural'))
    const system = await screen.findByTestId('style-system')
    fireEvent.change(system, { target: { value: 'new template' } })
    fireEvent.blur(system)
    await waitFor(() => expect(h.callsOf('style_save')).toHaveLength(1))
    // 第一份还没返回：点开关只合并字段，不并发发出
    await userEvent.click(screen.getByTestId('style-dictionary'))
    expect(h.callsOf('style_save')).toHaveLength(1)
    pending.shift()?.()
    await waitFor(() => expect(h.callsOf('style_save')).toHaveLength(2))
    const second = h.callsOf('style_save')[1]?.style as {
      system_template: string
      dictionary_mode: boolean
    }
    expect(second).toMatchObject({ system_template: 'new template', dictionary_mode: false })
    pending.shift()?.()
    await waitFor(() =>
      expect(screen.getByTestId('style-dictionary')).toHaveAttribute('aria-checked', 'false'),
    )
    expect(screen.getByTestId('style-system')).toHaveValue('new template')
  })

  it('术语表：编辑后停顿写入；译文留空 = 与原文相同（不译）', async () => {
    const h = installHarness(translateSnapshot())
    h.handle('glossary_get', () => [{ source: 'token', target: '令牌', note: null }])
    renderApp('translate')
    await userEvent.click(await screen.findByTestId('glossary-add'))
    const sources = await screen.findAllByTestId('glossary-source')
    await userEvent.type(sources[1] as HTMLElement, 'Tauri')
    await waitFor(() => expect(h.callsOf('glossary_set').length).toBeGreaterThan(0), {
      timeout: 2000,
    })
    const last = h.callsOf('glossary_set').at(-1)?.entries
    expect(last).toEqual([
      { source: 'token', target: '令牌', note: null },
      { source: 'Tauri', target: 'Tauri', note: null },
    ])
  })
})

describe('其他分组', () => {
  it('剪贴板：清空历史需要确认，保留置顶', async () => {
    const h = installHarness()
    h.handle('clipboard_clear', () => 12)
    renderApp('clipboard')
    await userEvent.click(await screen.findByTestId('clip-clear'))
    expect(h.callsOf('clipboard_clear')).toHaveLength(0)
    // 原地变成确认条：红色触发按钮不再显示（同一屏不会有两个红按钮）
    expect(screen.queryByTestId('clip-clear')).toBeNull()
    await userEvent.click(
      within(await screen.findByTestId('clip-clear-confirm')).getByRole('button', {
        name: '清空',
      }),
    )
    await waitFor(() => expect(h.callsOf('clipboard_clear')).toEqual([{ keep_pinned: true }]))
    expect(await screen.findByTestId('settings-notice')).toHaveTextContent('已清除 12 条记录')
  })

  it('外观：选强调色写入预设名；色块是 roving tabindex 的单选组（方向键切换）', async () => {
    const h = installHarness()
    renderApp('appearance')
    await userEvent.click(await screen.findByTestId('accent-green'))
    expect(h.callsOf('settings_patch')).toEqual([{ patch: { appearance: { accent: 'green' } } }])
    const group = screen.getByRole('radiogroup', { name: '强调色' })
    await waitFor(() =>
      expect(
        within(group)
          .getAllByRole('radio')
          .filter((r) => r.tabIndex === 0)
          .map((r) => r.dataset.testid),
      ).toEqual(['accent-green']),
    )
    screen.getByTestId('accent-green').focus()
    fireEvent.keyDown(screen.getByTestId('accent-green'), { key: 'ArrowRight' })
    expect(h.callsOf('settings_patch').at(-1)).toEqual({
      patch: { appearance: { accent: 'graphite' } },
    })
    expect(screen.getByTestId('accent-graphite')).toHaveFocus()
    fireEvent.keyDown(screen.getByTestId('accent-graphite'), { key: 'Home' })
    expect(h.callsOf('settings_patch').at(-1)).toEqual({
      patch: { appearance: { accent: 'system' } },
    })
  })

  it('OCR：original 细节注明只对 OpenAI 官方端点生效', async () => {
    installHarness()
    renderApp('ocr')
    await userEvent.click(await screen.findByTestId('ocr-detail-select'))
    expect(await screen.findByTestId('ocr-detail-select-menu')).toHaveTextContent(
      '仅 OpenAI 官方端点',
    )
  })

  it('关于：已知新版本时可以安装；第三方许可按许可证分组', async () => {
    const h = installHarness(
      snapshot(
        {},
        {
          update: {
            state: 'available',
            version: '9.9.9',
            notes: '- 新功能',
            progress: null,
            error: null,
          },
        },
      ),
    )
    h.handle('app_info', () => ({
      version: '0.1.0',
      platform: snapshot().runtime.platform,
      config_dir: '/cfg',
      data_dir: '/data',
      log_dir: '/log',
      dev_mode: false,
    }))
    renderApp('about')
    expect(await screen.findByTestId('about-update')).toHaveTextContent('发现新版本 9.9.9')
    await userEvent.click(screen.getByTestId('update-install'))
    expect(h.callsOf('update_install')).toHaveLength(1)
    expect(await screen.findByText('/cfg')).toBeInTheDocument()
    await userEvent.click(screen.getByTestId('about-licenses-toggle'))
    const groups = await screen.findAllByTestId('license-group')
    expect(groups.length).toBeGreaterThan(3)
    expect(screen.getByTestId('about-licenses')).toHaveTextContent('Apache-2.0 OR MIT')
    act(() =>
      h.emit('update:status', {
        state: 'ready',
        version: '9.9.9',
        notes: null,
        progress: null,
        error: null,
      }),
    )
    expect(await screen.findByTestId('update-relaunch')).toBeInTheDocument()
    // 开发者错误文本不直接显示：只给通用说明，原文收在「错误详情」里
    act(() =>
      h.emit('update:status', {
        state: 'error',
        version: null,
        notes: null,
        progress: null,
        error: 'reqwest: connection refused (os error 61)',
      }),
    )
    const update = await screen.findByTestId('about-update')
    await waitFor(() => expect(update).toHaveTextContent('检查更新失败'))
    expect(screen.queryByTestId('update-error-detail')).toBeNull()
    await userEvent.click(within(update).getByRole('button', { name: '错误详情' }))
    expect(await screen.findByTestId('update-error-detail')).toHaveTextContent('connection refused')
  })
})
