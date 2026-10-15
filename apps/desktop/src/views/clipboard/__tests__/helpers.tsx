/**
 * 剪贴板 / toast 视图测试的公共部件：样例条目、假的传输层（记录命令、按名字给出结果、可推送事件）、渲染视图。
 */
import { act, fireEvent, render } from '@testing-library/react'
import type { ReactElement } from 'react'
import { vi } from 'vitest'
import type { PanelPhase, ViewProps } from '../../../panel/types'
import { boot, setTransport, type Transport } from '../../../shared/ipc'
import type {
  ClipboardChanged,
  ClipboardPayload,
  ClipHit,
  ClipItemMeta,
  ClipKind,
  EventMap,
  EventName,
  OsKind,
} from '../../../shared/ipc/types'
import { getAppearance, setAppearance } from '../../../shared/hooks/useAppearance'
import { MotionRoot } from '../../../shared/motion/MotionRoot'
import { startOfDay } from '../model'

export const NOW = new Date(2026, 9, 11, 15, 30).getTime()
export const TODAY = startOfDay(NOW)
export const HOUR = 3_600_000

export function meta(id: number, partial: Partial<ClipItemMeta> = {}): ClipItemMeta {
  return {
    id,
    kind: 'text' as ClipKind,
    created_at: NOW - id * 60_000,
    last_used_at: NOW - id * 60_000,
    use_count: 1,
    pinned: false,
    source: null,
    preview: `item ${id}`,
    char_count: 6,
    line_count: 1,
    truncated: false,
    image: null,
    files: null,
    link: null,
    color: null,
    ...partial,
  }
}

export const hit = (item: ClipItemMeta, highlights: ClipHit['highlights'] = []): ClipHit => ({
  item,
  highlights,
})

export function payload(
  items: readonly ClipItemMeta[],
  extra: Partial<ClipboardPayload> = {},
): ClipboardPayload & { kind: 'clipboard' } {
  return {
    kind: 'clipboard',
    page: {
      items: items.map((it) => hit(it)),
      pinned_count: items.filter((it) => it.pinned).length,
      next_offset: null,
      query: null,
      filter: 'all',
    },
    paused: false,
    hint_bar: true,
    paste_on_double_click: false,
    ...extra,
  }
}

/** 两条置顶 + 今天 4 条（第 3 条是图片，64 行高）+ 昨天 2 条 */
export function sample(): ClipItemMeta[] {
  return [
    meta(1, { pinned: true, last_used_at: TODAY - 5 * 24 * HOUR, preview: '收货地址' }),
    meta(2, {
      pinned: true,
      kind: 'link',
      preview: 'https://github.com',
      link: 'https://github.com',
    }),
    meta(3, { last_used_at: NOW - 60_000, preview: 'alpha' }),
    meta(4, { last_used_at: NOW - 2 * 60_000, preview: 'beta' }),
    meta(5, {
      kind: 'image',
      last_used_at: NOW - 3 * 60_000,
      preview: '',
      char_count: null,
      line_count: null,
      image: { width: 800, height: 600, bytes: 1000, thumb_url: 'thumb/5' },
    }),
    meta(6, { last_used_at: NOW - 4 * 60_000, preview: 'gamma' }),
    meta(7, { last_used_at: TODAY - 2 * HOUR, preview: 'delta' }),
    meta(8, { last_used_at: TODAY - 3 * HOUR, preview: 'epsilon' }),
  ]
}

type Listener = (payload: unknown) => void

export interface FakeIpc {
  calls: Array<[string, Record<string, unknown>]>
  /** 命令 → 结果（函数时按参数计算；抛出 = 命令失败） */
  results: Record<string, unknown>
  listeners: Map<string, Set<Listener>>
  emit<K extends EventName>(event: K, payload: EventMap[K]): void
  changed(change: ClipboardChanged): void
  called(cmd: string): Array<Record<string, unknown>>
}

