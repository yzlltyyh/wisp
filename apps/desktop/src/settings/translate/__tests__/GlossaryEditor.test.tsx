/**
 * 术语表编辑器：逐条写入（upsert 带上改动前的原文、清空原文与删除行写 remove，不再整表替换）、
 * 收到 glossary:changed 时在没有未写出的改动时重新读取（沿用行、不丢焦点）、编辑中途的外部变化等编辑结束再读，
 * 以及读回条目与已有行的对应规则。
 */
import { act, fireEvent, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import type { GlossaryEntry } from '../../../shared/ipc/types'
import { installHarness, type Harness } from '../../__tests__/harness'
import { resetWrites } from '../../lib/writes'
import { resetSettingsStore, useSettingsStore } from '../../store'
import { GlossaryEditor, reconcileRows, settleGlossaryWrites, type Row } from '../GlossaryEditor'

const e = (source: string, target: string, note: string | null = null): GlossaryEntry => ({
  source,
  target,
  note,
})

/** 按 Rust 的规则在内存里维护术语表（upsert / remove 逐条改） */
function glossaryHarness(initial: GlossaryEntry[]): {
  h: Harness
  server: { entries: GlossaryEntry[] }
} {
  const h = installHarness()
  const server = { entries: [...initial] }
  h.handle('glossary_get', () => structuredClone(server.entries))
  h.handle('glossary_upsert', ({ entry, previous_source }) => {
    const next = entry as GlossaryEntry
    const previous = previous_source as string | null
    const out: GlossaryEntry[] = []
    let placed = false
    for (const x of server.entries) {
      if (x.source === next.source || x.source === previous) {
        if (!placed) out.push(next)
        placed = true
        continue
      }
      out.push(x)
    }
    if (!placed) out.push(next)
    server.entries = out
  })
  h.handle('glossary_remove', ({ source }) => {
    server.entries = server.entries.filter((x) => x.source !== source)
  })
  return { h, server }
}

/** 等编辑器停顿后写出、串行写入全部返回 */
async function settle(): Promise<void> {
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 500))
    await settleGlossaryWrites()
  })
}

const sources = () => screen.getAllByTestId('glossary-source') as HTMLInputElement[]
const targets = () => screen.getAllByTestId('glossary-target') as HTMLInputElement[]

beforeEach(() => {
  resetSettingsStore()
  resetWrites()
})

afterEach(async () => {
  await settleGlossaryWrites()
})

describe('术语表编辑器：逐条写入', () => {
  it('改译文、改原文、清空原文、删除行：每次只写这一条，带上改动前的原文', async () => {
    const { h, server } = glossaryHarness([e('token', '令牌'), e('GPU', 'GPU')])
    render(<GlossaryEditor />)
    await waitFor(() => expect(sources()).toHaveLength(2))

    // 改译文
    await userEvent.clear(targets()[0] as HTMLElement)
    await userEvent.type(targets()[0] as HTMLElement, '记号')
    await settle()
    expect(h.callsOf('glossary_upsert')).toEqual([
      { entry: e('token', '记号'), previous_source: 'token' },
    ])

    // 改原文：带上原来的原文，Rust 据此原位替换
    await userEvent.type(sources()[0] as HTMLElement, 's')
    await settle()
    expect(h.callsOf('glossary_upsert').at(-1)).toEqual({
      entry: e('tokens', '记号'),
      previous_source: 'token',
    })
    expect(server.entries).toEqual([e('tokens', '记号'), e('GPU', 'GPU')])

    // 清空原文 = 删掉这一条
    await userEvent.clear(sources()[1] as HTMLElement)
    await settle()
    expect(h.callsOf('glossary_remove')).toEqual([{ source: 'GPU' }])

    // 删除行
    fireEvent.click(document.querySelector('.st-glossary-remove') as HTMLElement)
    await settle()
    expect(h.callsOf('glossary_remove').at(-1)).toEqual({ source: 'tokens' })
    expect(server.entries).toEqual([])
    expect(h.callsOf('glossary_set')).toHaveLength(0)
  })

  it('打开之后别处加进来的术语：编辑自己的行不会把它删掉', async () => {
    const { h, server } = glossaryHarness([e('token', '令牌')])
    render(<GlossaryEditor />)
    await waitFor(() => expect(sources()).toHaveLength(1))
    // 其他设备同步来一条（编辑器还没重新读取）
    server.entries = [...server.entries, e('Remote', '远端')]
    await userEvent.clear(targets()[0] as HTMLElement)
    await userEvent.type(targets()[0] as HTMLElement, '记号')
    await settle()
    expect(server.entries).toEqual([e('token', '记号'), e('Remote', '远端')])
    expect(h.callsOf('glossary_set')).toHaveLength(0)
  })
})

