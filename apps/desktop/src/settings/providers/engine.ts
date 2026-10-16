/**
 * 翻译 / OCR 用哪个模型：设置里「默认模型」「OCR 模型」下拉的纯函数（不依赖 React）。
 *
 * 下拉是分组的：「离线模型」（本机运行；未下载的也列出并注明大小）、文字识别服务（PaddleOCR，只有 OCR 才有）、各 LLM 服务。
 * 选中的值按来源写到不同地方：
 * - 离线模型 → 本机的 `offline.<task>_model`（不同步：每台设备算力不同，同步不能把这台设备从离线改回云端）；
 * - 其他 → 同步的 `defaults.<task>`，并清空本机的 `offline.<task>_model`（否则离线优先，选了也不生效）。
 * 下拉显示的当前值同样按这个优先级：本机选了离线模型就显示它，否则显示 `defaults`。
 */
import { LOCAL_MODELS, modelKind, offlineIdOf, offlineRef } from '../../shared/offline/catalog'
import type {
  LocalModelId,
  LocalModelInfo,
  ModelRef,
  Provider,
  Settings,
} from '../../shared/ipc/types'
import type { Patch } from '../lib/patch'
import { ocrOptions, parseRefKey, refKey, translateOptions, type ModelOption } from './model'

export type EngineTask = 'translate' | 'ocr'

/** 离线模型在下拉里的值（与一次性指定时的模型引用同一种写法：保留的服务商 id `@offline`） */
export function offlineValue(id: LocalModelId): string {
  return refKey(offlineRef(id)) as string
}

/** 下拉的值是不是离线模型（是则返回 id） */
export function offlineOfValue(value: string): LocalModelId | null {
  return offlineIdOf(parseRefKey(value))
}

/** 本机为这个任务选中的离线模型 */
export function selectedOffline(settings: Settings, task: EngineTask): LocalModelId | null {
  return task === 'ocr' ? settings.offline.ocr_model : settings.offline.translate_model
}

/** 同步的默认模型 */
export function defaultRef(settings: Settings, task: EngineTask): ModelRef | null {
  return task === 'ocr' ? settings.defaults.ocr : settings.defaults.translate
}

/** 下拉显示的当前值：本机的离线模型优先，其次是默认模型 */
export function currentValue(settings: Settings, task: EngineTask): string | null {
  const local = selectedOffline(settings, task)
  return local ? offlineValue(local) : refKey(defaultRef(settings, task))
}

/** 云端候选（LLM 服务 + OCR 的文字识别服务） */
export function cloudOptions(providers: readonly Provider[], task: EngineTask): ModelOption[] {
  return task === 'ocr' ? ocrOptions(providers) : translateOptions(providers)
}

/** 这个任务可以用的离线模型（按界面顺序）；快照里没有的（还没读到库存）也列出，大小未知 */
export function offlineCandidates(
  task: EngineTask,
  models: readonly LocalModelInfo[] | null,
): Array<{ id: LocalModelId; info: LocalModelInfo | null }> {
  return LOCAL_MODELS.filter((id) => modelKind(id) === task).map((id) => ({
    id,
    info: models?.find((m) => m.id === id) ?? null,
  }))
}

/**
 * 选中某个值时写入的补丁：离线 → `offline.<task>_model`；其他 → `defaults.<task>`，本机选过离线模型时一并清空。
 */
export function choicePatch(settings: Settings, task: EngineTask, value: string): Patch {
  const local = offlineOfValue(value)
  const key = task === 'ocr' ? 'ocr_model' : 'translate_model'
  if (local) return { offline: { [key]: local } }
  const patch: Patch = { defaults: { [task]: parseRefKey(value) } }
  if (selectedOffline(settings, task) !== null) patch.offline = { [key]: null }
  return patch
}

/**
 * 「模型步骤已完成」：翻译有可用的模型——本机选了离线翻译模型，或者有服务且设置了默认翻译模型。
 * 引导的步骤条、「接入模型」这一步与「试一试」用同一个判断
 */
export function translateReady(settings: Settings | null): boolean {
  if (!settings) return false
  if (settings.offline.translate_model !== null) return true
  return settings.providers.length > 0 && settings.defaults.translate !== null
}
