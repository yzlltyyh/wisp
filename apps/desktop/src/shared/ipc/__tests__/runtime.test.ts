import { afterEach, describe, expect, it, vi } from 'vitest'
import { getTransport, setTransport, whenReady } from '../runtime'
import { WispIpcError, type Transport } from '../transport'

afterEach(() => setTransport(null))

const fake = (kind: Transport['kind']): Transport => ({
  kind,
  invoke: async () => undefined as never,
  openStream: async () => undefined as never,
  listen: async () => () => {},
  window: { label: 'panel', startDragging: async () => {}, close: async () => {} },
  protocolUrl: (p) => `wisp://localhost/${p}`,
})

describe('runtime', () => {
  it('传输层已就绪时 whenReady 同步执行（不引入异步跳转）', () => {
    setTransport(fake('tauri'))
    const fn = vi.fn<() => void>()
    whenReady(fn)
    expect(fn).toHaveBeenCalledTimes(1)
  })

  it('浏览器测试环境默认选择 mock，并在加载完成后切换为真实 mock 实现', async () => {
    const deferred = getTransport()
    expect(deferred.kind).toBe('mock')
    await new Promise<void>((resolve) => whenReady(resolve))
    const loaded = getTransport()
    expect(loaded.kind).toBe('mock')
    await expect(loaded.invoke('app_info')).resolves.toMatchObject({ version: expect.any(String) })
  })
})

describe('WispIpcError', () => {
  it('保留 IpcError 的 code / message / llm', () => {
    const e = WispIpcError.from('settings_get', { code: 'keychain', message: 'denied', llm: null })
    expect(e).toBeInstanceOf(Error)
    expect(e).toMatchObject({
      name: 'WispIpcError',
      code: 'keychain',
      message: 'denied',
      command: 'settings_get',
    })
  })

  it('字符串与未知值归为 internal', () => {
    expect(WispIpcError.from('x', 'boom')).toMatchObject({ code: 'internal', message: 'boom' })
    expect(WispIpcError.from('x', new Error('bad'))).toMatchObject({
      code: 'internal',
      message: 'bad',
    })
    expect(WispIpcError.from('x', { code: 'nope', message: 'm' }).code).toBe('internal')
  })

  it('Tauri 的参数反序列化失败归为 invalid_argument', () => {
    // tauri 2 InvokeError 的原文格式（非法枚举值、缺参数、类型不对都走这里，进不到命令函数体）
    const raw =
      'invalid args `reason` for command `panel_hide`: unknown variant `nope`, expected one of `escape`, `outside_click`'
    expect(WispIpcError.from('panel_hide', raw)).toMatchObject({
      code: 'invalid_argument',
      message: raw,
      command: 'panel_hide',
      llm: null,
    })
    expect(
      WispIpcError.from(
        'panel_ready',
        'invalid args `content_height` for command `panel_ready`: command panel_ready missing required key content_height',
      ).code,
    ).toBe('invalid_argument')
    expect(WispIpcError.from('x', new Error('invalid args `a` for command `x`: bad')).code).toBe(
      'invalid_argument',
    )
    // 只认前缀：中间出现同样的字样不算
    expect(WispIpcError.from('x', 'oops: invalid args `a`').code).toBe('internal')
  })
})
