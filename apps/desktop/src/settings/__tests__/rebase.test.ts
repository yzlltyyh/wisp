/**
 * store：写入进行中（在途或排队）收到 `settings:changed` 时，以新快照为底重放本地还没确认的改动——别处的改动照常出现，
 * 正在编辑的值不回跳；命令返回的快照同样重放其余在途的改动。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { Settings, SettingsSnapshot } from '../../shared/ipc/types'
import { at } from '../lib/patch'
import { resetWrites } from '../lib/writes'
import {
  applySnapshot,
  flushQueuedPatches,
  loadSettings,
  patchSettings,
  queuePatch,
  queueWrite,
  resetSettingsStore,
  useSettingsStore,
  writeSettings,
} from '../store'
import { installHarness, provider, snapshot } from './harness'

const settings = (): Settings => {
  const snap = useSettingsStore.getState().snap
  if (!snap) throw new Error('no snapshot')
  return snap.settings
}

/** 一个手动放行的命令返回 */
function deferred<T>() {
  let resolve: (v: T) => void = () => {}
  const promise = new Promise<T>((r) => {
    resolve = r
  })
  return { promise, resolve }
}

beforeEach(() => {
  resetSettingsStore()
  resetWrites()
})

afterEach(() => {
  vi.useRealTimers()
  resetSettingsStore()
})

describe('store：在新快照上重放本地改动', () => {
  it('补丁在途时收到别处的改动：别处的改动出现，正在写的值不回跳', async () => {
    const h = installHarness()
    await loadSettings()
    const reply = deferred<SettingsSnapshot>()
    h.handle('settings_patch', () => reply.promise)
    const pending = patchSettings(at('appearance.font_size', 16))
    // 其他设备同步来的改动（Rust 广播的快照里还没有这次补丁）
    const remote = snapshot()
    remote.settings.translate.long_text_threshold = 5000
    applySnapshot(remote)
    expect(settings().translate.long_text_threshold).toBe(5000)
    expect(settings().appearance.font_size).toBe(16)
    // 补丁返回：Rust 的快照里两者都有
    const after = snapshot()
    after.settings.translate.long_text_threshold = 5000
    after.settings.appearance.font_size = 16
    reply.resolve(after)
    await pending
    expect(settings().translate.long_text_threshold).toBe(5000)
    expect(settings().appearance.font_size).toBe(16)
    expect(useSettingsStore.getState().pending).toBe(0)
  })

  it('排队中的服务改动：别处对另一个服务的改动出现，自己的改动保留到发出', async () => {
    vi.useFakeTimers()
    installHarness(snapshot({ providers: [provider('a'), provider('b')] }))
    await loadSettings()
    queueWrite(
      'provider:a',
      (s) => ({
        ...s,
        providers: s.providers.map((p) => (p.id === 'a' ? { ...p, name: 'A local' } : p)),
      }),
      () => {},
    )
    expect(settings().providers[0]?.name).toBe('A local')
    // 别处改了 b、又加了 c
    applySnapshot(
      snapshot({
        providers: [provider('a'), provider('b', { name: 'B remote' }), provider('c')],
      }),
    )
    expect(settings().providers.map((p) => p.name)).toEqual(['A local', 'B remote', 'c'])
    flushQueuedPatches()
  })

  it('同一时刻多个写入：先返回的那个以 Rust 的快照为底，重放其余还在途的改动', async () => {
    const h = installHarness()
    await loadSettings()
    const first = deferred<SettingsSnapshot>()
    h.handle('settings_patch', () => first.promise)
    const a = patchSettings(at('appearance.font_size', 16))
    const second = deferred<SettingsSnapshot>()
    const b = writeSettings(
      (s) => ({ ...s, general: { ...s.general, start_hidden: false } }),
      () => second.promise,
      { command: 'test' },
    )
    // 第一个返回（夹紧后的值，里面还没有第二个写入）
    const afterA = snapshot()
    afterA.settings.appearance.font_size = 16
    first.resolve(afterA)
    await a
    expect(settings().appearance.font_size).toBe(16)
    expect(settings().general.start_hidden).toBe(false)
    const afterB = structuredClone(afterA)
    afterB.settings.general.start_hidden = false
    second.resolve(afterB)
    await b
    expect(settings().general.start_hidden).toBe(false)
    expect(useSettingsStore.getState().pending).toBe(0)
  })

  it('没有写入时：新快照原样替换', async () => {
    installHarness()
    await loadSettings()
    const remote = snapshot()
    remote.settings.appearance.font_size = 17
    applySnapshot(remote)
    expect(useSettingsStore.getState().snap?.settings).toBe(remote.settings)
  })

  it('排队的补丁同样重放；发出之后由返回的快照接替', async () => {
    vi.useFakeTimers()
    const h = installHarness()
    await loadSettings()
    queuePatch('font', at('appearance.font_size', 15))
    const remote = snapshot()
    remote.settings.appearance.theme = 'dark'
    applySnapshot(remote)
    expect(settings().appearance.font_size).toBe(15)
    expect(settings().appearance.theme).toBe('dark')
    h.state.snap = remote
    await vi.advanceTimersByTimeAsync(400)
    expect(h.callsOf('settings_patch')).toEqual([{ patch: { appearance: { font_size: 15 } } }])
    expect(settings().appearance.font_size).toBe(15)
    expect(settings().appearance.theme).toBe('dark')
  })
})
