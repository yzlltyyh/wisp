/**
 * 「接入模型」这一步的草稿状态，放在引导窗口（OnboardingWindow）里而不是这一步自己的组件里：
 * 点底栏的「上一步」「跳过这一步」或步骤条离开这一步，再回来时，已经填的 Key、区域、测试结果、选好的默认模型都还在
 * （测试结果本来就按 Provider id 记在 providers/hooks.ts 的会话表里）。
 *
 * 为测试写进凭据存储、最后没保存的 Key：换了别的模板时删掉；离开窗口（完成 / 稍后设置 / ⌘W / 重启）时由 writes.ts 的
 * 「关窗前放弃」删掉（useKeyEntry 草稿期间登记）。保存成功后整个草稿清空，回来时显示已接入的摘要。
 */
import { useCallback, useState } from 'react'
import type { ModelRef, Provider, ProviderTemplate } from '../../shared/ipc/types'
import { useKeyEntry, type KeyEntry } from '../../settings/providers/keyEntry'
import { useRuntime } from '../../settings/store'

export type ModelPhase = 'summary' | 'choose' | 'others' | 'form'

export interface ModelDefaults {
  translate: ModelRef | null
  ocr: ModelRef | null
}

export interface ModelDraftState {
  /** 当前阶段；null = 按是否已经接入过服务决定（摘要 / 卡片） */
  phase: ModelPhase | null
  draft: { provider: Provider; template: ProviderTemplate } | null
  defaults: ModelDefaults
  /** 最近一次成功拉取模型列表时的连接（协议 + base_url）：没变就不再拉取，免得覆盖用户的选择 */
  fetchedFor: string | null
}

export interface ModelDraftStore {
  state: ModelDraftState
  update: (patch: Partial<ModelDraftState>) => void
  /** 保存成功：清空草稿（Key 已转为已保存） */
  reset: () => void
  key: KeyEntry
}

const INITIAL: ModelDraftState = {
  phase: null,
  draft: null,
  defaults: { translate: null, ocr: null },
  fetchedFor: null,
}

/** 拉取模型列表的连接指纹（协议 + base_url） */
export function fetchFingerprint(p: Pick<Provider, 'protocol' | 'base_url'>): string {
  return `${p.protocol}\u0000${p.base_url.trim()}`
}

export function useModelDraft(): ModelDraftStore {
  const runtime = useRuntime()
  const [state, setState] = useState<ModelDraftState>(INITIAL)
  const id = state.draft?.provider.id ?? ''
  const key = useKeyEntry({ providerId: id, hasKey: runtime?.has_key[id] ?? false, draft: true })
  const update = useCallback(
    (patch: Partial<ModelDraftState>) => setState((s) => ({ ...s, ...patch })),
    [],
  )
  const reset = useCallback(() => setState(INITIAL), [])
  return { state, update, reset, key }
}
