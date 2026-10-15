/**
 * 面板外壳：
 * - prepare 路径不依赖 rAF / ResizeObserver / Motion 回调 / setTimeout：收到 panel:prepare 后同步调用 panel_ready（远低于 40 ms）；
 * - 测量值非法时退回视图的估计高度；
 * - 原位交叉淡入（transition = crossfade）不重新挂载卡片；
 * - 退出：Native 立即 panel_hide；Css 播完退出动画（或兜底）再 panel_hide；隐藏后卸载视图、`<html data-hidden>`；
 * - 视图切换：新视图先在暂存层（不可见、不参与布局、inert）里渲染；宽度变化时旧视图原地淡出，透明后才交换并改宽；
 *   宽度不变时旧视图脱离布局淡出；交换之后立即 panel_resize；淡出中的旧视图保留切换前的 props；
 *   暂存视图里的 autoFocus 推迟到显示时、m.* 的进入动画被屏蔽（显示之后挂载的照常）；
 * - 窗口不能透明（shadow_margin = 0）：变矮立即 panel_resize；外框不做透明度 / 变换；退出立即 panel_hide；
 * - Esc 经键盘路由的全局层关闭面板。
 */
import { act, cleanup, render, screen, waitFor } from '@testing-library/react'
import { m } from 'motion/react'
import { createRoot } from 'react-dom/client'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { boot, setTransport, type Transport } from '../../../shared/ipc'
import type { EventMap, EventName, PanelPayload, PanelPrepare } from '../../../shared/ipc/types'
import { getAppearance, setAppearance } from '../../../shared/hooks/useAppearance'
import { useKeyScope } from '../../../shared/keys/useKeyScope'
import { TextArea } from '../../../shared/ui'
import { triggerResizeObservers } from '../../../test/stubs'
import { views } from '../../views'
import { setViewOverride } from '../devHost'
import { PanelApp } from '../PanelApp'
import { usePanelStore } from '../panelStore'
import { installPanelEvents } from '../usePanelLifecycle'
import type { ViewSwitchRenderProps } from '../ViewSwitch'

type Listener = (payload: unknown) => void
let listeners: Map<string, Set<Listener>>
let log: string[]
let calls: Array<[string, Record<string, unknown>]>

function fakeTransport(): Transport {
  return {
    kind: 'mock',
    invoke: <T,>(cmd: string, args?: Record<string, unknown>) => {
      calls.push([cmd, args ?? {}])
      log.push(cmd)
      if (cmd === 'panel_ready') {
        return Promise.resolve({ session_id: args?.session_id, focused: false } as T)
      }
      return Promise.resolve(undefined as T)
    },
    openStream: () => Promise.reject(new Error('unused')),
    listen: (event, cb) => {
      const set = listeners.get(event) ?? new Set()
      set.add(cb as Listener)
      listeners.set(event, set)
      return Promise.resolve(() => set.delete(cb as Listener))
    },
    window: {} as Transport['window'],
    protocolUrl: (p) => p,
  }
}

/** 用例之间的占位传输层：吞掉迟到的调用 */
function idleTransport(): Transport {
  return { ...fakeTransport(), invoke: <T,>() => Promise.resolve(undefined as T) }
}

function emit<K extends EventName>(event: K, payload: EventMap[K]): void {
  for (const cb of listeners.get(event) ?? []) cb(payload)
}

const translate: PanelPayload = {
  kind: 'translate',
  source_text: 'hello',
  request_id: null,
  task: 'translate',
  style_id: 'natural',
  detected_lang: 'en',
  target_lang: 'zh-CN',
  dict_mode: false,
  long_text: null,
  not_ready: null,
  selection_method: null,
}
const input: PanelPayload = { kind: 'input', initial_text: '', target_lang: null, hint: null }

let seq = 0
function prepare(payload: PanelPayload, extra: Partial<PanelPrepare> = {}): PanelPrepare {
  return {
    session_id: ++seq,
    view: payload.kind,
    payload,
    width: 400,
    max_height: 500,
    placement: 'below',
    origin_x: 40,
    focused: false,
    transition: 'enter',
    ...extra,
  }
}

const savedFrame = boot.frame_mode
const savedMargin = boot.shadow_margin
const savedAppearance = getAppearance()

