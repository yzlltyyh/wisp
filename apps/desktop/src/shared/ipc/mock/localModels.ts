/**
 * mock 的离线模型：一份内存里的快照 + 离线模型命令的默认处理函数。行为按 Rust 的契约近似：下载在后台按固定间隔推进度
 * （`offline_models:changed`），先补齐缺的推理组件再下模型；完成后进入「已安装」并刷新设置快照（`RuntimeInfo.offline`）；
 * 暂停保留进度、取消清零；删除仍被依赖的推理组件抛 `invalid_argument`。设置页的各种场景（下载中 / 内存不足 / 待检测……）
 * 在此基础上覆写 `state.offline.snapshot`。
 */
import type {
  LocalModelId,
  LocalModelInfo,
  LocalModelsSnapshot,
  LocalRuntimeId,
  OfflineSummary,
  OsKind,
  SettingsSnapshot,
} from '../types'
import type { DefaultsDeps } from './defaults'
import { mockError } from './types'

/** 进度推进的间隔与步数（约 2.5 s 下完，够截图与手动点暂停） */
const TICK_MS = 250
const TICKS = 10

export interface MockOffline {
  snapshot: LocalModelsSnapshot
}

const MB = 1_000_000

/** 各模型依赖的推理组件（与 Rust 目录一致） */
export const MODEL_RUNTIMES: Record<LocalModelId, readonly LocalRuntimeId[]> = {
  pp_ocrv6_tiny: ['onnx_runtime'],
  pp_ocrv6_small: ['onnx_runtime'],
  paddleocr_vl_1_6: ['llama_cpp', 'onnx_runtime'],
  index_translate_2b: ['llama_cpp'],
  index_translate_9b: ['llama_cpp'],
}

function model(
  id: LocalModelId,
  download_bytes: number,
  memory_estimate_mb: number,
): LocalModelInfo {
  return {
    id,
    kind: id.startsWith('index_') ? 'translate' : 'ocr',
    download_bytes,
    pending_runtime_bytes: 0,
    state: { state: 'not_installed' },
    load: 'unloaded',
    memory_estimate_mb,
    fitness: { level: 'good', reason_key: null },
    update_available: false,
  }
}

/**
 * 各平台推理组件的下载量（与 Rust 目录里的发布包一致）。Windows 的 ONNX Runtime 从整包（约 79 MB）里按区间只取
 * onnxruntime.dll，实际下载约 5.7 MB
 */
const RUNTIME_BYTES: Record<OsKind, Record<LocalRuntimeId, number>> = {
  macos: { llama_cpp: 12_191_582, onnx_runtime: 33_057_743 },
  windows: { llama_cpp: 32_182_350, onnx_runtime: 5_730_829 },
  linux: { llama_cpp: 30_497_889, onnx_runtime: 9_130_098 },
}

export function createOfflineState(os: OsKind = 'macos'): MockOffline {
  const bytes = RUNTIME_BYTES[os]
  const snapshot: LocalModelsSnapshot = {
    models: [
      model('pp_ocrv6_tiny', 6.2 * MB, 200),
      model('pp_ocrv6_small', 31 * MB, 400),
      model('paddleocr_vl_1_6', 1953 * MB, 3000),
      model('index_translate_2b', 1312 * MB, 2200),
      model('index_translate_9b', 5780 * MB, 7500),
    ],
    runtimes: [
      {
        id: 'llama_cpp',
        version: 'b11669',
        installed: false,
        size: bytes.llama_cpp,
        in_use: false,
      },
      {
        id: 'onnx_runtime',
        version: '1.28.3',
        installed: false,
        size: bytes.onnx_runtime,
        in_use: false,
      },
    ],
    device: { total_memory: 17_179_869_184, apple_silicon: true, gpus: [], gpu_probed: true },
    disk_used: 0,
    disk_free: 120_000 * MB,
  }
  return { snapshot: withPending(snapshot) }
}

/** 按推理组件的安装情况重算每个模型的「另需下载」与组件的 in_use */
function withPending(s: LocalModelsSnapshot): LocalModelsSnapshot {
  const installed = (id: LocalRuntimeId) => s.runtimes.some((r) => r.id === id && r.installed)
  const runtimeSize = (id: LocalRuntimeId) => s.runtimes.find((r) => r.id === id)?.size ?? 0
  const models = s.models.map((m) => ({
    ...m,
    pending_runtime_bytes: MODEL_RUNTIMES[m.id]
      .filter((r) => !installed(r))
      .reduce((sum, r) => sum + runtimeSize(r), 0),
  }))
  const runtimes = s.runtimes.map((r) => ({
    ...r,
    in_use: models.some(
      (m) => m.state.state === 'installed' && MODEL_RUNTIMES[m.id].includes(r.id),
    ),
  }))
  const disk_used = models
    .filter((m) => m.state.state === 'installed')
    .reduce((sum, m) => sum + m.download_bytes, 0)
  return { ...s, models, runtimes, disk_used }
}

/** 设置快照里的摘要 */
export function offlineSummary(o: MockOffline): OfflineSummary {
  return {
    installed: o.snapshot.models.filter((m) => m.state.state === 'installed').map((m) => m.id),
  }
}

