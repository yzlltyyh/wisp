/**
 * mock 的同步：假服务器按 Rust 的契约近似——测试连接的失败放进报告、地址 / 同步密码不合规抛 invalid_argument、
 * 同步密码不对抛 conflict（message 是 `errors.sync.*`）；连接后写入 settings.sync 并在后台带进度同步；断开、清除、
 * 生成同步密码；以及设置窗口的同步场景都已登记。
 */
import { afterEach, beforeAll, describe, expect, it, vi } from 'vitest'
import * as api from '../../commands'
import { COMMAND_NAMES } from '../../commands'
import { getTransport, whenReady } from '../../runtime'
import type { SyncStatus } from '../../types'

beforeAll(async () => {
  await new Promise<void>((resolve) => whenReady(resolve))
})

afterEach(() => {
  vi.useRealTimers()
})

const mock = () => {
  const h = window.__WISP_MOCK__
  if (!h) throw new Error('mock 未安装')
  return h
}

const account = { server_url: 'https://dav.jianguoyun.com/dav/', username: 'me@example.com' }
const content = { settings: true, api_keys: true, clipboard: true, images: true, ocr_history: true }

describe('mock 同步', () => {
  it('每条同步命令都有默认处理函数', () => {
    const registered = new Set(mock().router.registered())
    const sync = COMMAND_NAMES.filter((c) => c.startsWith('sync_'))
    expect(sync).toHaveLength(8)
    expect(sync.filter((c) => !registered.has(c))).toEqual([])
  })

  it('地址与同步密码的校验、测试连接、创建、后台进度、设备、断开与清除', async () => {
    vi.useFakeTimers()
    const { state } = mock()
    state.sync.server.probeMs = 0
    state.sync.server.connectMs = 0
    const statuses: SyncStatus[] = []
    const un = await getTransport().listen('sync:status', (s) => statuses.push(s))

    await expect(
      api.syncProbe({ ...account, server_url: 'http://dav.example.com/' }, 'pw'),
    ).rejects.toMatchObject({ code: 'invalid_argument', message: 'errors.sync.https_required' })

    const probing = api.syncProbe(account, 'pw')
    await vi.advanceTimersByTimeAsync(0)
    expect(await probing).toEqual({ ok: true, error: null, error_detail: null, vault: null })

    await expect(api.syncConnect(account, 'pw', 'short', content)).rejects.toMatchObject({
      code: 'invalid_argument',
      message: 'errors.sync.passphrase_weak',
    })
    await expect(api.syncConnect(account, 'pw', 'ME@EXAMPLE.COM', content)).rejects.toMatchObject({
      message: 'errors.sync.passphrase_reused',
    })

    const connecting = api.syncConnect(account, 'pw', 'correct-horse-battery', content)
    await vi.advanceTimersByTimeAsync(0)
    const result = await connecting
    expect(result.outcome).toBe('created')
    expect(state.settings.sync).toMatchObject({ enabled: true, ...account })
    expect(state.sync.status.phase).toBe('syncing')
    await vi.advanceTimersByTimeAsync(5_000)
    expect(state.sync.status.phase).toBe('idle')
    expect(statuses.some((s) => s.progress !== null && s.progress.done > 0)).toBe(true)
    expect(await api.syncDevices()).toEqual([expect.objectContaining({ this_device: true })])

    // 另一台设备用错的同步密码加入
    state.settings.sync.enabled = false
    state.sync.server.restorable = false
    // 先接住拒绝再推进计时器：拒绝发生在推进期间，不能成为未处理的拒绝
    const wrong = api.syncConnect(account, 'pw', 'not-the-right-one', content).then(
      () => null,
      (e: unknown) => e,
    )
    await vi.advanceTimersByTimeAsync(0)
    expect(await wrong).toMatchObject({ code: 'conflict', message: 'errors.sync.passphrase' })
    state.settings.sync.enabled = true

    await api.syncDisconnect()
    expect(state.settings.sync.enabled).toBe(false)
    expect(state.sync.status.phase).toBe('off')
    await expect(api.syncNow()).rejects.toMatchObject({ message: 'errors.sync.not_connected' })

    const reset = api.syncResetRemote(account, 'pw')
    await vi.advanceTimersByTimeAsync(0)
    await reset
    expect(state.sync.server.vault).toBeNull()
    un()
  })

  it('应用密码留空 = 已保存的那一份：清除本机正在同步的服务器后仍保存着，可以原地重新开始；断开会删除它', async () => {
    vi.useFakeTimers()
    const { state } = mock()
    state.sync.server.probeMs = 0
    state.sync.server.connectMs = 0
    const settle = async <T>(p: Promise<T>): Promise<T> => {
      const caught = p.then(
        (v) => ({ v }),
        (e: unknown) => ({ e }),
      )
      await vi.advanceTimersByTimeAsync(5_000)
      const r = await caught
      if ('e' in r) throw r.e
      return r.v
    }
    // 没有保存过：测试连接报告找不到已保存的密码，连接被拒绝
    state.sync.saved = null
    state.settings.sync = { ...state.settings.sync, enabled: false }
    expect(await settle(api.syncProbe(account, null))).toMatchObject({
      ok: false,
      error: 'credentials_missing',
    })
    await expect(
      settle(api.syncConnect(account, null, 'correct-horse-battery', content)),
    ).rejects.toMatchObject({ message: 'errors.sync.credentials_missing' })

    // 连接之后保存；「更新密码…」里忘了同步密码 → 清除（应用密码留空）→ 留空重新测试、设置新的同步密码并连接
    state.sync.server.vault = null
    await settle(api.syncConnect(account, 'app-password-0000', 'correct-horse-battery', content))
    await settle(api.syncResetRemote(account, null))
    expect(state.settings.sync.enabled).toBe(false)
    expect(state.sync.server.vault).toBeNull()
    expect(await settle(api.syncProbe(account, null))).toMatchObject({ ok: true, vault: null })
    const again = await settle(api.syncConnect(account, null, 'a-brand-new-pass', content))
    expect(again.outcome).toBe('created')
    // 新的同步密码同样不能与保存着的应用密码相同
    state.sync.server.vault = null
    await expect(
      settle(api.syncConnect(account, null, ' APP-PASSWORD-0000 ', content)),
    ).rejects.toMatchObject({ message: 'errors.sync.passphrase_reused' })

    await settle(api.syncDisconnect())
    expect(state.sync.saved).toBeNull()
    expect(await settle(api.syncProbe(account, null))).toMatchObject({
      error: 'credentials_missing',
    })
  })

  it('生成的同步密码：24 个 base32 字符，每 4 个一组', async () => {
    expect(await api.syncGeneratePassphrase()).toMatch(/^[A-Z2-7]{4}(-[A-Z2-7]{4}){5}$/)
  })

  it('设置窗口的同步场景都已登记（未连接 / 连接各阶段 / 已连接 / 同步中 / 每一种错误）', async () => {
    const ids = (await mock().listScenarios()).map((s) => s.id)
    for (const id of [
      'settings.sync',
      'settings.sync-probing',
      'settings.sync-empty-server',
      'settings.sync-existing',
      'settings.sync-restorable',
      'settings.sync-connected',
      'settings.sync-syncing',
      'settings.sync-error-auth',
      'settings.sync-error-vault_reset',
      'settings.sync-error-internal',
    ])
      expect(ids).toContain(id)
    expect(ids.filter((id) => id.startsWith('settings.sync-error-'))).toHaveLength(14)
  })
})