/** jsdom 没有布局：按「布局里的视图」（不在暂存层、不是脱离布局的淡出层）给出内容高度 */
function contentHeights(heights: Partial<Record<PanelPayload['kind'], number>>, fallback = 120) {
  vi.spyOn(HTMLElement.prototype, 'offsetHeight', 'get').mockImplementation(function (
    this: HTMLElement,
  ) {
    if (!this.hasAttribute('data-panel-content')) return 0
    const view = this.querySelector('.panel-view:is([data-state="shown"], [data-state="leaving"])')
    const kind = view?.getAttribute('data-view') as PanelPayload['kind'] | null
    return (kind && heights[kind]) ?? fallback
  })
}

/** 视图所在的层（ViewSwitch 的 .panel-view） */
function layerOf(testId: string): HTMLElement | null {
  return screen.queryByTestId(testId)?.closest<HTMLElement>('.panel-view') ?? null
}

beforeEach(async () => {
  listeners = new Map()
  log = []
  calls = []
  setTransport(fakeTransport())
  // 减少动效：退出 / 交叉淡入 ≤ 100 ms，测试更快
  setAppearance({ ...savedAppearance, reduceMotion: true })
  render(<PanelApp />)
  await waitFor(() => {
    if (!listeners.get('panel:prepare')?.size) throw new Error('外壳尚未订阅 panel:prepare')
  })
})

afterEach(() => {
  boot.frame_mode = savedFrame
  boot.shadow_margin = savedMargin
  setViewOverride(null)
  delete document.documentElement.dataset.opaque
  setAppearance(savedAppearance)
  // 不清成 null：用例结束后才到期的 rAF / 兜底计时器（panel_painted、panel_hide）会调用 getTransport()，
  // 为 null 时会触发浏览器 mock 的异步加载，加载完成后覆盖下一个用例注入的传输层
  setTransport(idleTransport())
  vi.unstubAllGlobals()
})

describe('prepare 路径', () => {
  // 确定性：不用墙钟断言「远低于 40 ms」——同一段 4–5 ms 的同步工作在高负载的
  // CI 上会被抢占成几十毫秒。要保证的是 prepare 路径**不等待**任何异步回调：这里把计时器与 rAF 全部换成
  // 永远不会自己触发的假实现，panel_ready 仍必须在 emit 的同一次同步调用里发出（事件计数），而且只发一次。
  // 40 ms 的耗时预算由真机的冒烟（prepare→shown p95）与 window lab 把关
  it('rAF / ResizeObserver / 计时器都不触发时，收到 prepare 后同步调用 panel_ready（只一次）', () => {
    // 预热一次（首次渲染含模块初始化，不代表常驻面板的唤出路径）
    act(() => emit('panel:prepare', prepare(input)))
    log = []
    // rAF 由 stubGlobal 换成只记录、从不回调的实现（afterEach 的 unstubAllGlobals 还原）；计时器交给假计时器
    vi.stubGlobal('requestAnimationFrame', () => {
      log.push('raf')
      return 0
    })
    vi.useFakeTimers({
      toFake: [
        'setTimeout',
        'clearTimeout',
        'setInterval',
        'clearInterval',
        'requestIdleCallback',
        'cancelIdleCallback',
      ],
    })
    try {
      let readyDuringEmit = -1
      act(() => {
        emit('panel:prepare', prepare(translate))
        readyDuringEmit = log.filter((c) => c === 'panel_ready').length
      })
      // emit 返回之前（act 收尾之前、任何计时器 / 帧回调之前）就已发出
      expect(readyDuringEmit).toBe(1)
      expect(log[0]).toBe('panel_ready')
      expect(log.filter((c) => c === 'panel_ready')).toHaveLength(1)
      expect(screen.getByTestId('view-translate')).toBeTruthy()
    } finally {
      vi.useRealTimers()
    }
  })

  it('测量为 0 / NaN 时退回视图的估计高度', () => {
    act(() => emit('panel:prepare', prepare(translate)))
    const ready = calls.find(([c]) => c === 'panel_ready')?.[1]
    expect(ready?.content_height).toBe(views.translate.estimatedHeight)
  })

  it('ready 返回后进入动画、下一帧报 panel_painted', async () => {
    act(() => emit('panel:prepare', prepare(translate)))
    await waitFor(() => expect(['entering', 'shown']).toContain(usePanelStore.getState().phase))
    await waitFor(() => expect(log).toContain('panel_painted'))
  })
})