export function installOfflineDefaults(d: DefaultsDeps): void {
  const { bus, router: r, state } = d
  const offline = state.offline
  /** 每个模型当前的下载代次：暂停 / 取消之后，还在走的进度不再写回 */
  const generation = new Map<LocalModelId, number>()
  /** 「下载并使用」：装好后选用的模型（与 Rust 一致：暂停 / 失败重试保留，取消 / 删除清掉） */
  const selectAfter = new Set<LocalModelId>()
  /** 按模型类别写 `offline.*_model` 并广播设置快照 */
  const selectModel = (id: LocalModelId) => {
    const key = find(id).kind === 'ocr' ? 'ocr_model' : 'translate_model'
    state.settings = {
      ...state.settings,
      offline: { ...state.settings.offline, [key]: id },
    }
  }

  const emit = () => {
    offline.snapshot = withPending(offline.snapshot)
    bus.emit('offline_models:changed', offline.snapshot)
    return offline.snapshot
  }
  const update = (id: LocalModelId, patch: Partial<LocalModelInfo>) => {
    offline.snapshot = {
      ...offline.snapshot,
      models: offline.snapshot.models.map((m) => (m.id === id ? { ...m, ...patch } : m)),
    }
  }
  const find = (id: LocalModelId) => {
    const m = offline.snapshot.models.find((x) => x.id === id)
    if (!m) throw mockError('not_found', `mock: offline model ${id} not found`)
    return m
  }
  /** 安装 / 删除改变了 `RuntimeInfo.offline`：按当前（可能被场景覆写的）settings_get 广播一份设置快照 */
  const settingsChanged = async () => {
    const snap = (await r.dispatch('settings_get', {})) as SettingsSnapshot
    bus.emit('settings:changed', snap)
  }

  const finish = (id: LocalModelId) => {
    offline.snapshot = {
      ...offline.snapshot,
      runtimes: offline.snapshot.runtimes.map((rt) =>
        MODEL_RUNTIMES[id].includes(rt.id) ? { ...rt, installed: true } : rt,
      ),
    }
    update(id, { state: { state: 'installed' } })
    if (selectAfter.delete(id)) selectModel(id)
    emit()
    void settingsChanged()
  }

  r.handle('offline_models_get', () => withPending(offline.snapshot))
  r.handle('offline_model_download', ({ model: id, select }) => {
    const m = find(id)
    const st = m.state.state
    if (st === 'installed') {
      if (select) {
        selectModel(id)
        void settingsChanged()
      }
      return
    }
    if (select) selectAfter.add(id)
    if (st === 'downloading' || st === 'queued' || st === 'verifying') return
    const total = m.download_bytes + m.pending_runtime_bytes
    let done = m.state.state === 'paused' ? m.state.done : 0
    const gen = (generation.get(id) ?? 0) + 1
    generation.set(id, gen)
    const step = () => {
      if (generation.get(id) !== gen) return
      done = Math.min(total, done + Math.ceil(total / TICKS))
      if (done >= total) {
        update(id, { state: { state: 'verifying' } })
        emit()
        setTimeout(() => generation.get(id) === gen && finish(id), TICK_MS)
        return
      }
      update(id, {
        state: {
          state: 'downloading',
          done,
          total,
          bytes_per_sec: Math.round(total / TICKS / 0.25),
        },
      })
      emit()
      setTimeout(step, TICK_MS)
    }
    update(id, { state: { state: 'downloading', done, total, bytes_per_sec: 0 } })
    emit()
    setTimeout(step, TICK_MS)
  })
  r.handle('offline_model_pause', ({ model: id }) => {
    const m = find(id)
    if (m.state.state !== 'downloading' && m.state.state !== 'queued') return
    generation.set(id, (generation.get(id) ?? 0) + 1)
    const done = m.state.state === 'downloading' ? m.state.done : 0
    update(id, {
      state: { state: 'paused', done, total: m.download_bytes + m.pending_runtime_bytes },
    })
    emit()
  })
  r.handle('offline_model_cancel', ({ model: id }) => {
    find(id)
    selectAfter.delete(id)
    generation.set(id, (generation.get(id) ?? 0) + 1)
    update(id, { state: { state: 'not_installed' } })
    emit()
  })
  r.handle('offline_model_delete', ({ model: id }) => {
    find(id)
    selectAfter.delete(id)
    update(id, { state: { state: 'not_installed' }, load: 'unloaded', update_available: false })
    const s = state.settings.offline
    if (s.ocr_model === id || s.translate_model === id) {
      state.settings = {
        ...state.settings,
        offline: {
          ...s,
          ocr_model: s.ocr_model === id ? null : s.ocr_model,
          translate_model: s.translate_model === id ? null : s.translate_model,
        },
      }
    }
    emit()
    void settingsChanged()
  })
  r.handle('offline_runtime_delete', ({ runtime }) => {
    const inUse = withPending(offline.snapshot).runtimes.find((rt) => rt.id === runtime)?.in_use
    if (inUse)
      throw mockError('invalid_argument', 'mock: runtime is still used by an installed model')
    offline.snapshot = {
      ...offline.snapshot,
      runtimes: offline.snapshot.runtimes.map((rt) =>
        rt.id === runtime ? { ...rt, installed: false } : rt,
      ),
    }
    emit()
  })
  r.handle('offline_release', () => {
    offline.snapshot = {
      ...offline.snapshot,
      models: offline.snapshot.models.map((m) => ({ ...m, load: 'unloaded' as const })),
    }
    emit()
  })
  r.handle('offline_reveal', () => undefined)
  r.handle('offline_devices_refresh', () => {
    offline.snapshot = {
      ...offline.snapshot,
      device: { ...offline.snapshot.device, gpu_probed: true },
    }
    return emit()
  })
}
