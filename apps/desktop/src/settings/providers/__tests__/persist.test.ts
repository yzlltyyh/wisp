/**
 * 服务的写入按 id 只写一个服务：编辑合并后 `provider_save`（不带其他服务）、添加 `provider_add`、排序
 * `providers_reorder`；服务已在别处删除时提示原因并回到服务器状态，不把它加回来。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { getAppearance, setAppearance } from '../../../shared/hooks/useAppearance'
import type { Provider } from '../../../shared/ipc/types'
import { installHarness, provider, snapshot } from '../../__tests__/harness'
import { loadSettingsMessages } from '../../i18n'
import { resetWrites } from '../../lib/writes'
import {
  applySnapshot,
  dropQueued,
  loadSettings,
  resetSettingsStore,
  useSettingsStore,
} from '../../store'
import {
  addProvider,
  providerQueueKey,
  queueProviderSave,
  reorderProviders,
  saveProvider,
} from '../persist'

const appearance = getAppearance()
const ids = () => useSettingsStore.getState().snap?.settings.providers.map((p) => p.id)
const names = () => useSettingsStore.getState().snap?.settings.providers.map((p) => p.name)

beforeEach(() => {
  // 文案命名空间（设置窗口在入口处注册）
  loadSettingsMessages()
  resetSettingsStore()
  resetWrites()
  setAppearance({ ...appearance, uiLanguage: 'zh-CN' })
})

afterEach(() => {
  vi.useRealTimers()
  resetSettingsStore()
  setAppearance(appearance)
})

describe('服务的写入', () => {
  it('编辑：立即乐观更新，停顿后只发最后一次，且只带这一个服务', async () => {
    vi.useFakeTimers()
    const h = installHarness(snapshot({ providers: [provider('a'), provider('b')] }))
    await loadSettings()
    queueProviderSave(provider('a', { name: 'A1' }))
    queueProviderSave(provider('a', { name: 'A2' }))
    expect(names()).toEqual(['A2', 'b'])
    expect(h.callsOf('provider_save')).toHaveLength(0)
    await vi.advanceTimersByTimeAsync(400)
    expect(h.callsOf('provider_save')).toEqual([{ provider: provider('a', { name: 'A2' }) }])
    expect(h.callsOf('settings_patch')).toHaveLength(0)
    expect(names()).toEqual(['A2', 'b'])
  })

  it('排队期间别处改了另一个服务：保存之后两边的改动都在', async () => {
    vi.useFakeTimers()
    const h = installHarness(snapshot({ providers: [provider('a'), provider('b')] }))
    await loadSettings()
    queueProviderSave(provider('a', { name: 'A local' }))
    // 其他设备同步来的改动（Rust 那边已经生效并广播）
    h.state.snap = snapshot({ providers: [provider('a'), provider('b', { name: 'B remote' })] })
    applySnapshot(structuredClone(h.state.snap))
    expect(names()).toEqual(['A local', 'B remote'])
    await vi.advanceTimersByTimeAsync(400)
    expect(names()).toEqual(['A local', 'B remote'])
    expect(h.state.snap.settings.providers.map((p) => p.name)).toEqual(['A local', 'B remote'])
  })

  it('服务已在别处删除：提示原因，回到服务器状态，不把它加回来', async () => {
    const h = installHarness(snapshot({ providers: [provider('a', { name: 'Work' })] }))
    await loadSettings()
    // Rust 那边已经删掉了（窗口还没收到广播）
    h.state.snap = snapshot({ providers: [] })
    const ok = await saveProvider(provider('a', { name: 'Work 2' }))
    expect(ok).toBe(false)
    expect(useSettingsStore.getState().notice?.text).toBe(
      '「Work 2」已在其他设备上或配置文件里删除，刚才的改动没有保存。',
    )
    expect(ids()).toEqual([])
    expect(h.state.snap.settings.providers).toEqual([])
  })

  it('其他失败按通用文案提示', async () => {
    const h = installHarness(snapshot({ providers: [provider('a')] }))
    await loadSettings()
    h.handle('provider_save', () => {
      throw { code: 'io', message: 'errors.ipc.io', llm: null }
    })
    expect(await saveProvider(provider('a', { name: 'x' }))).toBe(false)
    expect(useSettingsStore.getState().notice?.text).toMatch(/^没能保存/)
  })

  it('删除前丢掉排队的改动：不会再发出', async () => {
    vi.useFakeTimers()
    const h = installHarness(snapshot({ providers: [provider('a')] }))
    await loadSettings()
    queueProviderSave(provider('a', { name: 'A1' }))
    dropQueued(providerQueueKey('a'))
    await vi.advanceTimersByTimeAsync(400)
    expect(h.callsOf('provider_save')).toHaveLength(0)
    expect(useSettingsStore.getState().pending).toBe(0)
  })

  it('添加：provider_add 只带新服务；排序：providers_reorder 只发 id', async () => {
    const h = installHarness(snapshot({ providers: [provider('a')] }))
    h.handle('providers_reorder', ({ ids: order }) => {
      const byId = new Map(h.state.snap.settings.providers.map((p) => [p.id, p]))
      const providers = (order as string[]).flatMap((id) => {
        const p = byId.get(id)
        return p ? [p] : []
      })
      h.state.snap = { ...h.state.snap, settings: { ...h.state.snap.settings, providers } }
      return structuredClone(h.state.snap)
    })
    await loadSettings()
    const fresh: Provider = provider('b', { name: 'New' })
    expect(await addProvider(fresh)).toBe(true)
    expect(h.callsOf('provider_add')).toEqual([{ provider: fresh }])
    expect(ids()).toEqual(['a', 'b'])
    const pending = reorderProviders(['b', 'a'])
    expect(ids()).toEqual(['b', 'a'])
    expect(await pending).toBe(true)
    expect(h.callsOf('providers_reorder')).toEqual([{ ids: ['b', 'a'] }])
    expect(ids()).toEqual(['b', 'a'])
    expect(h.callsOf('settings_patch')).toHaveLength(0)
  })
})