describe('页面级订阅（面板入口在渲染之前订阅）', () => {
  it('外壳挂载之前到达的 prepare：挂载后立即处理（同步 ready、只一次），不等 Rust 补发', async () => {
    cleanup()
    usePanelStore.getState().clear()
    usePanelStore.getState().setPhase('hidden')
    const off = installPanelEvents()
    try {
      await waitFor(() => expect(listeners.get('panel:prepare')?.size).toBe(1))
      // 页面刚加载：React 还没渲染。Rust 的第一次 emit 与补发都只有页面级监听收到（只留最新的一个）
      const p = prepare(translate)
      emit('panel:prepare', p)
      emit('panel:prepare', p)
      expect(log).not.toContain('panel_ready')
      render(<PanelApp />)
      await waitFor(() => expect(log.filter((c) => c === 'panel_ready')).toHaveLength(1))
      expect(calls.find(([c]) => c === 'panel_ready')?.[1]).toMatchObject({
        session_id: p.session_id,
      })
      expect(screen.getByTestId('view-translate')).toBeTruthy()
      // 外壳挂载之后不再另外订阅：仍只有页面级的一个监听
      expect(listeners.get('panel:prepare')?.size).toBe(1)
    } finally {
      cleanup()
      off()
    }
  })

  it('挂载之前到达的 prepare 在真实调度下（不经 act）：同步提交视图，ready 报布局里的真实高度而不是估计值', async () => {
    // 冷页面上这条路径在 layout effect 之后的微任务里处理，zustand 的订阅（passive effect）还没建立，
    // 如果 flushSync(begin) 之后外壳没有重渲染，会测到空卡片、退回视图的估计高度（剪贴板 240 对真实的 451）
    cleanup()
    usePanelStore.getState().clear()
    usePanelStore.getState().setPhase('hidden')
    contentHeights({ translate: 333 })
    const off = installPanelEvents()
    const g = globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }
    const saved = g.IS_REACT_ACT_ENVIRONMENT
    g.IS_REACT_ACT_ENVIRONMENT = false
    const host = document.createElement('div')
    document.body.append(host)
    const root = createRoot(host)
    try {
      await waitFor(() => expect(listeners.get('panel:prepare')?.size).toBe(1))
      const p = prepare(translate)
      emit('panel:prepare', p)
      root.render(<PanelApp />)
      await waitFor(() => expect(log).toContain('panel_ready'))
      expect(views.translate.estimatedHeight).not.toBe(333)
      expect(calls.find(([c]) => c === 'panel_ready')?.[1]).toEqual({
        session_id: p.session_id,
        content_height: 333,
      })
    } finally {
      root.unmount()
      host.remove()
      g.IS_REACT_ACT_ENVIRONMENT = saved
      off()
    }
  })

  it('冷唤出的 Boot.prepare：不等任何事件，外壳挂载后直接 ready；只用一次，页面加载后的同会话 emit 不再处理', async () => {
    cleanup()
    usePanelStore.getState().clear()
    usePanelStore.getState().setPhase('hidden')
    const p = prepare(translate)
    boot.prepare = p
    const off = installPanelEvents()
    try {
      expect(boot.prepare).toBeNull()
      render(<PanelApp />)
      await waitFor(() => expect(log.filter((c) => c === 'panel_ready')).toHaveLength(1))
      expect(calls.find(([c]) => c === 'panel_ready')?.[1]).toMatchObject({
        session_id: p.session_id,
      })
      await waitFor(() => expect(usePanelStore.getState().phase).not.toBe('preparing'))
      // Rust 在页面加载后 emit 同一会话作兜底：按会话去重
      act(() => emit('panel:prepare', p))
      await new Promise((r) => setTimeout(r, 10))
      expect(log.filter((c) => c === 'panel_ready')).toHaveLength(1)
    } finally {
      cleanup()
      off()
      boot.prepare = null
    }
  })

  it('Boot 里的会话在页面 JS 运行前已被 Rust 结束（外壳期间 Esc / 失焦 / 截图前隐藏）：ready 回 null → 复位为隐藏、data-hidden 恢复', async () => {
    // hide_request 发出时页面还没有监听（丢失），页面随后凭 Boot.prepare 渲染了已结束的会话。不能停在
    // preparing（视图挂着、data-hidden 被去掉、无限动画恢复、资源释放计时器不排）
    cleanup()
    usePanelStore.getState().clear()
    usePanelStore.getState().setPhase('hidden')
    document.documentElement.dataset.hidden = ''
    const base = fakeTransport()
    let known = false
    setTransport({
      ...base,
      invoke: <T,>(cmd: string, args?: Record<string, unknown>) => {
        if (cmd === 'panel_ready' && !known) {
          calls.push([cmd, args ?? {}])
          log.push(cmd)
          return Promise.resolve(null as T)
        }
        return base.invoke<T>(cmd, args)
      },
    })
    const p = prepare(translate)
    boot.prepare = p
    const off = installPanelEvents()
    try {
      render(<PanelApp />)
      await waitFor(() => expect(log.filter((c) => c === 'panel_ready')).toHaveLength(1))
      await waitFor(() => expect(usePanelStore.getState().phase).toBe('hidden'))
      expect(usePanelStore.getState().prepare).toBeNull()
      expect(document.documentElement.dataset.hidden).toBe('')
      expect(screen.queryByTestId('view-translate')).toBeNull()
      expect(log).not.toContain('panel_painted')
      // 同一会话之后真的到达（Rust 当时还没把暂存变成会话的竞态）：阶段是 hidden，照常处理、ready、显示
      known = true
      act(() => emit('panel:prepare', p))
      await waitFor(() => expect(log.filter((c) => c === 'panel_ready')).toHaveLength(2))
      await waitFor(() => expect(usePanelStore.getState().phase).not.toBe('preparing'))
      expect(usePanelStore.getState().phase).not.toBe('hidden')
      expect(document.documentElement.dataset.hidden).toBeUndefined()
      expect(screen.getByTestId('view-translate')).toBeTruthy()
    } finally {
      cleanup()
      off()
      boot.prepare = null
    }
  })

  it('ready 回 null 时已换成别的会话（被新的 prepare 取代）：不复位新会话', async () => {
    cleanup()
    usePanelStore.getState().clear()
    usePanelStore.getState().setPhase('hidden')
    const base = fakeTransport()
    let release: (() => void) | null = null
    const stale = prepare(translate)
    setTransport({
      ...base,
      invoke: <T,>(cmd: string, args?: Record<string, unknown>) => {
        if (cmd === 'panel_ready' && args?.session_id === stale.session_id) {
          calls.push([cmd, args ?? {}])
          log.push(cmd)
          return new Promise<T>((resolve) => {
            release = () => resolve(null as T)
          })
        }
        return base.invoke<T>(cmd, args)
      },
    })
    boot.prepare = stale
    const off = installPanelEvents()
    try {
      render(<PanelApp />)
      await waitFor(() => expect(release).not.toBeNull())
      const next = prepare(input)
      act(() => emit('panel:prepare', next))
      await waitFor(() =>
        expect(usePanelStore.getState().prepare?.session_id).toBe(next.session_id),
      )
      await act(async () => {
        release?.()
        await new Promise((r) => setTimeout(r, 10))
      })
      expect(usePanelStore.getState().prepare?.session_id).toBe(next.session_id)
      expect(usePanelStore.getState().phase).not.toBe('hidden')
    } finally {
      cleanup()
      off()
      boot.prepare = null
    }
  })

  it('重新加载出来的页面（WebContent 崩溃后的 reload）不采用冻结在初始化脚本里的 Boot.prepare', async () => {
    cleanup()
    usePanelStore.getState().clear()
    usePanelStore.getState().setPhase('hidden')
    const perf = performance as { getEntriesByType?: (type: string) => unknown[] }
    const saved = perf.getEntriesByType
    perf.getEntriesByType = (type: string) => (type === 'navigation' ? [{ type: 'reload' }] : [])
    boot.prepare = prepare(translate)
    let off: (() => void) | null = null
    try {
      off = installPanelEvents()
      expect(boot.prepare).toBeNull()
      render(<PanelApp />)
      await new Promise((r) => setTimeout(r, 20))
      expect(log).not.toContain('panel_ready')
      expect(screen.queryByTestId('view-translate')).toBeNull()
    } finally {
      perf.getEntriesByType = saved
      cleanup()
      off?.()
      boot.prepare = null
    }
  })

  it('还没处理的会话先收到 hide_request：丢掉它，外壳挂载后不显示', async () => {
    cleanup()
    usePanelStore.getState().clear()
    usePanelStore.getState().setPhase('hidden')
    const off = installPanelEvents()
    try {
      await waitFor(() => expect(listeners.get('panel:hide_request')?.size).toBe(1))
      const p = prepare(translate)
      emit('panel:prepare', p)
      emit('panel:hide_request', { session_id: p.session_id, reason: 'other' })
      render(<PanelApp />)
      await new Promise((r) => setTimeout(r, 20))
      expect(log).not.toContain('panel_ready')
      expect(screen.queryByTestId('view-translate')).toBeNull()
    } finally {
      cleanup()
      off()
    }
  })
})

