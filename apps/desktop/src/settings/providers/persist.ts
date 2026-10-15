/**
 * 服务的写入：按 id 只写一个服务，不再把整个 providers 数组发回去——设置窗口里的旧数组会把别处的改动（其他设备同步来的、
 * 手改配置文件的）写回旧值，甚至把别处刚删掉的服务加回来。
 *
 * - 编辑已有的服务：本地乐观替换这一项，停顿后 `provider_save`（同一个服务连续改动只发最后一次）。服务已在别处删除时
 *   Rust 返回 not_found：提示「已在其他设备上或配置文件里删除」，并回到服务器状态（编辑器随之回到列表），不把它加回来；
 * - 添加：`provider_add`（追加到末尾，其他服务不动）；
 * - 排序：`providers_reorder`（只重排、不改内容）。
 */
import { t } from '../../shared/i18n/i18n'
import { api, type WispIpcError } from '../../shared/ipc'
import type { Provider, Settings } from '../../shared/ipc/types'
import { ipcErrorText } from '../lib/errors'
import { notify, queueWrite, writeSettings, type LocalChange } from '../store'
import { providerName } from './model'

/** 某个服务在合并写入队列里的 key（删除前据此丢掉还没发出的改动） */
export function providerQueueKey(id: string): string {
  return `provider:${id}`
}

/** 本地按 id 替换（不存在时不加回来：可能刚在别处删掉） */
function replaceById(next: Provider): LocalChange {
  return (s: Settings) =>
    s.providers.some((p) => p.id === next.id)
      ? { ...s, providers: s.providers.map((p) => (p.id === next.id ? next : p)) }
      : s
}

/** 本地追加（同 id 已存在时替换，与 Rust 的 provider_add 一致） */
function appendOrReplace(next: Provider): LocalChange {
  return (s: Settings) =>
    s.providers.some((p) => p.id === next.id)
      ? { ...s, providers: s.providers.map((p) => (p.id === next.id ? next : p)) }
      : { ...s, providers: [...s.providers, next] }
}

/** 保存失败的提示：服务已在别处删除时说清楚原因，其余按通用文案 */
function saveError(provider: Provider) {
  return (error: WispIpcError) => {
    const text =
      error.code === 'not_found'
        ? t('settings.providers.deleted_elsewhere', { name: providerName(provider) })
        : t('settings.notice.save_failed', { reason: ipcErrorText(error, 'provider_save') })
    notify(text, 'danger')
  }
}

/** 立即保存一个已有的服务（`provider_save`）；返回是否成功 */
export function saveProvider(next: Provider): Promise<boolean> {
  return writeSettings(replaceById(next), () => api.providerSave(next), {
    command: 'provider_save',
    onError: saveError(next),
  })
}

/** 编辑已有的服务：立即乐观更新，停顿后保存（同一个服务只发最后一次） */
export function queueProviderSave(next: Provider): void {
  queueWrite(providerQueueKey(next.id), replaceById(next), () => void saveProvider(next))
}

/** 添加服务（`provider_add`）；返回是否成功（失败已提示） */
export function addProvider(next: Provider): Promise<boolean> {
  return writeSettings(appendOrReplace(next), () => api.providerAdd(next), {
    command: 'provider_add',
  })
}

/** 调整服务的顺序（`providers_reorder`）：本地按同一规则重排（未知 id 忽略，缺少的按原顺序排在后面） */
export function reorderProviders(ids: readonly string[]): Promise<boolean> {
  const reorder: LocalChange = (s) => {
    const rest = [...s.providers]
    const ordered = ids.flatMap((id) => {
      const i = rest.findIndex((p) => p.id === id)
      return i < 0 ? [] : rest.splice(i, 1)
    })
    return { ...s, providers: [...ordered, ...rest] }
  }
  return writeSettings(reorder, () => api.providersReorder([...ids]), {
    command: 'providers_reorder',
  })
}
