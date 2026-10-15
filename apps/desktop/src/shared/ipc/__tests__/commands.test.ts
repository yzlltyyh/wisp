/**
 * 命令封装的契约测试：每个具名导出都发出自己的命令名（camelCase ↔ snake_case）、参数键全部 snake_case、
 * 流式命令走 openStream；mock 路由覆盖全部命令。
 */
import { afterEach, describe, expect, it } from 'vitest'
import * as api from '../commands'
import { COMMAND_NAMES, STREAM_COMMANDS } from '../commands'
import { setTransport, type Transport } from '../index'

interface Call {
  cmd: string
  args: Record<string, unknown>
  stream: boolean
}

function recorder(calls: Call[]): Transport {
  return {
    kind: 'mock',
    invoke: async (cmd, args) => {
      calls.push({ cmd, args: args ?? {}, stream: false })
      return undefined as never
    },
    openStream: async (cmd, args) => {
      calls.push({ cmd, args, stream: true })
      return undefined as never
    },
    listen: async () => () => {},
    window: { label: 'panel', startDragging: async () => {}, close: async () => {} },
    protocolUrl: (p) => p,
  }
}

const toSnake = (name: string) => name.replace(/[A-Z]/g, (c) => `_${c.toLowerCase()}`)

afterEach(() => setTransport(null))

describe('commands.ts', () => {
  type AnyCommand = (...args: unknown[]) => Promise<unknown>
  const fns = (Object.entries(api) as [string, unknown][]).filter(
    (entry): entry is [string, AnyCommand] => typeof entry[1] === 'function',
  )

  it('COMMAND_NAMES 无重复，且与具名导出一一对应', () => {
    expect(new Set(COMMAND_NAMES).size).toBe(COMMAND_NAMES.length)
    expect(fns.map(([name]) => toSnake(name)).sort()).toEqual([...COMMAND_NAMES].sort())
  })

  it.each(fns.map(([name, fn]) => [name, fn] as const))(
    '%s 发出对应的 snake_case 命令与参数键',
    async (name, fn) => {
      const calls: Call[] = []
      setTransport(recorder(calls))
      const dummy = Array.from({ length: fn.length }, () => 'x')
      await fn(...dummy)
      expect(calls).toHaveLength(1)
      const [call] = calls as [Call]
      expect(call.cmd).toBe(toSnake(name))
      expect(call.stream).toBe((STREAM_COMMANDS as readonly string[]).includes(call.cmd))
      for (const key of Object.keys(call.args)) expect(key).toMatch(/^[a-z][a-z0-9_]*$/)
    },
  )
})
