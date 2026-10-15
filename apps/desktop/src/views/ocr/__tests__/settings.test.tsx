/**
 * OCR 视图的设置快照：`settings_get` 失败时只重试拉取，`settings:changed` 只订阅一次（OCR 与翻译视图各自持有快照）。
 */
import { act, render } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { setTransport, type Transport } from '../../../shared/ipc'
import type { SettingsSnapshot } from '../../../shared/ipc/types'
import { useSettingsSnapshot } from '../settings'

let listens: string[]
let gets: number
let failGet: boolean
let emit: ((payload: unknown) => void) | null

function fake(): Transport {
  return {
    kind: 'mock',
    invoke: <T,>(cmd: string) => {
      if (cmd !== 'settings_get') return Promise.resolve(undefined as T)
      gets += 1
      if (failGet) return Promise.reject({ code: 'internal', message: 'not ready', llm: null })
      return Promise.resolve({ marker: `get-${gets}` } as unknown as T)
    },
    openStream: () => Promise.reject(new Error('unused')),
    listen: (event, cb) => {
      listens.push(event)
      emit = cb as (payload: unknown) => void
      return Promise.resolve(() => {})
    },
    window: {} as Transport['window'],
    protocolUrl: (p) => p,
  }
}

function Probe({ seen }: { seen: Array<SettingsSnapshot | null> }) {
  seen.push(useSettingsSnapshot())
  return null
}

beforeEach(() => {
  listens = []
  gets = 0
  failGet = true
  emit = null
  setTransport(fake())
})
afterEach(() => setTransport(null))

describe('设置快照', () => {
  it('settings_get 失败后再次订阅只重试拉取，事件监听不重复', async () => {
    const seen: Array<SettingsSnapshot | null> = []
    const first = render(<Probe seen={seen} />)
    await act(async () => {})
    expect(gets).toBe(1)
    expect(listens).toEqual(['settings:changed'])
    first.unmount()
    failGet = false
    render(<Probe seen={seen} />)
    await act(async () => {})
    expect(gets).toBe(2)
    expect(listens).toEqual(['settings:changed'])
    expect((seen.at(-1) as unknown as { marker: string }).marker).toBe('get-2')
    // 之后的变更经事件到达
    act(() => emit?.({ marker: 'event' }))
    expect((seen.at(-1) as unknown as { marker: string }).marker).toBe('event')
  })
})
