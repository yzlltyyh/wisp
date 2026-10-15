/**
 * 浏览器 mock 引擎入口。只在 `isTauri()` 为 false 且 `__WISP_MOCK__` 为 true 时被动态导入。
 *
 * URL 参数：`?label=panel&scenario=translate.streaming-learn&theme=dark&os=windows&lang=en-US&reduced=1&material=solid`
 * 另有：`frame=native|css`、`hidden_raf=0`、`hud=0|1`、`desktop=0`、`ipc_log=1`、`ipc_latency=<ms>`、`dev=1`、
 * `panel_width=<px>`（覆盖场景的卡片宽度）。
 *
 * 场景放在 `scenarios/<文件>.ts`（按视图分文件），每个文件导出 `scenarios: Scenario[]`，id 的前缀即文件名。
 */
import { boot, devFlags, windowKindOf, type WindowKind } from '../boot'
import type { Transport } from '../transport'
import type { PanelPayload, PanelPrepare, PanelView, Rect, Settings } from '../types'
import { MockAssets } from './assets'
import { MockBus } from './bus'
import { createState, installDefaults, type MockState } from './defaults'
import { mountDesktop } from './desktop'
import { createFixtures, SAMPLE, type MockFixtures } from './fixtures'
import { mountHud } from './hud'
import { MockRouter } from './router'
import { errorStream, MockStreams, streamRate, textStream } from './streams'
import type { MockContext, MockEntry, PrepareOptions, Scenario } from './types'
import { frameStaticWindow, MockPanelWindow } from './window'

export type * from './types'
export { mockError } from './types'

interface ScenarioModule {
  scenarios?: readonly Scenario[]
  default?: readonly Scenario[]
}

const scenarioModules = import.meta.glob<ScenarioModule>('./scenarios/*.ts')

/** 列出全部场景（HUD、QA 截图脚本经 `window.__WISP_MOCK__.listScenarios()` 使用） */
export async function listScenarios(): Promise<Scenario[]> {
  const modules = await Promise.all(Object.values(scenarioModules).map((load) => load()))
  return modules.flatMap((m) => [...(m.scenarios ?? m.default ?? [])])
}

async function findScenario(id: string): Promise<Scenario | undefined> {
  const file = id.split('.')[0] ?? ''
  const load = scenarioModules[`./scenarios/${file}.ts`]
  if (!load) return undefined
  const m = await load()
  return (m.scenarios ?? m.default ?? []).find((s) => s.id === id)
}

/** 场景应在哪个入口、以哪个 label 打开 */
export function entryOf(s: Pick<Scenario, 'view' | 'entry'>): { entry: MockEntry; label: string } {
  if (s.entry) return { entry: s.entry, label: defaultLabel(s.entry) }
  switch (s.view) {
    case 'settings':
      return { entry: 'settings', label: 'settings' }
    case 'onboarding':
      return { entry: 'settings', label: 'onboarding' }
    case 'pinned':
      return { entry: 'pinned', label: 'pinned-mock' }
    case 'overlay':
      return { entry: 'overlay', label: 'overlay-0' }
    default:
      return { entry: 'panel', label: 'panel' }
  }
}

/** 场景的相对 URL（QA 再追加 theme / os / reduced 等参数） */
export function scenarioUrl(s: Pick<Scenario, 'id' | 'view' | 'entry'>): string {
  const { entry, label } = entryOf(s)
  return `/${entry}.html?label=${encodeURIComponent(label)}&scenario=${encodeURIComponent(s.id)}`
}

function defaultLabel(entry: MockEntry): string {
  return entry === 'pinned' ? 'pinned-mock' : entry === 'overlay' ? 'overlay-0' : entry
}

export interface MockHandle {
  readonly bus: MockBus
  readonly router: MockRouter
  readonly streams: MockStreams
  readonly state: MockState
  readonly fixtures: MockFixtures
  readonly ctx: MockContext
  readonly panel: MockPanelWindow | null
  listScenarios(): Promise<Array<Scenario & { url: string }>>
}

