/**
 * 服务编辑器的两个异步动作：测试连接（`provider_test`）与拉取模型列表（`provider_models`，Rust 负责与已有列表合并：
 * 保留用户设置、首次拉取 ≤ 20 个全部启用、之后的新模型默认不启用）。
 * 结果按「发起时的 Provider id」归档：换了 Provider 或组件卸载后迟到的结果不会写到别人身上。
 *
 * 测试结论只在本次窗口会话里记住（列表的状态徽标与编辑器共用），并带着发起时的「连接配置指纹」：之后改了 base_url、
 * 协议、请求头、quirks 或 Key，结论就标为过期（`stale`）——列表显示「需重新测试」，编辑器把旧结果变淡并提示重新测试。
 */
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  useSyncExternalStore,
} from 'react'
import { api } from '../../shared/ipc'
import type { ModelEntry, Provider } from '../../shared/ipc/types'
import { ipcErrorText } from '../lib/errors'
import { initialModels } from './model'
import type { TestRun } from './TestReport'

interface Recorded {
  run: TestRun
  /** 发起测试时的连接配置指纹 */
  fingerprint: string
}

/** 本次窗口会话里每个 Provider 最近一次的测试结果（不持久化） */
const lastRuns = new Map<string, Recorded>()
const runListeners = new Set<() => void>()
let runsVersion = 0
const IDLE: TestRun = { phase: 'idle' }

/** 每个 Provider 的 Key 改动次数（写入 / 删除后加一）：Key 换了，旧的测试结论也不再算数 */
const keyVersions = new Map<string, number>()

function notifyRuns(): void {
  runsVersion += 1
  for (const l of runListeners) l()
}

/** Key 写入或删除之后调用 */
export function bumpKeyVersion(providerId: string): void {
  keyVersions.set(providerId, (keyVersions.get(providerId) ?? 0) + 1)
  notifyRuns()
}

/** 影响连接结果的字段（名称、模型开关等不算） */
export function connectionFingerprint(p: Provider): string {
  return JSON.stringify([
    p.protocol,
    p.base_url.trim(),
    p.headers,
    p.quirks,
    keyVersions.get(p.id) ?? 0,
  ])
}

export interface TestView {
  run: TestRun
  /** 结论是在另一套连接配置下得出的 */
  stale: boolean
}

/** 某个 Provider 最近一次的测试结果与它是否已过期 */
export function lastTestRun(p: Provider): TestView {
  const rec = lastRuns.get(p.id)
  if (!rec) return { run: IDLE, stale: false }
  return {
    run: rec.run,
    stale: rec.run.phase !== 'running' && rec.fingerprint !== connectionFingerprint(p),
  }
}

/**
 * 最近一次测试连接里流式探针**成功**用到的模型（结论没有过期时）：新服务的默认翻译模型优先用它
 * （/models 的第一项常是嵌入模型，探针已经跳过了它们，见 crates/wisp-llm/src/probe.rs）
 */
export function probedModel(p: Provider): string | null {
  const { run, stale } = lastTestRun(p)
  if (stale || run.phase !== 'done' || !run.report.stream.ok) return null
  return run.report.model_used
}

export function subscribeTestRuns(listener: () => void): () => void {
  runListeners.add(listener)
  return () => runListeners.delete(listener)
}

/** 测试结果的版本号（列表用它在结果变化时重渲染） */
export function useTestRunsVersion(): number {
  return useSyncExternalStore(
    subscribeTestRuns,
    () => runsVersion,
    () => runsVersion,
  )
}

function record(p: Provider, run: TestRun, fingerprint: string) {
  lastRuns.set(p.id, { run, fingerprint })
  notifyRuns()
}

/** 测试用：清空会话里的结论 */
export function resetTestRuns(): void {
  lastRuns.clear()
  keyVersions.clear()
  notifyRuns()
}

/** 测试结果直接读共享表：切换 Provider 时自然显示那个 Provider 的结果 */
export function useProviderTest(provider: Provider) {
  useTestRunsVersion()
  const view = lastTestRun(provider)
  const latest = useRef(provider)
  useLayoutEffect(() => {
    latest.current = provider
  })

  /** `key`：草稿里还没保存的 Key（只用于这一次请求，不经钥匙串）；null = 已保存的服务 */
  const start = useCallback(async (key: string | null = null): Promise<TestRun> => {
    const p = latest.current
    // 指纹按发起时的配置记：测试进行中又改了 URL，结论一出来就是过期的
    const fingerprint = connectionFingerprint(p)
    record(p, { phase: 'running' }, fingerprint)
    let next: TestRun
    try {
      next = { phase: 'done', report: await api.providerTest(p, key) }
    } catch (error) {
      next = { phase: 'error', message: ipcErrorText(error, 'provider_test') }
    }
    record(p, next, fingerprint)
    return next
  }, [])

  return { run: view.run, stale: view.stale, start }
}

export function useModelFetch(provider: Provider, onModels: (models: ModelEntry[]) => void) {
  const [state, setState] = useState<{ id: string; loading: boolean; error: string | null }>({
    id: provider.id,
    loading: false,
    error: null,
  })
  // 换了 Provider：在渲染中清掉上一个的状态
  if (state.id !== provider.id) setState({ id: provider.id, loading: false, error: null })
  const latest = useRef({ provider, onModels })
  useLayoutEffect(() => {
    latest.current = { provider, onModels }
  })
  const alive = useRef(true)
  useEffect(() => {
    alive.current = true
    return () => {
      alive.current = false
    }
  }, [])

  /** 采用一份已经合并好的模型列表（拉取的结果，或测试连接顺带返回的 `fetched_models`） */
  const apply = useCallback((models: ModelEntry[]) => {
    const p = latest.current.provider
    // 第一次拿到列表：非对话模型默认不启用（model.ts initialModels）
    latest.current.onModels(initialModels(p.models, models))
    setState({ id: p.id, loading: false, error: null })
  }, [])

  /** `key` 的含义同 useProviderTest 的 start */
  const fetchModels = useCallback(
    async (key: string | null = null): Promise<boolean> => {
      const p = latest.current.provider
      setState({ id: p.id, loading: true, error: null })
      const same = () => alive.current && latest.current.provider.id === p.id
      try {
        const models = await api.providerModels(p, key)
        if (same()) apply(models)
        return true
      } catch (e) {
        if (same())
          setState({ id: p.id, loading: false, error: ipcErrorText(e, 'provider_models') })
        return false
      }
    },
    [apply],
  )

  return { loading: state.loading, error: state.error, fetchModels, apply }
}
