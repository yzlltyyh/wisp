/**
 * 设置窗口里与离线模型、PaddleOCR 服务有关的 mock 场景（由 scenarios/settings.ts 收进它的场景表，id 以 `settings.`
 * 开头）。每个场景只覆写 `state.offline.snapshot` 与设置里的 `offline.*`，命令的默认处理函数照常工作（localModels.ts）：
 * 在场景里点「下载」同样会走一遍模拟的进度。
 */
import type {
  DeviceInfo,
  LocalModelId,
  LocalModelInfo,
  LocalModelsSnapshot,
  LocalRuntimeId,
  Provider,
} from '../types'
import type { MockHandle } from './index'
import type { MockContext, Scenario } from './types'

type PageFn = (
  id: string,
  label: string,
  section: string,
  extra?: (ctx: MockContext, mock: MockHandle) => void,
) => Scenario

type ClickFn = (ctx: MockContext, selector: string, then?: () => void) => void
/** 按模板生成服务（scenarios/settings.ts 的 templateProvider；作为参数传入，不反向依赖场景表） */
type ProviderFn = (templateId: string, providerId?: string) => Provider

const MB = 1_000_000
const GB = 1024 * 1024 * 1024

/** 改几个模型的字段 */
export function patchModels(
  mock: MockHandle,
  patches: Partial<Record<LocalModelId, Partial<LocalModelInfo>>>,
): void {
  const snap = mock.state.offline.snapshot
  mock.state.offline.snapshot = {
    ...snap,
    models: snap.models.map((m) => ({ ...m, ...patches[m.id] })),
  }
}

/** 把推理组件标成已安装，并按已安装的模型重算占用空间（与 localModels.ts 的 withPending 同一口径） */
export function installRuntimes(mock: MockHandle, ids: readonly LocalRuntimeId[]): void {
  const snap = mock.state.offline.snapshot
  const runtimes = snap.runtimes.map((r) => (ids.includes(r.id) ? { ...r, installed: true } : r))
  mock.state.offline.snapshot = recompute({ ...snap, runtimes })
}

function recompute(s: LocalModelsSnapshot): LocalModelsSnapshot {
  const installed = (id: LocalRuntimeId) => s.runtimes.some((r) => r.id === id && r.installed)
  const deps: Record<LocalModelId, LocalRuntimeId[]> = {
    pp_ocrv6_tiny: ['onnx_runtime'],
    pp_ocrv6_small: ['onnx_runtime'],
    paddleocr_vl_1_6: ['llama_cpp', 'onnx_runtime'],
    index_translate_2b: ['llama_cpp'],
    index_translate_9b: ['llama_cpp'],
  }
  const size = (id: LocalRuntimeId) => s.runtimes.find((r) => r.id === id)?.size ?? 0
  const models = s.models.map((m) => ({
    ...m,
    pending_runtime_bytes: deps[m.id]
      .filter((r) => !installed(r))
      .reduce((sum, r) => sum + size(r), 0),
  }))
  const runtimes = s.runtimes.map((r) => ({
    ...r,
    in_use: models.some((m) => m.state.state === 'installed' && deps[m.id].includes(r.id)),
  }))
  const disk_used =
    models
      .filter((m) => m.state.state === 'installed')
      .reduce((sum, m) => sum + m.download_bytes, 0) +
    runtimes.filter((r) => r.installed).reduce((sum, r) => sum + r.size, 0)
  return { ...s, models, runtimes, disk_used }
}

/** 已下载：小模型 + 2B（两个推理组件都装好），并选用它们 */
export function installedPair(mock: MockHandle, select = true): void {
  patchModels(mock, {
    pp_ocrv6_small: { state: { state: 'installed' } },
    index_translate_2b: { state: { state: 'installed' } },
  })
  installRuntimes(mock, ['onnx_runtime', 'llama_cpp'])
  if (select) {
    mock.state.settings = {
      ...mock.state.settings,
      offline: {
        ...mock.state.settings.offline,
        ocr_model: 'pp_ocrv6_small',
        translate_model: 'index_translate_2b',
      },
    }
  }
}

/** 一台 8 GB、没有独立显卡的 Windows 电脑（显卡已检测） */
const LOW_END: DeviceInfo = {
  total_memory: 8 * GB,
  apple_silicon: false,
  gpus: [],
  gpu_probed: true,
}

function setDevice(mock: MockHandle, device: DeviceInfo): void {
  mock.state.offline.snapshot = { ...mock.state.offline.snapshot, device }
}

/**
 * 32 GB、只有集成显卡的 Windows 笔记本：llama.cpp 装好、检测完显卡后，9B 与 VL 的下载停在推理组件之后（自动暂停），
 * 等用户看过检测结论再决定是否继续
 */