describe('原位交叉淡入', () => {
  it('transition = crossfade 时卡片不重新挂载，只换视图', async () => {
    act(() => emit('panel:prepare', prepare(translate)))
    await waitFor(() => expect(usePanelStore.getState().phase).not.toBe('preparing'))
    const card = screen.getByTestId('panel-card')
    const epoch = usePanelStore.getState().epoch
    act(() => emit('panel:prepare', prepare(input, { transition: 'crossfade' })))
    expect(usePanelStore.getState().epoch).toBe(epoch)
    expect(screen.getByTestId('panel-card')).toBe(card)
    expect(calls.filter(([c]) => c === 'panel_ready')).toHaveLength(2)
    await screen.findByTestId('view-input')
  })

  it('宽度变化的 crossfade prepare：ready 报旧视图高度；新视图暂存、旧视图原地淡出，透明后交换、改宽并立即 panel_resize', async () => {
    boot.frame_mode = 'native'
    contentHeights({ input: 120, translate: 180 })
    act(() => emit('panel:prepare', prepare(input, { width: 400 })))
    await waitFor(() => expect(usePanelStore.getState().phase).toBe('shown'))
    const next = prepare(translate, { width: 520, transition: 'crossfade' })
    act(() => emit('panel:prepare', next))
    const ready = calls.findLast(([c]) => c === 'panel_ready')?.[1]
    expect(ready).toEqual({ session_id: next.session_id, content_height: 120 })
    // 旧视图立即开始原地淡出（仍在布局里，不可交互）
    expect(layerOf('view-input')?.dataset.state).toBe('leaving')
    expect(layerOf('view-input')?.hasAttribute('inert')).toBe(true)
    // 新视图在 transition 里渲染进暂存层：不可见、不参与布局、inert，宽度还没变
    await waitFor(() => expect(layerOf('view-translate')?.dataset.state).toBe('staged'))
    expect(layerOf('view-translate')?.hasAttribute('inert')).toBe(true)
    expect(usePanelStore.getState().width).toBe(400)
    // 交换：新视图放进布局、新宽度生效、立即上报新内容的高度（ResizeObserver 的 stub 不会触发，只能来自同步上报）
    await waitFor(() => expect(layerOf('view-translate')?.dataset.state).toBe('shown'))
    expect(usePanelStore.getState().width).toBe(520)
    expect(calls).toContainEqual([
      'panel_resize',
      { session_id: next.session_id, content_height: 180 },
    ])
    // 已透明的旧视图直接移除
    expect(screen.queryByTestId('view-input')).toBeNull()
  })

  it('视图内切换且宽度变化：新宽度在交换（旧内容完全透明、新视图暂存好）时才生效', async () => {
    boot.frame_mode = 'css'
    act(() => emit('panel:prepare', prepare(input, { width: 420 })))
    await waitFor(() => expect(usePanelStore.getState().phase).not.toBe('preparing'))
    act(() => usePanelStore.getState().switchPayload(translate, 360))
    expect(usePanelStore.getState().width).toBe(420)
    expect(usePanelStore.getState().nextWidth).toBe(360)
    // 暂存层按新宽度排版：Css 帧模式下卡片宽度还是旧的，暂存层宽度 = 当前内容宽度 + 差值
    await waitFor(() => expect(layerOf('view-translate')?.dataset.state).toBe('staged'))
    expect(layerOf('view-translate')?.style.width).toBe('calc(100% - 60px)')
    expect(usePanelStore.getState().width).toBe(420)
    await waitFor(() => expect(usePanelStore.getState().width).toBe(360))
    expect(usePanelStore.getState().nextWidth).toBeNull()
    expect(layerOf('view-translate')?.dataset.state).toBe('shown')
    expect(layerOf('view-translate')?.style.width).toBe('')
  })

  it('宽度不变：暂存好之后旧视图脱离布局淡出、新视图淡入；淡出中的旧视图保留切换前的会话；淡出后移除', async () => {
    boot.frame_mode = 'native'
    contentHeights({ translate: 150, input: 90 })
    const first = prepare(translate)
    act(() => emit('panel:prepare', first))
    await waitFor(() => expect(usePanelStore.getState().phase).toBe('shown'))
    const next = prepare(input, { transition: 'crossfade' })
    act(() => emit('panel:prepare', next))
    // 暂存期间旧视图照常显示，props 定格在切换前（会话不会被新 prepare 覆盖）
    await waitFor(() => expect(layerOf('view-input')?.dataset.state).toBe('staged'))
    expect(layerOf('view-translate')?.dataset.state).toBe('shown')
    expect(screen.getByTestId('view-translate').dataset.session).toBe(String(first.session_id))
    // 交换：旧视图脱离布局淡出，新视图在布局里
    await waitFor(() => expect(layerOf('view-input')?.dataset.state).toBe('shown'))
    expect(layerOf('view-translate')?.dataset.state).toBe('exiting')
    expect(screen.getByTestId('view-translate').dataset.session).toBe(String(first.session_id))
    expect(calls).toContainEqual([
      'panel_resize',
      { session_id: next.session_id, content_height: 90 },
    ])
    await waitFor(() => expect(screen.queryByTestId('view-translate')).toBeNull())
  })

  it('暂存中的视图：autoFocus 推迟到显示时；挂载时的 m.* 进入动画被屏蔽，显示之后挂载的照常', async () => {
    boot.frame_mode = 'native'
    function Probe({ payload }: ViewSwitchRenderProps) {
      return (
        <section data-testid={`view-${payload.kind}`}>
          {/* oxlint-disable-next-line jsx-a11y/no-autofocus -- 输入 / 剪贴板视图的输入框自动聚焦，这里验证暂存视图推迟聚焦 */}
          <TextArea value="" onChange={() => {}} autoFocus aria-label={`draft-${payload.kind}`} />
          <m.div
            data-testid={`probe-${payload.kind}`}
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
          />
        </section>
      )
    }
    setViewOverride((p) => <Probe {...p} />)
    act(() => emit('panel:prepare', prepare(translate)))
    // 会话的第一个视图不在暂存层：挂载即聚焦，m.* 从 initial 起步
    expect(document.activeElement).toBe(screen.getByLabelText('draft-translate'))
    expect(screen.getByTestId('probe-translate').style.opacity).toBe('0')
    await waitFor(() => expect(usePanelStore.getState().phase).toBe('shown'))
    act(() => usePanelStore.getState().switchPayload(input))
    await waitFor(() => expect(layerOf('view-input')?.dataset.state).toBe('staged'))
    expect(document.activeElement).not.toBe(screen.getByLabelText('draft-input'))
    expect(screen.getByTestId('probe-input').style.opacity).toBe('1')
    await waitFor(() => expect(layerOf('view-input')?.dataset.state).toBe('shown'))
    expect(document.activeElement).toBe(screen.getByLabelText('draft-input'))
  })

  it('被替换的旧视图从切换请求起就 inert、键盘作用域停用；新视图显示之后才接管按键', async () => {
    boot.frame_mode = 'native'
    const hits: string[] = []
    function KeyProbe({ payload }: ViewSwitchRenderProps) {
      useKeyScope(`probe-${payload.kind}`, [
        {
          combo: 'Enter',
          allowInInput: true,
          run: () => {
            hits.push(payload.kind)
          },
        },
      ])
      return <section data-testid={`view-${payload.kind}`} />
    }
    setViewOverride((p) => <KeyProbe {...p} />)
    const enter = () =>
      act(() => {
        window.dispatchEvent(
          new KeyboardEvent('keydown', {
            key: 'Enter',
            code: 'Enter',
            bubbles: true,
            cancelable: true,
          }),
        )
      })
    act(() => emit('panel:prepare', prepare(translate)))
    await waitFor(() => expect(usePanelStore.getState().phase).toBe('shown'))
    enter()
    expect(hits).toEqual(['translate'])
    act(() => usePanelStore.getState().switchPayload(input))
    // 暂存期间旧视图仍显示（宽度不变），但已 inert，Enter 不再交给它；新视图也还没接管
    expect(layerOf('view-translate')?.dataset.state).toBe('shown')
    expect(layerOf('view-translate')?.hasAttribute('inert')).toBe(true)
    enter()
    expect(hits).toEqual(['translate'])
    await waitFor(() => expect(layerOf('view-input')?.dataset.state).toBe('shown'))
    await act(async () => {})
    enter()
    expect(hits).toEqual(['translate', 'input'])
  })
})

