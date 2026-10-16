/**
 * 离线模型分组的纯函数：每张模型卡显示哪种状态、适配标记的语气、速度与剩余时间的写法、「立即释放内存」的说明。
 * 不依赖 React，便于单测。
 */
import type { Tone } from '../../shared/ui'
import { formatBytes } from '../../shared/i18n/format'
import { t } from '../../shared/i18n/i18n'
import type {
  FitnessLevel,
  LocalModelInfo,
  LocalModelsSnapshot,
  LocalModelState,
  LocalRuntimeId,
} from '../../shared/ipc/types'

/** 模型卡的状态种类（状态行只在种类变化时重播进入动效；数字变化不重播） */
export type RowKind = LocalModelState['state']

export function rowKind(m: Pick<LocalModelInfo, 'state'>): RowKind {
  return m.state.state
}

/** 适配标记的语气：适合 = 成功色；较慢 = 警示；内存不足 = 危险；待检测 = 中性 */
export const FIT_TONE: Record<FitnessLevel, Tone> = {
  good: 'success',
  slow: 'warning',
  insufficient: 'danger',
  unknown: 'neutral',
}

const MINUTE = 60
const HOUR = 60 * MINUTE

/** 剩余时间：不到一分钟按秒（取整到 5 秒，避免数字每次刷新都跳），不到一小时按分钟，否则「1 小时 5 分」 */
export function formatEta(seconds: number): string {
  const rounded = Math.max(5, Math.ceil(seconds / 5) * 5)
  if (rounded < MINUTE) return t('settings.offline.eta_seconds', { n: rounded })
  if (seconds < HOUR) return t('settings.offline.eta_minutes', { n: Math.ceil(seconds / MINUTE) })
  let h = Math.floor(seconds / HOUR)
  let m = Math.ceil((seconds - h * HOUR) / MINUTE)
  if (m === MINUTE) {
    h += 1
    m = 0
  }
  return t('settings.offline.eta_hours', { h, m })
}

/** 速度：「12 MB/s」 */
export function formatSpeed(bytesPerSec: number): string {
  return t('settings.offline.speed', { speed: formatBytes(Math.max(0, bytesPerSec)) })
}

/** 正在占用内存的模型（加载中的也算：它马上就要占用） */
export function loadedModels(snap: LocalModelsSnapshot | null): LocalModelInfo[] {
  return snap?.models.filter((m) => m.load !== 'unloaded') ?? []
}

/** 已加载模型的估计内存合计（字节） */
export function loadedMemory(models: readonly LocalModelInfo[]): number {
  return models.reduce((sum, m) => sum + m.memory_estimate_mb * 1024 * 1024, 0)
}

/** 某个推理组件的显示名 */
export function runtimeName(id: LocalRuntimeId): string {
  return t(`settings.offline.runtime.${id}`)
}