function detectedPaused(mock: MockHandle): void {
  setDevice(mock, {
    total_memory: 32 * GB,
    apple_silicon: false,
    gpus: [{ name: 'Intel(R) Iris(R) Xe Graphics', vram_total_mb: 16_384, integrated: true }],
    gpu_probed: true,
  })
  installRuntimes(mock, ['llama_cpp'])
  const cpu = { level: 'slow', reason_key: 'settings.offline.fitness.integrated_gpu' } as const
  patchModels(mock, {
    paddleocr_vl_1_6: {
      fitness: cpu,
      state: { state: 'paused', done: 32.2 * MB, total: 1985.8 * MB + 5.7 * MB },
    },
    index_translate_9b: {
      fitness: cpu,
      state: { state: 'paused', done: 32.2 * MB, total: 5812 * MB },
    },
  })
}

/** 点容器里文字包含 `text` 的菜单项（菜单项的 id 是模型引用，里面有不能写进选择器的分隔符）；最多等 5 s */
function clickText(ctx: MockContext, container: string, text: string): void {
  let tries = 0
  const tick = () => {
    const items = document.querySelectorAll<HTMLElement>(`${container} [role^="menuitem"]`)
    const hit = Array.from(items).find((el) => el.textContent?.includes(text))
    if (hit) {
      hit.click()
      return
    }
    tries += 1
    if (tries < 100) ctx.after(50, tick)
  }
  ctx.after(50, tick)
}

