/**
 * 浏览器 mock 的公共类型。在 `scenarios/*.ts` 里编写场景时只需要这里的类型。
 */
import type { Boot } from '../boot'
import type { CommandArgs, CommandName, CommandResult, StreamListener } from '../commands'
import type {
  EventMap,
  EventName,
  IpcErrorCode,
  LlmError,
  PanelPayload,
  PanelPrepare,
  Placement,
  StreamEvent,
} from '../types'
import type { MockFixtures } from './fixtures'

/** 命令处理函数；抛出 `IpcError` 形状的对象（见 `mockError`）即模拟命令失败 */
export type CommandHandler<K extends CommandName> = (
  args: CommandArgs<K>,
  call: HandlerCall,
) => CommandResult<K> | Promise<CommandResult<K>>

export interface HandlerCall {
  readonly cmd: CommandName
  /** 流式命令（stream_attach / translate_start / ocr_start）的事件回调，相当于 Rust 侧的 Channel */
  readonly onEvent?: StreamListener
}

/** 流脚本：事件与等待交替；`{ wait }` 单位 ms */
export type StreamStep = StreamEvent | { wait: number }
export type StreamScript = readonly StreamStep[]

export interface TextStreamOptions {
  /** 每个 chunk 的字符数（默认 CJK 2、其他 4） */
  chunkChars?: number
  /** chunk 间隔，默认 20 ms */
  intervalMs?: number
  /** 首 token 前的等待，默认 280 ms */
  firstTokenMs?: number
  model?: { provider_id: string; model_id: string; model_label: string }
  meta?: Partial<Extract<StreamEvent, { type: 'meta' }>>
  done?: Partial<Extract<StreamEvent, { type: 'done' }>>
  /** 在 Delta 之前插入的事件（Phase / Notice 等） */
  before?: readonly StreamStep[]
}

/** 场景可以直接打开的入口；默认由 `view` 推导 */
export type MockEntry = 'panel' | 'settings' | 'pinned' | 'clipfloat' | 'overlay'
export type ScenarioView =
  PanelPayload['kind'] | 'pinned' | 'settings' | 'onboarding' | 'overlay' | 'page'

export interface Scenario {
  /** `<文件名>.<名字>`，例如 `translate.streaming-learn`；URL 参数 `?scenario=` 用它 */
  id: string
  /** 人类可读名称（HUD 与 QA 汇总页显示） */
  label: string
  /** 场景针对的视图 / 窗口 */
  view: ScenarioView
  /** 覆盖默认入口（例如组件展示页 `shell.gallery` 用 panel 入口但不走面板会话） */
  entry?: MockEntry
  /** 动效类场景：QA 额外录视频并截 `data-motion-mid` 中间帧 */
  motion?: boolean
  /** 自己调用 `ctx.settled()`；否则 mock 在没有挂起的流 / 定时器 / 事件后自动标记 */
  manualSettle?: boolean
  setup(ctx: MockContext): void | Promise<void>
}

export interface PrepareOptions {
  placement?: Placement
  focused?: boolean
  transition?: PanelPrepare['transition']
  width?: number
  maxHeight?: number
  /** 锚点（页面坐标，模拟选区矩形）；默认取假桌面里高亮的选区 */
  anchor?: { x: number; y: number; width: number; height: number }
}

export interface MockContext {
  readonly boot: Boot
  readonly label: string
  readonly fixtures: MockFixtures
  /** 发事件；本窗口还没有监听者时排队，等第一个监听者注册后投递（场景 setup 早于组件挂载） */
  emit<K extends EventName>(event: K, payload: EventMap[K]): void
  /** 覆写命令处理函数（返回恢复函数） */
  handle<K extends CommandName>(cmd: K, handler: CommandHandler<K>): () => void
  /** 让下一次 `translate_start` / `ocr_start` 使用这段脚本（不传 requestId 时） */
  nextStream(script: StreamScript): void
  /** 预置某个 request_id 的流：立即开始播放（相当于 Rust 在 t0 发出请求），attach 时回放 */
  stream(requestId: string, script: StreamScript): void
  /** 逐 chunk 输出一段文本的流脚本（默认每 20 ms 一个 chunk） */
  textStream(text: string, options?: TextStreamOptions): StreamScript
  /** 压力流：以 tokensPerSec 的速率输出 total 个 token（200 token/s × 4096） */
  streamRate(tokensPerSec: number, total: number, options?: TextStreamOptions): StreamScript
  /** 以错误结束的流 */
  errorStream(
    error: Partial<LlmError> & { code: LlmError['code'] },
    partialText?: string,
  ): StreamScript
  /** 构造 PanelPrepare 并发出 `panel:prepare`；返回 session_id */
  preparePanel(payload: PanelPayload, options?: PrepareOptions): number
  /** 让面板收到 `panel:hide_request` */
  requestHide(reason?: EventMap['panel:hide_request']['reason']): void
  /** 注册 `wisp://` 资源：返回 `protocolUrl(path)` 将给出的 URL */
  asset(path: string, body: Blob | string, headers?: Record<string, string>): string
  /**
   * 让 `<base>/<x>/<y>/<w>/<h>[/<max_px>]` 的小块从已注册的整屏 RGBA 帧 `fullPath` 里裁出（macOS 原生冻结帧的
   * `region_url`）；返回 base
   */
  regionAsset(base: string, fullPath: string): string
  /** 延时执行（计入「未稳定」） */
  after(ms: number, fn: () => void): void
  /** 标记场景已稳定（`<html data-settled>`，QA 截图等待它） */
  settled(): void
  /** 标记动效中间帧（`<html data-motion-mid>`） */
  motionMid(): void
  log(...args: unknown[]): void
}

/** 构造一个 IpcError 形状的错误对象（在命令处理函数里 `throw mockError(...)`） */
export function mockError(code: IpcErrorCode, message: string, llm: LlmError | null = null) {
  return { code, message, llm }
}