declare global {
  interface Window {
    /** 调试 / Playwright 用的 mock 句柄（只在 mock 模式存在） */
    __WISP_MOCK__?: MockHandle
  }
}

/** 安装 mock：模拟窗口、假桌面、默认命令、场景；resolve 后各入口才挂载（见 runtime.whenReady） */
export async function installMock(): Promise<Transport> {
  const q = new URLSearchParams(location.search)
  const label = boot.label
  const kind = windowKindOf(label)
  const bus = new MockBus()
  const router = new MockRouter({
    latencyMs: Number(q.get('ipc_latency')) || 0,
    log: q.get('ipc_log') === '1',
  })
  const streams = new MockStreams()
  const fixtures = createFixtures(boot)
  const state = createState(fixtures)
  const assets = new MockAssets()
  const transparent = kind === 'panel' || kind === 'pinned' || kind === 'overlay'
  const desktop = transparent && q.get('desktop') !== '0' ? mountDesktop(boot) : null
  const panel =
    kind === 'panel' ? new MockPanelWindow(boot, bus, desktop, devFlags.hiddenRafBlocked) : null
  if (!panel) frameStaticWindow(boot, desktop)
  installDefaults({ boot, bus, router, streams, state, fixtures, assets, panel, desktop })

  let pendingTimers = 0
  let sessionSeq = 0

  const ctx: MockContext = {
    boot,
    label,
    fixtures,
    emit: (event, payload) => bus.emitWhenListened(event, payload),
    handle: (cmd, handler) => router.handle(cmd, handler),
    nextStream: (script) => streams.enqueue(script),
    stream: (requestId, script) => streams.start(requestId, script),
    textStream,
    streamRate,
    errorStream,
    preparePanel(payload, options = {}) {
      const session_id = ++sessionSeq
      startPayloadStream(payload, streams)
      const prepare = buildPrepare(session_id, payload, options, state.settings, panel, desktop)
      panel?.setNextAnchor(options.anchor ?? null)
      bus.emitWhenListened('panel:prepare', prepare)
      return session_id
    },
    requestHide: (reason = 'escape') => panel?.requestHide(reason),
    asset: (path, body, headers) => assets.register(path, body, headers),
    regionAsset: (base, fullPath) => assets.rgbaRegions(base, fullPath),
    after(ms, fn) {
      pendingTimers++
      setTimeout(() => {
        pendingTimers--
        fn()
      }, ms)
    },
    settled() {
      document.documentElement.dataset.settled = 'true'
    },
    motionMid() {
      document.documentElement.dataset.motionMid = 'true'
    },
    log: (...args) => console.info('[mock]', ...args),
  }

  const autoSettle = () => {
    const started = performance.now()
    let quiet = 0
    const timer = setInterval(() => {
      const busy = bus.pending > 0 || streams.active > 0 || pendingTimers > 0
      quiet = busy ? 0 : quiet + 1
      if (quiet >= 4 || performance.now() - started > 10_000) {
        clearInterval(timer)
        ctx.settled()
      }
    }, 100)
  }

  const transport: Transport = {
    kind: 'mock',
    invoke: <T>(cmd: string, args?: Record<string, unknown>) =>
      router.dispatch(cmd, args) as Promise<T>,
    openStream: <T>(
      cmd: string,
      args: Record<string, unknown>,
      onEvent: Parameters<Transport['openStream']>[2],
    ) => router.dispatch(cmd, args, onEvent) as Promise<T>,
    listen: (event, cb) => Promise.resolve(bus.listen(event, cb)),
    window: {
      label,
      startDragging: async () => ctx.log('startDragging'),
      close: async () => {
        const root = document.getElementById('root')
        if (root) root.style.visibility = 'hidden'
        ctx.log('window.close', label)
      },
      setSize: async (width, height) => {
        const root = document.getElementById('root')
        if (root) Object.assign(root.style, { width: `${width}px`, height: `${height}px` })
      },
    },
    protocolUrl: (path) => assets.url(path),
  }

  window.__WISP_MOCK__ = {
    bus,
    router,
    streams,
    state,
    fixtures,
    ctx,
    panel,
    listScenarios: async () => (await listScenarios()).map((s) => ({ ...s, url: scenarioUrl(s) })),
  }

  const scenarioId = devFlags.scenario
  let scenario: Scenario | undefined
  if (scenarioId) {
    scenario = await findScenario(scenarioId)
    if (scenario) {
      document.documentElement.dataset.scenario = scenario.id
      await scenario.setup(ctx)
    } else {
      console.error(`[mock] 找不到场景 "${scenarioId}"`)
    }
  }
  if (!scenario?.manualSettle) autoSettle()

  const hud = q.get('hud')
  if (hud === '1' || (hud !== '0' && !scenarioId)) {
    mountHud({
      boot,
      kind: kind satisfies WindowKind,
      ctx,
      fixtures,
      streams,
      listScenarios,
      entryOf,
    })
  }

  return transport
}

