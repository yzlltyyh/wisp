/**
 * mock 的命令路由：命令名 → 处理函数。默认处理函数见 `defaults.ts`，场景可以逐条覆写。
 * 参数与返回值都经结构化克隆，模拟跨进程序列化（调用方改返回值不会污染 mock 状态）。
 */
import type { CommandName, StreamListener } from '../commands'
import { WispIpcError } from '../transport'
import type { CommandHandler, HandlerCall } from './types'

type AnyHandler = (args: unknown, call: HandlerCall) => unknown

export interface RouterOptions {
  /** 每次调用的模拟延迟（ms），URL 参数 `ipc_latency`；默认 0 */
  latencyMs?: number
  /** 在控制台打印每次调用（URL 参数 `ipc_log=1`） */
  log?: boolean
}

export class MockRouter {
  private readonly handlers = new Map<CommandName, AnyHandler>()
  private readonly options: RouterOptions
  /** 每条命令的调用次数（测试 / 调试用） */
  readonly calls = new Map<CommandName, number>()

  constructor(options: RouterOptions = {}) {
    this.options = options
  }

  /** 注册或覆写处理函数；返回恢复为之前处理函数的函数 */
  handle<K extends CommandName>(cmd: K, handler: CommandHandler<K>): () => void {
    const previous = this.handlers.get(cmd)
    this.handlers.set(cmd, handler as AnyHandler)
    return () => {
      if (previous) this.handlers.set(cmd, previous)
      else this.handlers.delete(cmd)
    }
  }

  has(cmd: CommandName): boolean {
    return this.handlers.has(cmd)
  }

  registered(): CommandName[] {
    return [...this.handlers.keys()]
  }

  async dispatch(cmd: string, args: unknown, onEvent?: StreamListener): Promise<unknown> {
    const name = cmd as CommandName
    this.calls.set(name, (this.calls.get(name) ?? 0) + 1)
    const handler = this.handlers.get(name)
    if (!handler) {
      throw new WispIpcError(cmd, {
        code: 'not_implemented',
        message: `mock: no handler for command "${cmd}"`,
        llm: null,
      })
    }
    if (this.options.latencyMs) await sleep(this.options.latencyMs)
    const input = structuredClone(args ?? {})
    try {
      const result = await handler(input, onEvent ? { cmd: name, onEvent } : { cmd: name })
      if (this.options.log) console.debug('[mock ipc]', cmd, input, '→', result)
      return result === undefined ? undefined : structuredClone(result)
    } catch (error) {
      if (this.options.log) console.debug('[mock ipc]', cmd, input, '✕', error)
      throw WispIpcError.from(cmd, error)
    }
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms))
}