describe('术语表编辑器：两行原文相同', () => {
  /** 加一行「Tauri → NEW-TARGET」并让它写出（Rust 把它合并进已有的 Tauri 那一条） */
  async function addDuplicate(): Promise<HTMLInputElement> {
    fireEvent.click(screen.getByTestId('glossary-add'))
    await waitFor(() => expect(sources()).toHaveLength(2))
    const source = sources()[1] as HTMLInputElement
    await userEvent.type(source, 'Tauri')
    await userEvent.type(targets()[1] as HTMLElement, 'NEW-TARGET')
    act(() => (targets()[1] as HTMLElement).blur())
    await settle()
    return source
  }

  it('加一行替换旧的那行、再删掉旧行：留下新加的那一行，不会一起删掉', async () => {
    const { h, server } = glossaryHarness([e('Tauri', 'Tauri')])
    render(<GlossaryEditor />)
    await waitFor(() => expect(sources()).toHaveLength(1))
    await addDuplicate()
    expect(server.entries).toEqual([e('Tauri', 'NEW-TARGET')])

    fireEvent.click(document.querySelector('.st-glossary-remove') as HTMLElement)
    await settle()
    expect(h.callsOf('glossary_remove')).toHaveLength(0)
    expect(server.entries).toEqual([e('Tauri', 'NEW-TARGET')])
    act(() => h.emit('glossary:changed', null))
    await waitFor(() => expect(sources()).toHaveLength(1))
    expect(sources()[0]).toHaveValue('Tauri')
    expect(targets()[0]).toHaveValue('NEW-TARGET')
  })

  it('清空旧行的原文：那一条交给新加的行，不删除', async () => {
    const { h, server } = glossaryHarness([e('Tauri', 'Tauri')])
    render(<GlossaryEditor />)
    await waitFor(() => expect(sources()).toHaveLength(1))
    await addDuplicate()
    await userEvent.clear(sources()[0] as HTMLElement)
    act(() => (sources()[0] as HTMLElement).blur())
    await settle()
    expect(h.callsOf('glossary_remove')).toHaveLength(0)
    expect(server.entries).toEqual([e('Tauri', 'NEW-TARGET')])
  })

  it('改掉旧行的原文：旧行作为新条目写入，不把新加那一行的条目改名带走', async () => {
    const { server } = glossaryHarness([e('Tauri', 'Tauri')])
    render(<GlossaryEditor />)
    await waitFor(() => expect(sources()).toHaveLength(1))
    await addDuplicate()
    await userEvent.type(sources()[0] as HTMLElement, '2')
    act(() => (sources()[0] as HTMLElement).blur())
    await settle()
    expect(server.entries).toEqual([e('Tauri', 'NEW-TARGET'), e('Tauri2', 'Tauri')])
  })
})