// ---------------------------------------------------------------------------

/** 载荷里带 request_id 但还没有对应的流时，按默认脚本开始播放（相当于 Rust 在 t0 发出请求） */
function startPayloadStream(payload: PanelPayload, streams: MockStreams): void {
  if (payload.kind !== 'translate' && payload.kind !== 'ocr') return
  const id = payload.request_id
  if (!id || streams.has(id)) return
  const script =
    streams.takeQueued() ??
    (payload.kind === 'translate'
      ? textStream(SAMPLE.translation, {
          meta: { detected_source_lang: payload.detected_lang, target_lang: payload.target_lang },
        })
      : textStream(SAMPLE.ocr, { chunkChars: 3 }))
  streams.start(id, script)
}

const CJK = /[぀-ヿ㐀-鿿가-힯＀-￯]/

/** 面板宽度规则（mock 近似 Rust 的取值） */
function viewWidth(view: PanelView, payload: PanelPayload, settings: Settings): number {
  switch (view) {
    case 'ocr':
      return 440
    case 'input':
      return 420
    case 'clipboard':
      return settings.appearance.clipboard_width
    case 'toast':
      return 280
    case 'translate': {
      const { translate_min_width: min, translate_max_width: max } = settings.appearance
      if (payload.kind !== 'translate' || payload.dict_mode) return min
      let units = 0
      for (const ch of payload.source_text) units += CJK.test(ch) ? 2 : 1
      const t = Math.min(1, Math.max(0, (units - 80) / 320))
      return Math.round(min + (max - min) * t)
    }
  }
}

function buildPrepare(
  session_id: number,
  payload: PanelPayload,
  options: PrepareOptions,
  settings: Settings,
  panel: MockPanelWindow | null,
  desktop: ReturnType<typeof mountDesktop> | null,
): PanelPrepare {
  const view = payload.kind
  const work: Rect = panel?.workArea() ?? { x: 0, y: 0, width: innerWidth, height: innerHeight }
  // `?panel_width=360`：覆盖场景给的卡片宽度（窄卡片 / 伪本地化下的版式用例）
  const forced = Number(new URLSearchParams(location.search).get('panel_width'))
  const width = forced > 0 ? forced : (options.width ?? viewWidth(view, payload, settings))
  const placement = options.placement ?? (view === 'clipboard' ? 'center' : 'below')
  const anchor = options.anchor ??
    desktop?.selectionRect() ?? { x: work.width * 0.3, y: work.height * 0.3, width: 1, height: 20 }
  const cardX = Math.max(work.x, Math.min(anchor.x, work.x + work.width - width))
  const originX =
    placement === 'center'
      ? width / 2
      : Math.max(0, Math.min(width, anchor.x + Math.min(anchor.width, 48) / 2 - cardX))
  return {
    session_id,
    view,
    payload,
    width,
    max_height: Math.round(options.maxHeight ?? work.height * settings.appearance.max_height_ratio),
    placement,
    origin_x: Math.round(originX),
    focused: options.focused ?? (view === 'clipboard' || view === 'input'),
    transition: options.transition ?? 'enter',
  }
}
