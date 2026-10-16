/**
 * 设置 → 同步：未连接 → 连接子视图（测试连接决定同步密码一节：设置 / 输入并合并 / 重新连接；就地校验；NFC；
 * 忘记同步密码时原地确认清除并重新测试；改了连接信息需要重新测试）→ 已连接（状态行的各种状态与 `sync:status` 推送、
 * 错误的原因 + 建议与操作、立即同步、自动同步与同步内容、设备、断开与清除的原地确认）。成功从不弹提示，应用密码不进钥匙串命令。
 */
import { act, render, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { getAppearance, setAppearance } from '../../../shared/hooks/useAppearance'
import { tKey } from '../../../shared/i18n/i18n'
import { boot } from '../../../shared/ipc'
import type {
  SyncDevice,
  SyncErrorKind,
  SyncProbe,
  SyncSettings,
  SyncStatus,
} from '../../../shared/ipc/types'
import { installHarness, ipcError, snapshot, type Harness } from '../../__tests__/harness'
import { resetWrites } from '../../lib/writes'
import { SettingsApp } from '../../SettingsApp'
import { resetSettingsStore, setQueueDelay, useSettingsStore } from '../../store'

const appearance = getAppearance()
const URL_NUTSTORE = 'https://dav.jianguoyun.com/dav/'
const ACCOUNT = 'me@example.com'
const APP_PASSWORD = 'app-password-0000'
const MINUTE = 60_000

const idle: SyncStatus = {
  phase: 'idle',
  last_ok_ms: null,
  pending: false,
  error: null,
  transient: false,
  error_detail: null,
  progress: null,
  device_count: 0,
}

function syncSettings(over: Partial<SyncSettings> = {}): SyncSettings {
  return { ...snapshot().settings.sync, ...over }
}

function connectedSnapshot(status: Partial<SyncStatus> = {}) {
  return snapshot(
    { sync: syncSettings({ enabled: true, server_url: URL_NUTSTORE, username: ACCOUNT }) },
    { sync: { ...idle, last_ok_ms: Date.now() - 2 * MINUTE, device_count: 2, ...status } },
  )
}

function renderSync(h: Harness) {
  boot.route = { kind: 'settings', section: 'sync' }
  render(<SettingsApp />)
  return h
}

const devices: SyncDevice[] = [
  {
    id: 'b',
    name: 'Office PC',
    os: 'windows',
    app_version: '0.2.0',
    last_seen_ms: Date.now() - 3 * MINUTE,
    this_device: false,
  },
  {
    id: 'a',
    name: 'MacBook Air',
    os: 'macos',
    app_version: '0.2.0',
    last_seen_ms: Date.now(),
    this_device: true,
  },
]

/** 连接子视图：打开、填地址 / 账号 / 应用密码 */
async function openConnect(h: Harness, probe: SyncProbe) {
  h.handle('sync_probe', () => probe)
  await userEvent.click(await screen.findByTestId('sync-open'))
  await screen.findByTestId('sync-connect-view')
  await userEvent.type(screen.getByTestId('sync-url'), URL_NUTSTORE)
  await userEvent.type(screen.getByTestId('sync-username'), ACCOUNT)
  await userEvent.type(screen.getByTestId('sync-password'), APP_PASSWORD)
}

/** sync_connect 成功：像 Rust 一样写入 settings.sync，返回状态 */
function acceptConnect(h: Harness, outcome: 'created' | 'joined' | 'restored' = 'created') {
  h.handle('sync_connect', (args) => {
    const account = args.account as { server_url: string; username: string }
    h.state.snap = {
      ...h.state.snap,
      settings: {
        ...h.state.snap.settings,
        sync: {
          ...h.state.snap.settings.sync,
          enabled: true,
          server_url: account.server_url,
          username: account.username,
        },
      },
    }
    return { outcome, status: { ...idle, last_ok_ms: Date.now(), device_count: 1 } }
  })
}

const emptyServer: SyncProbe = { ok: true, error: null, error_detail: null, vault: null }
const withData = (restorable = false): SyncProbe => ({
  ok: true,
  error: null,
  error_detail: null,
  vault: {
    supported: true,
    device_count: 2,
    last_modified_ms: Date.now() - 3 * 24 * 60 * MINUTE,
    restorable,
  },
})

beforeEach(() => {
  resetSettingsStore()
  resetWrites()
  setQueueDelay(null)
  setAppearance({ ...appearance, os: 'macos', uiLanguage: 'zh-CN' })
})

afterEach(() => {
  boot.route = null
  setAppearance(appearance)
})

describe('未连接与连接子视图', () => {
  it('未连接：一句说明 + 连接按钮；地址检查就地提示，未测试前不能连接', async () => {
    const h = renderSync(installHarness())
    const page = await screen.findByTestId('sync-not-connected')
    expect(page).toHaveTextContent('你自己的 WebDAV 存储')
    await openConnect(h, emptyServer)
    expect(screen.getByTestId('sync-connect')).toBeDisabled()
    expect(screen.getByTestId('sync-connect-note')).toHaveTextContent('测试连接通过后才能连接')

    // http 的公网地址：立即就地提示，测试按钮不可用
    const url = screen.getByTestId('sync-url')
    await userEvent.clear(url)
    await userEvent.type(url, 'http://dav.example.com/dav/')
    expect(await screen.findByTestId('sync-url-issue')).toHaveTextContent(
      '只有局域网地址可以用 http',
    )
    expect(screen.getByTestId('sync-test')).toBeDisabled()
    // 局域网的 http 可以
    await userEvent.clear(url)
    await userEvent.type(url, 'http://192.168.1.20:5005/')
    expect(screen.queryByTestId('sync-url-issue')).toBeNull()
    expect(screen.getByTestId('sync-test')).toBeEnabled()
    // 格式不对（没有 https://）：「测试连接」仍可点，点了就地说明原因，不发请求
    await userEvent.clear(url)
    await userEvent.type(url, 'dav.example.com/dav')
    await userEvent.click(screen.getByTestId('sync-test'))
    expect(await screen.findByTestId('sync-url-issue')).toHaveTextContent('地址无效')
    expect(h.callsOf('sync_probe')).toHaveLength(0)
  })

  it('服务器没有数据：设置同步密码（就地校验、可生成），连接时带 NFC 规范化的密码与草稿里的同步内容', async () => {
    const h = renderSync(installHarness())
    acceptConnect(h)
    h.handle('sync_generate_passphrase', () => 'ABCD-EFGH-IJKL-MNOP-QRST-UVWX')
    await openConnect(h, emptyServer)
    // 草稿里关掉剪贴板图片（随连接一起提交，不单独写设置）
    await userEvent.click(screen.getByTestId('sync-content-images-switch'))
    await userEvent.click(screen.getByTestId('sync-test'))

    const report = await screen.findByTestId('sync-probe-report')
    await waitFor(() => expect(report).toHaveTextContent('尚无同步数据'))
    expect(h.callsOf('sync_probe')[0]).toEqual({
      account: { server_url: URL_NUTSTORE, username: ACCOUNT },
      password: APP_PASSWORD,
    })
    const section = await screen.findByTestId('sync-pass-section')
    expect(section).toHaveTextContent('设置同步密码')
    expect(section).toHaveTextContent('忘记同步密码将无法读取服务器上的数据')
    expect(section).toHaveTextContent('系统可能会请求访问钥匙串')

    // 太短
    const pass = screen.getByTestId('sync-pass')
    await userEvent.type(pass, 'short')
    await userEvent.click(screen.getByTestId('sync-connect'))
    expect(await screen.findByTestId('sync-pass-issue')).toHaveTextContent('至少需要 12 个字符')
    // 与应用密码相同
    await userEvent.clear(pass)
    await userEvent.type(pass, APP_PASSWORD.toUpperCase())
    expect(screen.getByTestId('sync-pass-issue')).toHaveTextContent('不能与应用密码')
    // 两次不一致
    await userEvent.clear(pass)
    const accent = String.fromCodePoint(0x301)
    await userEvent.type(pass, `cafe${accent}-sync-pass-1`)
    await userEvent.type(screen.getByTestId('sync-pass-confirm'), 'something-else-1')
    expect(screen.getByTestId('sync-pass-mismatch')).toHaveTextContent('两次输入的不一致')
    expect(h.callsOf('sync_connect')).toHaveLength(0)

    // 生成一个：两栏都填好、显示明文
    await userEvent.click(screen.getByTestId('sync-pass-generate'))
    await waitFor(() =>
      expect(screen.getByTestId('sync-pass')).toHaveValue('ABCD-EFGH-IJKL-MNOP-QRST-UVWX'),
    )
    expect(screen.getByTestId('sync-pass')).toHaveAttribute('type', 'text')
    expect(screen.getByTestId('sync-pass-generated')).toHaveTextContent('先把它存进密码管理器')

    // 改回自己的（分解形式的 é），确认后连接：交给 Rust 的是 NFC
    await userEvent.clear(screen.getByTestId('sync-pass'))
    await userEvent.type(screen.getByTestId('sync-pass'), `cafe${accent}-sync-pass-1`)
    await userEvent.clear(screen.getByTestId('sync-pass-confirm'))
    await userEvent.type(
      screen.getByTestId('sync-pass-confirm'),
      `caf${String.fromCodePoint(0xe9)}-sync-pass-1`,
    )
    await userEvent.click(screen.getByTestId('sync-connect'))

    await waitFor(() => expect(h.callsOf('sync_connect')).toHaveLength(1))
    const sent = h.callsOf('sync_connect')[0] as Record<string, unknown>
    expect(sent.passphrase).toBe(`caf${String.fromCodePoint(0xe9)}-sync-pass-1`)
    expect(sent.password).toBe(APP_PASSWORD)
    expect(sent.account).toEqual({ server_url: URL_NUTSTORE, username: ACCOUNT })
    expect(sent.content).toMatchObject({ images: false, clipboard: true, settings: true })

    // 回到已连接页；成功不弹提示；应用密码从不经过钥匙串命令
    expect(await screen.findByTestId('sync-connected')).toBeInTheDocument()
    expect(screen.getByTestId('sync-account')).toHaveTextContent('dav.jianguoyun.com')
    expect(useSettingsStore.getState().notice).toBeNull()
    expect(h.callsOf('secret_set')).toHaveLength(0)
  })

  it('服务器已有数据：输入同步密码并说明合并；密码不对就地提示；忘记时原地确认清除并重新测试', async () => {
    const h = renderSync(installHarness())
    await openConnect(h, withData())
    await userEvent.click(screen.getByTestId('sync-test'))
    const report = await screen.findByTestId('sync-probe-report')
    await waitFor(() =>
      expect(report).toHaveTextContent('已有 2 台设备的同步数据 · 最近更新于 3 天前'),
    )
    const section = await screen.findByTestId('sync-pass-section')
    expect(section).toHaveTextContent('输入同步密码')
    expect(section).toHaveTextContent('服务器上已有的设置优先')

    h.handle('sync_connect', () => {
      throw ipcError('conflict', 'errors.sync.passphrase')
    })
    await userEvent.type(screen.getByTestId('sync-pass'), 'not-the-right-one')
    await userEvent.click(screen.getByTestId('sync-connect'))
    expect(await screen.findByTestId('sync-pass-error')).toHaveTextContent('同步密码不正确')

    // 忘记了：原地确认，清除时带上表单里的账号与应用密码，然后重新测试 → 变成「设置同步密码」
    h.handle('sync_reset_remote', () => undefined)
    h.handle('sync_probe', () => emptyServer)
    await userEvent.click(screen.getByTestId('sync-forgot'))
    const confirm = await screen.findByTestId('sync-forgot-confirm')
    expect(confirm).toHaveTextContent('其他设备都会停止同步')
    await userEvent.click(within(confirm).getByTestId('sync-forgot-ok'))
    await waitFor(() => expect(h.callsOf('sync_reset_remote')).toHaveLength(1))
    expect(h.callsOf('sync_reset_remote')[0]).toEqual({
      account: { server_url: URL_NUTSTORE, username: ACCOUNT },
      password: APP_PASSWORD,
    })
    await waitFor(() =>
      expect(screen.getByTestId('sync-pass-section')).toHaveTextContent('设置同步密码'),
    )
    expect(screen.getByTestId('sync-pass')).toHaveValue('')
  })

  it('输入同步密码时还没填就点连接：就地提示并把焦点给输入框，不发请求', async () => {
    const h = renderSync(installHarness())
    await openConnect(h, withData())
    await userEvent.click(screen.getByTestId('sync-test'))
    await screen.findByTestId('sync-pass-section')
    expect(screen.queryByTestId('sync-pass-error')).toBeNull()
    await userEvent.click(screen.getByTestId('sync-connect'))
    expect(await screen.findByTestId('sync-pass-error')).toHaveTextContent('请输入同步密码')
    expect(screen.getByTestId('sync-pass')).toHaveFocus()
    expect(screen.getByTestId('sync-pass')).toHaveAttribute('aria-invalid', 'true')
    expect(h.callsOf('sync_connect')).toHaveLength(0)
    // 开始输入后提示消失
    await userEvent.type(screen.getByTestId('sync-pass'), 'x')
    expect(screen.queryByTestId('sync-pass-error')).toBeNull()
  })

  it('设置同步密码时 Rust 拒绝了它：在输入框下说明原因，不再显示「已生成」', async () => {
    const h = renderSync(installHarness())
    h.handle('sync_generate_passphrase', () => 'ABCD-EFGH-IJKL-MNOP-QRST-UVWX')
    h.handle('sync_connect', () => {
      throw ipcError('invalid_argument', 'errors.sync.passphrase_reused')
    })
    await openConnect(h, emptyServer)
    await userEvent.click(screen.getByTestId('sync-test'))
    await screen.findByTestId('sync-pass-section')
    await userEvent.click(screen.getByTestId('sync-pass-generate'))
    await screen.findByTestId('sync-pass-generated')
    await userEvent.click(screen.getByTestId('sync-connect'))
    const error = await screen.findByTestId('sync-pass-error')
    expect(error).toHaveTextContent(tKey('errors.sync.passphrase_reused') as string)
    expect(error).toHaveAttribute('role', 'alert')
    expect(screen.queryByTestId('sync-pass-generated')).toBeNull()
  })

  it('本机就是这份数据：「重新连接」；改了连接信息后旧结论作废，需要重新测试', async () => {
    const h = renderSync(installHarness())
    acceptConnect(h, 'restored')
    await openConnect(h, withData(true))
    await userEvent.click(screen.getByTestId('sync-test'))
    const section = await screen.findByTestId('sync-pass-section')
    expect(section).toHaveTextContent('本机尚未上传的改动会继续同步')
    expect(screen.getByTestId('sync-connect')).toHaveTextContent('重新连接')

    await userEvent.type(screen.getByTestId('sync-username'), 'x')
    expect(await screen.findByTestId('sync-probe-stale')).toBeInTheDocument()
    expect(screen.getByTestId('sync-connect')).toBeDisabled()
    await userEvent.type(screen.getByTestId('sync-username'), '{Backspace}')
    expect(screen.queryByTestId('sync-probe-stale')).toBeNull()

    await userEvent.type(screen.getByTestId('sync-pass'), 'correct horse battery')
    await userEvent.click(screen.getByTestId('sync-connect'))
    expect(await screen.findByTestId('sync-connected')).toBeInTheDocument()
  })

  it('测试失败：失败的一步写原因，建议在报告下面，技术细节可折叠', async () => {
    const h = renderSync(installHarness())
    await openConnect(h, { ok: false, error: 'auth', error_detail: 'http 401', vault: null })
    await userEvent.click(screen.getByTestId('sync-test'))
    const report = await screen.findByTestId('sync-probe-report')
    await waitFor(() => expect(report).toHaveTextContent('账号或应用密码不正确'))
    // 表单里的建议是就地可做的（不提已连接页的「更新密码…」）
    expect(screen.getByTestId('sync-probe-advice')).toHaveTextContent(
      '检查账号与应用密码后重新测试',
    )
    expect(report).toHaveTextContent('未执行')
    expect(screen.queryByTestId('sync-pass-section')).toBeNull()
    expect(screen.getByTestId('sync-connect')).toBeDisabled()
  })

  it('测试连接本身出错（例如读不到已保存的密码）：第一步只标失败，不写「已登录」', async () => {
    const h = renderSync(installHarness())
    await openConnect(h, emptyServer)
    h.handle('sync_probe', () => {
      throw ipcError('keychain', 'errors.ipc.keychain')
    })
    await userEvent.click(screen.getByTestId('sync-test'))
    expect(await screen.findByTestId('sync-probe-error')).toBeInTheDocument()
    const report = screen.getByTestId('sync-probe-report')
    expect(report).toHaveTextContent('连接与登录')
    expect(report).not.toHaveTextContent('已登录')
    expect(report).toHaveTextContent('未执行')
  })
})

describe('已连接', () => {
  it('主机 · 账号与安静的状态行；sync:status 推送进度；暂时性失败不标红', async () => {
    const h = installHarness(connectedSnapshot())
    h.handle('sync_devices', () => devices)
    renderSync(h)
    await screen.findByTestId('sync-connected')
    expect(screen.getByTestId('sync-account')).toHaveTextContent(`dav.jianguoyun.com·${ACCOUNT}`)
    expect(screen.getByTestId('sync-status-line')).toHaveTextContent('已同步 · 2 分钟前')

    act(() =>
      h.emit('sync:status', { ...idle, phase: 'syncing', progress: { done: 120, total: 1000 } }),
    )
    await waitFor(() =>
      expect(screen.getByTestId('sync-status-line')).toHaveTextContent('同步中 · 120 / 1000'),
    )
    expect(screen.getByTestId('sync-progress')).toHaveStyle({ transform: 'scaleX(0.12)' })

    act(() =>
      h.emit('sync:status', {
        ...idle,
        phase: 'error',
        error: 'network',
        transient: true,
        error_detail: 'timeout',
      }),
    )
    await waitFor(() =>
      expect(screen.getByTestId('sync-status-line')).toHaveTextContent(
        '暂时无法连接，稍后自动重试',
      ),
    )
    expect(screen.getByTestId('sync-status-line')).toHaveAttribute('data-tone', 'warning')
    expect(screen.queryByTestId('sync-progress')).toBeNull()

    act(() => h.emit('sync:status', { ...idle, pending: true }))
    await waitFor(() =>
      expect(screen.getByTestId('sync-status-line')).toHaveTextContent('有改动等待同步'),
    )
  })

  it('需要处理的错误：原因 + 建议 + 操作；「更新密码…」进入只读地址的子视图', async () => {
    const h = installHarness(
      connectedSnapshot({
        phase: 'error',
        error: 'auth',
        transient: false,
        error_detail: 'http 401',
      }),
    )
    renderSync(h)
    await screen.findByTestId('sync-connected')
    const line = screen.getByTestId('sync-status-line')
    expect(line).toHaveTextContent('账号或应用密码不正确')
    expect(line).toHaveAttribute('data-tone', 'danger')
    expect(screen.getByTestId('sync-status-advice')).toHaveTextContent('重新输入应用密码')
    await userEvent.click(screen.getByTestId('sync-update'))
    const view = await screen.findByTestId('sync-connect-view')
    expect(view).toHaveAttribute('data-mode', 'update')
    expect(screen.getByTestId('sync-url-readonly')).toHaveTextContent(URL_NUTSTORE)
    expect(screen.queryByTestId('sync-url')).toBeNull()
    // 留空 = 沿用已保存的应用密码：可以直接测试，密码传 null
    h.handle('sync_probe', () => withData(true))
    await userEvent.click(screen.getByTestId('sync-test'))
    await waitFor(() => expect(h.callsOf('sync_probe')).toHaveLength(1))
    expect(h.callsOf('sync_probe')[0]?.password).toBeNull()
  })

  it('更新密码时忘了同步密码：应用密码留空也能清除并在原地设置新的同步密码、连接', async () => {
    const h = installHarness(connectedSnapshot({ phase: 'error', error: 'vault_reset' }))
    acceptConnect(h)
    renderSync(h)
    await userEvent.click(await screen.findByTestId('sync-fix'))
    await screen.findByTestId('sync-connect-view')
    h.handle('sync_probe', () => withData())
    await userEvent.click(screen.getByTestId('sync-test'))
    await screen.findByTestId('sync-pass-section')

    // 像 Rust 一样：清除本机正在同步的服务器 → 本机断开（地址与账号保留），应用密码仍然保存着
    h.handle('sync_reset_remote', () => {
      h.state.snap = {
        ...h.state.snap,
        settings: {
          ...h.state.snap.settings,
          sync: { ...h.state.snap.settings.sync, enabled: false },
        },
      }
    })
    h.handle('sync_probe', () => emptyServer)
    await userEvent.click(screen.getByTestId('sync-forgot'))
    const confirm = await screen.findByTestId('sync-forgot-confirm')
    await userEvent.click(within(confirm).getByTestId('sync-forgot-ok'))
    await waitFor(() => expect(h.callsOf('sync_reset_remote')).toHaveLength(1))
    expect(h.callsOf('sync_reset_remote')[0]).toEqual({
      account: { server_url: URL_NUTSTORE, username: ACCOUNT },
      password: null,
    })
    // 仍在同一个子视图里重新测试（应用密码继续留空），同步密码一节变成「设置」
    await waitFor(() =>
      expect(screen.getByTestId('sync-pass-section')).toHaveTextContent('设置同步密码'),
    )
    expect(h.callsOf('sync_probe').at(-1)?.password).toBeNull()

    await userEvent.type(screen.getByTestId('sync-pass'), 'a-brand-new-sync-pass')
    await userEvent.type(screen.getByTestId('sync-pass-confirm'), 'a-brand-new-sync-pass')
    await userEvent.click(screen.getByTestId('sync-connect'))
    await waitFor(() => expect(h.callsOf('sync_connect')).toHaveLength(1))
    expect(h.callsOf('sync_connect')[0]).toMatchObject({
      account: { server_url: URL_NUTSTORE, username: ACCOUNT },
      password: null,
      passphrase: 'a-brand-new-sync-pass',
    })
    expect(await screen.findByTestId('sync-connected')).toBeInTheDocument()
  })

  it.each([
    ['vault_reset', 'sync-fix', '重新连接…'],
    ['passphrase', 'sync-update', '更新密码…'],
    ['credentials_missing', 'sync-update', '更新密码…'],
    ['auth', 'sync-update', '更新密码…'],
  ] as const)('%s：主要操作是 %s「%s」，同名按钮只有一个', async (error, testId, action) => {
    renderSync(installHarness(connectedSnapshot({ phase: 'error', error, transient: false })))
    await screen.findByTestId('sync-connected')
    const button = screen.getByTestId(testId)
    expect(button).toHaveTextContent(action)
    expect(button).toHaveAttribute('data-variant', 'primary')
    expect(screen.getAllByRole('button', { name: action })).toHaveLength(1)
  })

  it('每一种错误都有两种语言的「原因 + 建议」', () => {
    const kinds: SyncErrorKind[] = [
      'network',
      'auth',
      'forbidden',
      'not_found',
      'quota',
      'server',
      'https_required',
      'passphrase',
      'vault_reset',
      'format_too_new',
      'credentials_missing',
      'keychain',
      'corrupt',
      'internal',
    ]
    for (const lang of ['zh-CN', 'en-US'] as const) {
      setAppearance({ ...appearance, uiLanguage: lang })
      for (const kind of kinds) {
        expect(tKey(`settings.sync.error.${kind}.reason`), `${lang} ${kind}`).toBeTruthy()
        expect(tKey(`settings.sync.error.${kind}.advice`), `${lang} ${kind}`).toBeTruthy()
      }
    }
  })

  it('立即同步、自动同步与同步内容（API Key 跟随「设置与模型服务」）', async () => {
    const h = installHarness(connectedSnapshot())
    h.handle('sync_now', () => ({ ...idle, phase: 'syncing', progress: null }))
    renderSync(h)
    await screen.findByTestId('sync-connected')
    await userEvent.click(screen.getByTestId('sync-now'))
    await waitFor(() => expect(h.callsOf('sync_now')).toHaveLength(1))
    await waitFor(() => expect(screen.getByTestId('sync-status-line')).toHaveTextContent('同步中…'))

    await userEvent.click(screen.getByTestId('sync-auto-switch'))
    await waitFor(() => expect(h.callsOf('settings_patch')).toHaveLength(1))
    expect(h.callsOf('settings_patch')[0]?.patch).toEqual({ sync: { auto: false } })

    expect(screen.getByTestId('sync-content-api_keys-switch')).toBeEnabled()
    await userEvent.click(screen.getByTestId('sync-content-settings-switch'))
    await waitFor(() => expect(h.callsOf('settings_patch')).toHaveLength(2))
    expect(h.callsOf('settings_patch')[1]?.patch).toEqual({
      sync: { content: { settings: false } },
    })
    expect(screen.getByTestId('sync-content-api_keys-switch')).toBeDisabled()

    // 关闭剪贴板历史：说明服务器上的记录会被移除
    await userEvent.click(screen.getByTestId('sync-content-clipboard-switch'))
    expect(await screen.findByTestId('sync-content-clipboard')).toHaveTextContent(
      '本机上传过的记录会从服务器上移除',
    )
    expect(screen.getByTestId('sync-local-note')).toHaveTextContent('快捷键按系统分别同步')
    expect(useSettingsStore.getState().notice).toBeNull()
  })

  it('验证码是「剪贴板历史」的子开关：单独关闭只写 verification_codes；剪贴板历史关闭时禁用并显示为关', async () => {
    const h = installHarness(connectedSnapshot())
    renderSync(h)
    await screen.findByTestId('sync-connected')
    const codes = () => screen.getByTestId('sync-content-verification_codes-switch')
    expect(codes()).toBeEnabled()
    expect(codes()).toBeChecked()
    await userEvent.click(codes())
    await waitFor(() => expect(h.callsOf('settings_patch')).toHaveLength(1))
    expect(h.callsOf('settings_patch')[0]?.patch).toEqual({
      sync: { content: { verification_codes: false } },
    })
    await userEvent.click(screen.getByTestId('sync-content-clipboard-switch'))
    await waitFor(() => expect(h.callsOf('settings_patch')).toHaveLength(2))
    expect(codes()).toBeDisabled()
    expect(codes()).not.toBeChecked()
    expect(screen.getByTestId('sync-content-verification_codes')).toHaveTextContent(
      '需要先同步剪贴板历史',
    )
  })

  it('设备：本机在前并标「本机」，其余显示系统与最近同步时间', async () => {
    const h = installHarness(connectedSnapshot())
    h.handle('sync_devices', () => devices)
    renderSync(h)
    await screen.findByTestId('sync-connected')
    const rows = await screen.findAllByTestId('sync-device')
    expect(rows[0]).toHaveTextContent('MacBook Air')
    expect(rows[0]).toHaveTextContent('本机')
    expect(rows[1]).toHaveTextContent('Office PC')
    expect(rows[1]).toHaveTextContent('Windows · 最近同步于 3 分钟前')
  })

  it('断开同步：有未上传改动时确认文案注明；确认后回到未连接页', async () => {
    const h = installHarness(connectedSnapshot({ pending: true }))
    h.handle('sync_disconnect', () => {
      h.state.snap = {
        ...h.state.snap,
        settings: {
          ...h.state.snap.settings,
          sync: { ...h.state.snap.settings.sync, enabled: false },
        },
      }
    })
    renderSync(h)
    await screen.findByTestId('sync-connected')
    await userEvent.click(screen.getByTestId('sync-disconnect'))
    const confirm = await screen.findByTestId('sync-disconnect-confirm')
    expect(confirm).toHaveTextContent('本机还有改动没有上传')
    await userEvent.click(within(confirm).getByTestId('sync-disconnect-ok'))
    await waitFor(() => expect(h.callsOf('sync_disconnect')).toHaveLength(1))
    expect(await screen.findByTestId('sync-not-connected')).toBeInTheDocument()
  })

  it('清除服务器上的数据：原地确认，用已连接的账号（不带参数）', async () => {
    const h = installHarness(connectedSnapshot())
    renderSync(h)
    await screen.findByTestId('sync-connected')
    await userEvent.click(screen.getByTestId('sync-reset'))
    const confirm = await screen.findByTestId('sync-reset-confirm')
    expect(confirm).toHaveTextContent('所有设备都会停止同步')
    await userEvent.click(within(confirm).getByTestId('sync-reset-ok'))
    await waitFor(() => expect(h.callsOf('sync_reset_remote')).toHaveLength(1))
    expect(h.callsOf('sync_reset_remote')[0]).toEqual({ account: null, password: null })
  })
})
