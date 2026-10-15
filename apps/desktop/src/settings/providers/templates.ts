/**
 * 模板目录（`provider_templates`，16 个，顺序由 Rust 给出）。窗口内只请求一次；失败时下次重新请求。
 */
import { api } from '../../shared/ipc'
import type { ProviderTemplate } from '../../shared/ipc/types'

let cache: Promise<ProviderTemplate[]> | null = null

export function loadTemplates(): Promise<ProviderTemplate[]> {
  cache ??= api.providerTemplates().catch((e: unknown) => {
    cache = null
    throw e
  })
  return cache
}

/** 测试用 */
export function resetTemplatesCache(): void {
  cache = null
}
