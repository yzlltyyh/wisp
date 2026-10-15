/**
 * IPC 传输层接口与错误类型。
 * 选择逻辑见 `runtime.ts`；真实实现见 `tauri.ts`，浏览器实现见 `mock/`。
 */
import type { EventMap, IpcError, IpcErrorCode, LlmError, StreamEvent } from './types'

export type Unlisten = () => void
export type InvokeArgs = Record<string, unknown>

/**
 * 当前窗口的门面：只包含 capability 允许的操作（拖动、关闭）。
 *
 * capabilities/default.json 只放行 `core:event:allow-listen|unlisten` 与 `core:window:allow-start-dragging|
 * internal-toggle-maximize|close`，**比 Tauri 的 core:default 更窄**。因此前端**不得**调用
 * `@tauri-apps/api/window` 的其他 getter / setter（innerSize、scaleFactor、setSize、onFocusChanged 背后的监听命令……）
 * 以及 app / path / webview / menu / tray 等核心 API：它们在 Tauri 里会被 ACL 拒绝，浏览器 mock 与 vitest 里却完全正常，
 * 要到联调才暴露。窗口相关的能力一律走 Rust 命令或 Rust 推送的事件；`__tests__/tauri-imports.test.ts`
 * 保证 `@tauri-apps/*` 只在 tauri.ts / runtime.ts 里、且只用到 core / event /（惰性的）window。
 */
export interface WindowFacade {
  readonly label: string
  startDragging(): Promise<void>
  close(): Promise<void>
  /** 仅 mock 实现（模拟窗口尺寸）；Tauri 侧窗口尺寸一律由 Rust 管理 */
  setSize?(width: number, height: number): Promise<void>
}

export interface Transport {
  readonly kind: 'tauri' | 'mock' | 'unavailable'
  /** 普通命令；参数对象的键保持 snake_case */
  invoke<T>(cmd: string, args?: InvokeArgs): Promise<T>
  /** 流式命令：传输层创建 Channel 并以 `on_event` 键随参数发送（流式输出不走事件） */
  openStream<T>(cmd: string, args: InvokeArgs, onEvent: (event: StreamEvent) => void): Promise<T>
  /** 只接收发给本窗口（或广播）的事件 */
  listen<K extends keyof EventMap>(event: K, cb: (payload: EventMap[K]) => void): Promise<Unlisten>
  readonly window: WindowFacade
  /** `wisp://` 资源 URL（Tauri：convertFileSrc(path, 'wisp')） */
  protocolUrl(path: string): string
}

/** 命令失败时抛出的错误：`code` / `message` / `llm` 与 Rust 的 IpcError 一致 */
export class WispIpcError extends Error implements IpcError {
  override readonly name = 'WispIpcError'
  readonly code: IpcErrorCode
  readonly llm: LlmError | null
  /** 出错的命令名，便于日志定位 */
  readonly command: string

  constructor(command: string, error: IpcError) {
    super(error.message)
    this.command = command
    this.code = error.code
    this.llm = error.llm
  }

  /**
   * 把 Tauri / mock 抛出的任意值规范化为 WispIpcError。
   *
   * 参数反序列化失败（非法枚举值、缺参数、类型不对）发生在进入 Rust 命令函数体之前，Tauri 以纯字符串拒绝：
   * 「invalid args `<key>` for command `<cmd>`: unknown variant …」（tauri 2 `InvokeError` 的格式），
   * 这里归一为 `invalid_argument`，与命令体内的参数校验一致。
   */
  static from(command: string, raw: unknown): WispIpcError {
    if (raw instanceof WispIpcError) return raw
    if (isIpcError(raw)) return new WispIpcError(command, raw)
    const message =
      raw instanceof Error ? raw.message : typeof raw === 'string' ? raw : JSON.stringify(raw)
    const code: IpcErrorCode = INVALID_ARGS.test(message) ? 'invalid_argument' : 'internal'
    return new WispIpcError(command, { code, message, llm: null })
  }

  toJSON(): IpcError & { command: string } {
    return { command: this.command, code: this.code, message: this.message, llm: this.llm }
  }
}

/** Tauri 在参数反序列化失败时的错误前缀（见 `WispIpcError.from`） */
const INVALID_ARGS = /^invalid args `/

const IPC_ERROR_CODES: ReadonlySet<string> = new Set<IpcErrorCode>([
  'invalid_argument',
  'not_found',
  'not_ready',
  'permission',
  'platform',
  'keychain',
  'io',
  'llm',
  'conflict',
  'not_implemented',
  'internal',
])

function isIpcError(value: unknown): value is IpcError {
  if (typeof value !== 'object' || value === null) return false
  const v = value as Partial<IpcError>
  return typeof v.code === 'string' && IPC_ERROR_CODES.has(v.code) && typeof v.message === 'string'
}