describe('窗口不能透明（Css 帧模式且 shadow_margin = 0）', () => {
  beforeEach(() => {
    boot.frame_mode = 'css'
    boot.shadow_margin = 0
  })

  it('内容变矮立即 panel_resize（没有卡片高度动画可等，窗口不会停在最高的高度）', async () => {
    let height = 200
    vi.spyOn(HTMLElement.prototype, 'offsetHeight', 'get').mockImplementation(function (
      this: HTMLElement,
    ) {
      return this.hasAttribute('data-panel-content') ? height : 0
    })
    const frames: FrameRequestCallback[] = []
    const p = prepare(translate)
    act(() => emit('panel:prepare', p))
    await waitFor(() => expect(usePanelStore.getState().phase).toBe('shown'))
    vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => frames.push(cb))
    const step = (h: number) => {
      height = h
      act(() => {
        triggerResizeObservers()
        for (const cb of frames.splice(0)) cb(performance.now())
      })
    }
    step(260)
    step(120)
    const resizes = calls
      .filter(([c]) => c === 'panel_resize')
      .map(([, a]) => a.content_height as number)
    expect(resizes).toEqual([260, 120])
  })

  it('外框不做透明度 / 缩放 / 位移，页面底色为实色；只有内容淡入', async () => {
    act(() => emit('panel:prepare', prepare(translate)))
    const frame = screen.getByTestId('panel-card-frame')
    expect(frame.hasAttribute('data-opaque')).toBe(true)
    expect(frame.style.opacity).toBe('')
    expect(frame.style.transform).toBe('')
    expect(document.documentElement.hasAttribute('data-opaque')).toBe(true)
    // prepare 阶段内容透明（卡片本身已是实色），进入后淡入到 1 并走到 shown
    const content = frame.querySelector<HTMLElement>('[data-panel-content]')
    expect(content?.style.opacity).toBe('0')
    await waitFor(() => expect(usePanelStore.getState().phase).toBe('shown'))
    expect(content?.style.opacity).toBe('1')
  })

  it('退出：不播动画，立即 panel_hide；fast 之后卸载', async () => {
    const p = prepare(translate)
    act(() => emit('panel:prepare', p))
    await waitFor(() => expect(usePanelStore.getState().phase).toBe('shown'))
    act(() => emit('panel:hide_request', { session_id: p.session_id, reason: 'escape' }))
    expect(calls.find(([c]) => c === 'panel_hide')?.[1]).toEqual({
      session_id: p.session_id,
      reason: 'escape',
    })
    const frame = screen.getByTestId('panel-card-frame')
    expect(frame.style.opacity).toBe('')
    await waitFor(() => expect(usePanelStore.getState().phase).toBe('hidden'))
  })
})