describe('术语表编辑器：外部变化', () => {
  it('没有未写出的改动时收到 glossary:changed 重新读取；已有的行沿用（不重新挂载）', async () => {
    const { h, server } = glossaryHarness([e('token', '令牌')])
    render(<GlossaryEditor />)
    await waitFor(() => expect(sources()).toHaveLength(1))
    const first = sources()[0]
    server.entries = [e('token', '记号'), e('Remote', '远端')]
    act(() => h.emit('glossary:changed', null))
    await waitFor(() => expect(sources()).toHaveLength(2))
    expect(sources()[0]).toBe(first)
    expect(targets()[0]).toHaveValue('记号')
    expect(sources()[1]).toHaveValue('Remote')
  })

  it('正在编辑某一行时先不读，失焦并写出之后再读；输入不被打断', async () => {
    const { h, server } = glossaryHarness([e('token', '令牌')])
    render(<GlossaryEditor />)
    await waitFor(() => expect(sources()).toHaveLength(1))
    const target = targets()[0] as HTMLInputElement
    await userEvent.click(target)
    await userEvent.type(target, '!')
    // 编辑中途别处加了一条
    server.entries = [...server.entries, e('Remote', '远端')]
    const reads = h.callsOf('glossary_get').length
    act(() => h.emit('glossary:changed', null))
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 20))
    })
    expect(h.callsOf('glossary_get')).toHaveLength(reads)
    expect(sources()).toHaveLength(1)
    expect(target).toHaveValue('令牌!')
    expect(target).toHaveFocus()
    // 离开这一行：先写出自己的改动，写完之后补上那次读取
    act(() => target.blur())
    await settle()
    await waitFor(() => expect(sources()).toHaveLength(2))
    expect(server.entries).toEqual([e('token', '令牌!'), e('Remote', '远端')])
    expect(targets()[0]).toHaveValue('令牌!')
  })

  it('写入失败：提示，并在写入结束后按 Rust 的内容重新读取', async () => {
    const { h, server } = glossaryHarness([e('token', '令牌')])
    h.handle('glossary_upsert', () => {
      throw { code: 'io', message: 'disk full', llm: null }
    })
    render(<GlossaryEditor />)
    await waitFor(() => expect(sources()).toHaveLength(1))
    await userEvent.clear(targets()[0] as HTMLElement)
    await userEvent.type(targets()[0] as HTMLElement, '记号')
    act(() => (targets()[0] as HTMLElement).blur())
    await settle()
    await waitFor(() => expect(useSettingsStore.getState().notice?.tone).toBe('danger'))
    await waitFor(() => expect(targets()[0]).toHaveValue('令牌'))
    expect(server.entries).toEqual([e('token', '令牌')])
  })
})

describe('reconcileRows', () => {
  const row = (key: number, x: GlossaryEntry): Row => ({ ...x, key })

  it('按原文对上已有的行；这一行自己写进去的内容保留原样；新的空行留在末尾', () => {
    // 第 1 行写入时译文留空（发出去的是与原文相同的译文）
    const blankTarget = row(1, e('Tauri', ''))
    const remote = row(2, e('token', '令牌'))
    const fresh = row(3, e('', ''))
    const saved = new Map<number, GlossaryEntry>([
      [1, e('Tauri', 'Tauri')],
      [2, e('token', '令牌')],
    ])
    const prev = [blankTarget, remote, fresh]
    const next = reconcileRows(
      prev,
      [e('Tauri', 'Tauri'), e('token', '记号'), e('New', '新')],
      saved,
    )
    expect(next[0]).toBe(blankTarget)
    expect(next[1]).toEqual(row(2, e('token', '记号')))
    expect(next[2]).toMatchObject(e('New', '新'))
    expect(next[3]).toBe(fresh)
    expect(saved.get(2)).toEqual(e('token', '记号'))
    // 没有变化：返回同一个数组（不触发重渲染）
    expect(
      reconcileRows(next, [e('Tauri', 'Tauri'), e('token', '记号'), e('New', '新')], saved),
    ).toBe(next)
    // 别处删掉的条目：对应的行消失
    const after = reconcileRows(next, [e('token', '记号')], saved)
    expect(after.map((r) => r.source)).toEqual(['token', ''])
  })
})
