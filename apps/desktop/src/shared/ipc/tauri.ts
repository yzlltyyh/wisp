/**
 * 真实传输层：只依赖 `@tauri-apps/api/core` 与 `@tauri-apps/api/event`；
 * 窗口门面惰性导入 `@tauri-apps/api/window`，Overlay 入口因此只会打进 core + event。
 */
import { Channel, convertFileSrc, invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import type * as WindowApi from '@tauri-apps/api/window'
import { WispIpcError, type InvokeArgs, type Transport, type WindowFacade } from './transport'
import type { EventMap, StreamEvent } from './types'

type WindowModule = typeof WindowApi

export function createTauriTransport(label: string): Transport {
  // 只接收发给本窗口的事件（emit_to(label)）以及广播（emit）
  const target = { kind: 'WebviewWindow', label } as const

  let windowModule: Promise<WindowModule> | null = null
  const currentWindow = () =>
    (windowModule ??= import('@tauri-apps/api/window')).then((m) => m.getCurrentWindow())

  const call = <T>(cmd: string, args?: InvokeArgs): Promise<T> =>
    invoke<T>(cmd, args).catch((error: unknown) => {
      throw WispIpcError.from(cmd, error)
    })

  const window: WindowFacade = {
    label,
    startDragging: () => currentWindow().then((w) => w.startDragging()),
    close: () => currentWindow().then((w) => w.close()),
  }

  return {
    kind: 'tauri',
    invoke: call,
    openStream<T>(cmd: string, args: InvokeArgs, onEvent: (event: StreamEvent) => void) {
      const channel = new Channel<StreamEvent>(onEvent)
      return call<T>(cmd, { ...args, on_event: channel })
    },
    listen<K extends keyof EventMap>(event: K, cb: (payload: EventMap[K]) => void) {
      return listen<EventMap[K]>(event, (e) => cb(e.payload), { target })
    },
    window,
    protocolUrl: (path) => convertFileSrc(path, 'wisp'),
  }
}