describe('进入', () => {
  it('Native + 减少动效：内容没有位移动画，照样从 entering 走到 shown', async () => {
    boot.frame_mode = 'native'
    act(() => emit('panel:prepare', prepare(translate)))
    await waitFor(() => expect(usePanelStore.getState().phase).toBe('shown'))
  })

  it('Native：完整动效下进入动画播完后到 shown', async () => {
    boot.frame_mode = 'native'
    setAppearance({ ...getAppearance(), reduceMotion: false })
    act(() => emit('panel:prepare', prepare(translate)))
    await waitFor(() => expect(usePanelStore.getState().phase).toBe('shown'), { timeout: 5000 })
  })
})

describe('重复的 prepare', () => {
  it('同一会话的 prepare 再次到达（Rust 补发）：不重新挂载卡片、不再次 ready', async () => {
    boot.frame_mode = 'css'
    const p = prepare(translate)
    act(() => emit('panel:prepare', p))
    await waitFor(() => expect(calls.some(([c]) => c === 'panel_painted')).toBe(true))
    const epoch = usePanelStore.getState().epoch
    const readies = calls.filter(([c]) => c === 'panel_ready').length
    act(() => emit('panel:prepare', p))
    expect(usePanelStore.getState().epoch).toBe(epoch)
    expect(calls.filter(([c]) => c === 'panel_ready').length).toBe(readies)
    // 收尾：隐藏面板，不把可见的会话与迟到的回调留给下一个用例
    act(() => emit('panel:hide_request', { session_id: p.session_id, reason: 'escape' }))
    await waitFor(() => expect(usePanelStore.getState().phase).toBe('hidden'))
  })
})

