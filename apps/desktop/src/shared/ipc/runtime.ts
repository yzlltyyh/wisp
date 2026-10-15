/**
 * 传输层的选择与就绪。
 *
 * - Tauri 内：`tauri.ts`（静态导入，零额外开销）；
 * - 浏览器 + dev / `--mode mock` / vitest：动态导入 `mock/`（`__WISP_MOCK__` 为编译期常量，生产包里整段删除）；
 * - 其他情况（生产包在普通浏览器里打开）：所有调用以 `not_implemented` 失败。
 */
import { isTauri } from '@tauri-apps/api/core'
import { boot } from './boot'
import { createTauriTransport } from './tauri'
import { WispIpcError, type Transport } from './transport'

// ---------------------------------------------------------------------------
// 选择与就绪

let current: Transport | null = null
/** mock 模块加载中（只在浏览器 mock 模式下非空） */
let loading: Promise<Transport> | null = null

function select(): Transport {
  if (isTauri()) return createTauriTransport(boot.label)
  if (__WISP_MOCK__) {
    loading = import('./mock')
      .then((m) => m.installMock())
      .then((t) => {
        current = t
        loading = null
        return t
      })
      .catch((error: unknown) => {
        console.error('[wisp] 浏览器 mock 加载失败', error)
        throw error
      })
    return createDeferredTransport(loading)
  }
  return createUnavailableTransport()
}

/** 当前传输层（同步）。mock 尚未加载完时返回一个转发到加载结果的代理。 */
export function getTransport(): Transport {
  return (current ??= select())
}

/** 测试用：注入或清除传输层（清除后下次调用重新选择） */
export function setTransport(transport: Transport | null): void {
  current = transport
  loading = null
}

/**
 * 在传输层就绪后执行 `fn`（各入口用它包住挂载）。
 * Tauri 内同步调用，不引入任何异步跳转（面板 prepare 路径上的每一次异步跳转都会推迟首帧）；
 * mock 模式下等 mock 安装完（rAF / ResizeObserver 补丁、模拟窗口、场景）再调用。
 */
export function whenReady(fn: () => void): void {
  getTransport()
  if (loading) void loading.then(() => fn())
  else fn()
}

function createDeferredTransport(ready: Promise<Transport>): Transport {
  const label = boot.label
  return {
    kind: 'mock',
    invoke: (cmd, args) => ready.then((t) => t.invoke(cmd, args)),
    openStream: (cmd, args, onEvent) => ready.then((t) => t.openStream(cmd, args, onEvent)),
    listen: (event, cb) => ready.then((t) => t.listen(event, cb)),
    window: {
      label,
      startDragging: () => ready.then((t) => t.window.startDragging()),
      close: () => ready.then((t) => t.window.close()),
      setSize: (w, h) => ready.then((t) => t.window.setSize?.(w, h)),
    },
    // mock 未加载时没有资源表：返回空图，加载后 getTransport() 会拿到真正的实现
    protocolUrl: () => EMPTY_IMAGE,
  }
}

function createUnavailableTransport(): Transport {
  const fail = (cmd: string) =>
    Promise.reject(
      new WispIpcError(cmd, {
        code: 'not_implemented',
        message: 'Wisp IPC is only available inside the Tauri app',
        llm: null,
      }),
    )
  return {
    kind: 'unavailable',
    invoke: (cmd) => fail(cmd),
    openStream: (cmd) => fail(cmd),
    listen: () => Promise.resolve(() => {}),
    window: {
      label: boot.label,
      startDragging: () => Promise.resolve(),
      close: () => Promise.resolve(),
    },
    protocolUrl: () => EMPTY_IMAGE,
  }
}

/** 1×1 透明 GIF */
const EMPTY_IMAGE = 'data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7'