export function fakeIpc(): FakeIpc {
  const ipc: FakeIpc = {
    calls: [],
    results: {},
    listeners: new Map(),
    emit(event, data) {
      for (const cb of ipc.listeners.get(event) ?? []) cb(data)
    },
    changed(change) {
      act(() => ipc.emit('clipboard:changed', change))
    },
    called: (cmd) => ipc.calls.filter(([c]) => c === cmd).map(([, a]) => a),
  }
  const transport: Transport = {
    kind: 'mock',
    invoke: <T,>(cmd: string, args?: Record<string, unknown>) => {
      ipc.calls.push([cmd, args ?? {}])
      const r = ipc.results[cmd]
      try {
        const value = typeof r === 'function' ? (r as (a: unknown) => unknown)(args) : r
        return Promise.resolve(value as T)
      } catch (error) {
        return Promise.reject(error)
      }
    },
    openStream: () => Promise.reject(new Error('unused')),
    listen: (event, cb) => {
      const set = ipc.listeners.get(event) ?? new Set()
      set.add(cb as Listener)
      ipc.listeners.set(event, set)
      return Promise.resolve(() => set.delete(cb as Listener))
    },
    window: {} as Transport['window'],
    protocolUrl: (p) => `wisp://${p}`,
  }
  setTransport(transport)
  return ipc
}

/** 用例之间的占位传输层：吞掉迟到的调用 */
export function idleTransport(): Transport {
  return {
    kind: 'mock',
    invoke: <T,>() => Promise.resolve(undefined as T),
    openStream: () => Promise.reject(new Error('unused')),
    listen: () => Promise.resolve(() => {}),
    window: {} as Transport['window'],
    protocolUrl: (p) => p,
  }
}

const savedAppearance = getAppearance()
const savedOs = boot.os

/** 平台（快捷键显示与键位解析）与减少动效 */
export function setPlatform(os: OsKind, reduceMotion = true): void {
  boot.os = os
  document.documentElement.dataset.os = os
  setAppearance({ ...savedAppearance, os, reduceMotion })
}

export function restorePlatform(): void {
  boot.os = savedOs
  setAppearance(savedAppearance)
}

export function viewProps<V extends 'clipboard' | 'toast'>(
  p: ViewProps<V>['payload'],
  extra: Partial<ViewProps<V>> = {},
): ViewProps<V> {
  return {
    sessionId: 7,
    payload: p,
    phase: 'shown' as PanelPhase,
    focused: true,
    host: 'panel',
    frame: 'native',
    placement: 'center',
    maxHeight: 520,
    requestHide: vi.fn<ViewProps<V>['requestHide']>(),
    switchView: vi.fn<ViewProps<V>['switchView']>(),
    activate: vi.fn<() => void>(),
    ...extra,
  } as ViewProps<V>
}

/**
 * 指针第一次出现在列表上（面板显示时 WebKit 的合成 pointermove）：列表只把它记作基线、不选中。
 * 需要「真实移动」的用例先调用它，之后的 pointermove 才会悬停选中
 */
export function armPointer(): void {
  const wrap = document.querySelector('.clip-listwrap')
  if (wrap) fireEvent.pointerMove(wrap, { clientX: 1, clientY: 1, pointerType: 'mouse' })
}

export function renderIn(ui: ReactElement) {
  return render(<MotionRoot>{ui}</MotionRoot>)
}

/** 发一个按键（走 window 捕获阶段的键盘路由） */
export function key(
  code: string,
  opts: {
    key?: string
    meta?: boolean
    ctrl?: boolean
    alt?: boolean
    shift?: boolean
    repeat?: boolean
    composing?: boolean
    target?: Element | null
  } = {},
): KeyboardEvent {
  const ev = new KeyboardEvent('keydown', {
    key: opts.key ?? keyOf(code),
    code,
    bubbles: true,
    cancelable: true,
    metaKey: !!opts.meta,
    ctrlKey: !!opts.ctrl,
    altKey: !!opts.alt,
    shiftKey: !!opts.shift,
    repeat: !!opts.repeat,
    isComposing: !!opts.composing,
  })
  act(() => {
    ;(opts.target ?? document.activeElement ?? document.body).dispatchEvent(ev)
  })
  return ev
}

function keyOf(code: string): string {
  if (code.startsWith('Key')) return code.slice(3).toLowerCase()
  if (code.startsWith('Digit')) return code.slice(5)
  if (code === 'Space') return ' '
  return code
}