describe('退出与隐藏', () => {
  it('ready 返回前收到 hide_request：不会被改回 entering，Css 照样 panel_hide', async () => {
    boot.frame_mode = 'css'
    const p = prepare(translate)
    const phases: string[] = []
    const unsubscribe = usePanelStore.subscribe((st) => phases.push(st.phase))
    act(() => {
      emit('panel:prepare', p)
      emit('panel:hide_request', { session_id: p.session_id, reason: 'click_outside' })
    })
    await waitFor(() => expect(calls.some(([c]) => c === 'panel_hide')).toBe(true))
    await waitFor(() => expect(usePanelStore.getState().phase).toBe('hidden'))
    unsubscribe()
    expect(phases).not.toContain('entering')
    expect(calls.some(([c]) => c === 'panel_painted')).toBe(false)
  })

  it('Native：立即 panel_hide（窗口 alpha 由 Rust 淡出），之后卸载视图并标记隐藏', async () => {
    boot.frame_mode = 'native'
    const p = prepare(translate)
    act(() => emit('panel:prepare', p))
    await waitFor(() => expect(usePanelStore.getState().phase).not.toBe('preparing'))
    act(() => emit('panel:hide_request', { session_id: p.session_id, reason: 'escape' }))
    expect(calls.find(([c]) => c === 'panel_hide')?.[1]).toEqual({
      session_id: p.session_id,
      reason: 'escape',
    })
    await waitFor(() => expect(usePanelStore.getState().phase).toBe('hidden'))
    expect(document.documentElement.hasAttribute('data-hidden')).toBe(true)
    expect(screen.queryByTestId('panel-card')).toBeNull()
    expect(usePanelStore.getState().payload).toBeNull()
  })

  it('Css：先播退出动画再 panel_hide；下一次 prepare 清除隐藏标记', async () => {
    boot.frame_mode = 'css'
    const p = prepare(translate)
    act(() => emit('panel:prepare', p))
    await waitFor(() => expect(usePanelStore.getState().phase).not.toBe('preparing'))
    act(() => emit('panel:hide_request', { session_id: p.session_id, reason: 'click_outside' }))
    expect(usePanelStore.getState().phase).toBe('exiting')
    expect(calls.some(([c]) => c === 'panel_hide')).toBe(false)
    await waitFor(() => expect(calls.some(([c]) => c === 'panel_hide')).toBe(true))
    expect(usePanelStore.getState().phase).toBe('hidden')
    act(() => emit('panel:prepare', prepare(translate)))
    expect(document.documentElement.hasAttribute('data-hidden')).toBe(false)
  })

  it('Esc 经键盘路由的全局层关闭面板', async () => {
    boot.frame_mode = 'native'
    const p = prepare(input, { focused: true })
    act(() => emit('panel:prepare', p))
    await waitFor(() => expect(usePanelStore.getState().phase).not.toBe('preparing'))
    act(() => {
      window.dispatchEvent(
        new KeyboardEvent('keydown', {
          key: 'Escape',
          code: 'Escape',
          bubbles: true,
          cancelable: true,
        }),
      )
    })
    expect(calls.find(([c]) => c === 'panel_hide')?.[1]).toEqual({
      session_id: p.session_id,
      reason: 'escape',
    })
  })
})