export function offlineScenarios(page: PageFn, click: ClickFn, provider: ProviderFn): Scenario[] {
  const addPaddle = (mock: MockHandle, url?: string) => {
    const p = provider('paddleocr')
    if (url) p.base_url = url
    mock.state.settings.providers = [...mock.state.settings.providers, p]
    mock.state.hasKey = { ...mock.state.hasKey, paddleocr: true }
  }
  return [
    page('offline', '设置 · 离线模型（未下载，Apple 芯片 16 GB）', 'offline'),
    page(
      'offline-downloading',
      '设置 · 离线模型（下载中 42% · 排队 · 已暂停 · 已选用）',
      'offline',
      (_ctx, mock) => {
        installedPair(mock, false)
        patchModels(mock, {
          index_translate_2b: { state: { state: 'not_installed' } },
          paddleocr_vl_1_6: {
            state: {
              state: 'downloading',
              done: 0.42 * 1985.8 * MB,
              total: 1985.8 * MB,
              bytes_per_sec: 12.4 * MB,
            },
          },
          index_translate_9b: { state: { state: 'queued' } },
          pp_ocrv6_tiny: {
            state: { state: 'paused', done: 2.1 * MB, total: 6.2 * MB },
          },
        })
        installRuntimes(mock, ['onnx_runtime'])
        mock.state.settings.offline.ocr_model = 'pp_ocrv6_small'
      },
    ),
    page(
      'offline-installed',
      '设置 · 离线模型（已下载并选用；识别模型已加载、翻译模型加载中）',
      'offline',
      (_ctx, mock) => {
        installedPair(mock)
        patchModels(mock, {
          pp_ocrv6_small: { load: 'loaded' },
          index_translate_2b: { load: 'loading' },
        })
      },
    ),
    page(
      'offline-low-memory',
      '设置 · 离线模型（8 GB、只有 CPU：较慢 / 内存不足）',
      'offline',
      (_ctx, mock) => {
        setDevice(mock, LOW_END)
        installRuntimes(mock, ['llama_cpp'])
        patchModels(mock, {
          paddleocr_vl_1_6: {
            fitness: { level: 'slow', reason_key: 'settings.offline.fitness.cpu_only' },
          },
          index_translate_2b: {
            fitness: { level: 'slow', reason_key: 'settings.offline.fitness.low_memory' },
          },
          index_translate_9b: {
            fitness: { level: 'insufficient', reason_key: 'settings.offline.fitness.low_memory' },
          },
        })
      },
    ),
    page(
      'offline-unfit-confirm',
      '设置 · 离线模型（内存不足时点下载：原地确认）',
      'offline',
      (ctx, mock) => {
        setDevice(mock, LOW_END)
        patchModels(mock, {
          index_translate_9b: {
            fitness: { level: 'insufficient', reason_key: 'settings.offline.fitness.low_memory' },
          },
        })
        click(ctx, '[data-testid="offline-model-index_translate_9b-download"]')
      },
    ),
    page(
      'offline-gpu-pending',
      '设置 · 离线模型（Windows：显卡待检测）',
      'offline',
      (_ctx, mock) => {
        setDevice(mock, {
          total_memory: 32 * GB,
          apple_silicon: false,
          gpus: [],
          gpu_probed: false,
        })
        patchModels(mock, {
          paddleocr_vl_1_6: {
            fitness: { level: 'unknown', reason_key: 'settings.offline.fitness.gpu_unknown' },
          },
          index_translate_9b: {
            fitness: { level: 'unknown', reason_key: 'settings.offline.fitness.gpu_unknown' },
          },
        })
      },
    ),
    page(
      'offline-gpu-pending-confirm',
      '设置 · 离线模型（Windows：显卡待检测时点下载 9B：原地确认）',
      'offline',
      (ctx, mock) => {
        setDevice(mock, { ...LOW_END, total_memory: 32 * GB, gpu_probed: false })
        patchModels(mock, {
          paddleocr_vl_1_6: {
            fitness: { level: 'unknown', reason_key: 'settings.offline.fitness.gpu_unknown' },
          },
          index_translate_9b: {
            fitness: { level: 'unknown', reason_key: 'settings.offline.fitness.gpu_unknown' },
          },
        })
        click(ctx, '[data-testid="offline-model-index_translate_9b-download"]')
      },
    ),
    page(
      'offline-detected-paused',
      '设置 · 离线模型（装好推理组件后检测出只有 CPU / 集成显卡：下载自动暂停，写出检测结论）',
      'offline',
      (_ctx, mock) => detectedPaused(mock),
    ),
    page(
      'offline-detected-resume',
      '设置 · 离线模型（检测后自动暂停的 9B 点「继续」：原地确认）',
      'offline',
      (ctx, mock) => {
        detectedPaused(mock)
        click(ctx, '[data-testid="offline-model-index_translate_9b-resume"]')
      },
    ),
    page(
      'offline-gpu',
      '设置 · 离线模型（Windows：已检测到 NVIDIA 显卡）',
      'offline',
      (_ctx, mock) => {
        setDevice(mock, {
          total_memory: 32 * GB,
          apple_silicon: false,
          gpus: [{ name: 'NVIDIA GeForce RTX 3060', vram_total_mb: 12_288, integrated: false }],
          gpu_probed: true,
        })
        installedPair(mock)
        patchModels(mock, { index_translate_9b: { state: { state: 'installed' }, load: 'loaded' } })
        mock.state.settings.offline.translate_model = 'index_translate_9b'
        installRuntimes(mock, ['llama_cpp', 'onnx_runtime'])
      },
    ),
    page(
      'offline-failed',
      '设置 · 离线模型（下载失败 · 文件损坏 · 有新版本）',
      'offline',
      (_ctx, mock) => {
        installedPair(mock, false)
        patchModels(mock, {
          paddleocr_vl_1_6: { state: { state: 'failed', code: 'network' } },
          index_translate_9b: { state: { state: 'failed', code: 'disk_full' } },
          pp_ocrv6_small: { state: { state: 'broken' } },
          index_translate_2b: { update_available: true },
        })
        installRuntimes(mock, ['llama_cpp', 'onnx_runtime'])
      },
    ),
    page('offline-delete-confirm', '设置 · 离线模型（删除：原地确认）', 'offline', (ctx, mock) => {
      installedPair(mock)
      click(ctx, '[data-testid="offline-model-pp_ocrv6_small-more"]', () =>
        click(
          ctx,
          '[data-testid="offline-model-pp_ocrv6_small-menu"] [role="menuitem"]:last-child',
        ),
      )
    }),
    page(
      'providers-offline-default',
      '设置 · 模型与服务（默认模型：翻译选了离线模型）',
      'providers',
      (_ctx, mock) => installedPair(mock),
    ),
    page(
      'providers-offline-menu',
      '设置 · 模型与服务（默认 OCR 下拉：离线 / PaddleOCR / 各服务分组）',
      'providers',
      (ctx, mock) => {
        addPaddle(mock)
        patchModels(mock, {
          pp_ocrv6_small: { state: { state: 'installed' } },
          paddleocr_vl_1_6: {
            state: {
              state: 'downloading',
              done: 0.3 * 1985.8 * MB,
              total: 1985.8 * MB,
              bytes_per_sec: 9 * MB,
            },
          },
        })
        installRuntimes(mock, ['onnx_runtime'])
        click(ctx, '[data-testid="defaults-ocr-select"]')
      },
    ),
    page(
      'providers-offline-confirm',
      '设置 · 模型与服务（选了未下载的离线翻译模型：下载并使用）',
      'providers',
      (ctx) =>
        click(ctx, '[data-testid="defaults-translate-select"]', () =>
          clickText(ctx, '[data-testid="defaults-translate-select-menu"]', 'Index-Translate 2B'),
        ),
    ),
    page('providers-paddle', '设置 · 模型与服务（PaddleOCR 服务）', 'providers', (ctx, mock) => {
      addPaddle(mock)
      click(ctx, '[data-testid="provider-row-paddleocr"]')
    }),
    page(
      'providers-paddle-insecure',
      '设置 · 模型与服务（PaddleOCR：公网地址写了 http，提示需要 https）',
      'providers',
      (ctx, mock) => {
        addPaddle(mock, 'http://ocr.example.com/layout-parsing')
        click(ctx, '[data-testid="provider-row-paddleocr"]')
      },
    ),
    page(
      'providers-paddle-serving',
      '设置 · 模型与服务（PaddleOCR：自托管 PaddleX 服务）',
      'providers',
      (ctx, mock) => {
        addPaddle(mock, 'http://192.168.1.20:8080/layout-parsing')
        click(ctx, '[data-testid="provider-row-paddleocr"]')
      },
    ),
    page('ocr-offline', '设置 · OCR（选了 PP-OCRv6：结构化置灰）', 'ocr', (_ctx, mock) => {
      installedPair(mock)
    }),
  ]
}
