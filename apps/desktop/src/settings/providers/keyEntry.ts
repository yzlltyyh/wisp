/**
 * API Key 的输入状态（只写不读，Key 只进系统凭据存储 `wisp/<provider_id>`）。
 *
 * - 已保存的服务：输入后回车或点「保存」写入；「测试连接」「拉取模型」之前输入框里还有没保存的 Key 时先写入
 *   （`commit()`，失败就地报错并中止），用户不用先记得按回车；
 * - 新建的草稿：Key 和其他字段一样只是草稿的一部分，**不**在输入时写入凭据存储——「添加服务 / 保存并继续」时才写。
 *   测试连接与拉取模型把草稿里的 Key 直接交给 Rust（`provider_test / provider_models(…, key)`，只用于这一次请求，
 *   不读也不写钥匙串）：草稿被放弃时凭据存储里什么都没留下，不需要「关窗前删除」的兜底。
 */
import { useCallback, useLayoutEffect, useRef, useState } from 'react'
import { api } from '../../shared/ipc'
import { ipcErrorText } from '../lib/errors'
import { trackWrite } from '../lib/writes'
import { setHasKey } from '../store'
import { bumpKeyVersion } from './hooks'

export interface KeyEntry {
  providerId: string
  /** 新建草稿（Key 跟着草稿一起提交） */
  draft: boolean
  /** 输入框里的内容（还没写进凭据存储的 Key） */
  value: string
  setValue: (v: string) => void
  /** 已保存的服务：是否显示输入框（没有 Key、或点了「更换」） */
  editing: boolean
  setEditing: (v: boolean) => void
  busy: boolean
  error: string | null
  /** 刚刚保存成功（显示对勾） */
  justSaved: boolean
  /** 已经有可用的 Key：已保存，或草稿里已经填了 */
  present: boolean
  /** 有没提交的输入（放弃草稿前需要确认） */
  dirty: boolean
  /** 把输入框里的 Key 写进凭据存储（没有输入时什么都不做）；失败时就地显示错误并返回 false */
  commit: () => Promise<boolean>
  /**
   * 测试连接 / 拉取模型时交给 Rust 的 Key：草稿 = 输入框里的 Key（去掉首尾空白，可以为空 = 以无 Key 探测）；
   * 已保存的服务 = null（Rust 从钥匙串读）
   */
  probeKey: () => string | null
  /** 删除已保存的 Key */
  remove: () => Promise<void>
  /** 草稿已经添加到设置（Key 已写入）：之后按已保存的服务显示 */
  markAdded: () => void
}

export function useKeyEntry({
  providerId,
  hasKey,
  draft,
}: {
  providerId: string
  /** Rust 的 `runtime.has_key[providerId]` */
  hasKey: boolean
  draft: boolean
}): KeyEntry {
  const [value, setValueState] = useState('')
  const [editing, setEditing] = useState(draft || !hasKey)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [justSaved, setJustSaved] = useState(false)
  /**
   * 已经添加到设置的草稿 id（按 id 记：引导里保存了一个服务之后再添加下一个，新草稿不能沿用「已添加」）。
   * 不是草稿（已保存的服务）时恒为已添加
   */
  const [addedId, setAddedId] = useState<string | null>(null)
  const addedRef = useRef<string | null>(null)
  const latest = useRef({ providerId, value, draft })
  useLayoutEffect(() => {
    latest.current = { providerId, value, draft }
  })

  // 换了一个 Provider（引导里重新选了模板）：输入框回到空白
  const [seenId, setSeenId] = useState(providerId)
  if (seenId !== providerId) {
    setSeenId(providerId)
    setValueState('')
    setError(null)
    setJustSaved(false)
    setEditing(draft || !hasKey)
  }

  // 已保存的服务：外部写入 / 删除了 Key（其他窗口、删除后重建）时回到对应状态；正在输入时不打断
  const [seenHasKey, setSeenHasKey] = useState(hasKey)
  if (seenHasKey !== hasKey) {
    setSeenHasKey(hasKey)
    if (!draft && !value) {
      setEditing(!hasKey)
      setError(null)
    }
  }

  const setValue = useCallback((v: string) => {
    const { providerId: id, value: prev, draft: isDraft } = latest.current
    setValueState(v)
    setError(null)
    // 草稿的 Key 换了：之前用它测出的结论标为过期（已保存的服务在写入时才算换 Key）
    if (isDraft && addedRef.current !== id && v.trim() !== prev.trim()) bumpKeyVersion(id)
  }, [])

  const commit = useCallback(async (): Promise<boolean> => {
    const { providerId: id, value: raw, draft: isDraft } = latest.current
    const key = raw.trim()
    if (!key) return true
    const asDraft = isDraft && addedRef.current !== id
    setBusy(true)
    setError(null)
    try {
      await trackWrite(api.secretSet(id, key))
      setHasKey(id, true)
      // 草稿的 Key 就是测试时用的那一个：写入不让结论过期
      if (!asDraft) bumpKeyVersion(id)
      if (!asDraft) {
        setValueState('')
        setEditing(false)
        setJustSaved(true)
      }
      return true
    } catch (e) {
      setError(ipcErrorText(e, 'secret_set'))
      return false
    } finally {
      setBusy(false)
    }
  }, [])

  const probeKey = useCallback((): string | null => {
    const { providerId: id, value: raw, draft: isDraft } = latest.current
    return isDraft && addedRef.current !== id ? raw.trim() : null
  }, [])

  const remove = useCallback(async () => {
    const id = latest.current.providerId
    setBusy(true)
    setError(null)
    try {
      await trackWrite(api.secretDelete(id))
      setHasKey(id, false)
      bumpKeyVersion(id)
      setEditing(true)
      setJustSaved(false)
    } catch (e) {
      setError(ipcErrorText(e, 'secret_delete'))
    } finally {
      setBusy(false)
    }
  }, [])

  const markAdded = useCallback(() => {
    const id = latest.current.providerId
    addedRef.current = id
    setAddedId(id)
    if (latest.current.value.trim()) {
      setValueState('')
      setEditing(false)
      setJustSaved(true)
    }
  }, [])

  const asDraft = draft && addedId !== providerId
  return {
    providerId,
    draft: asDraft,
    value,
    setValue,
    editing: asDraft ? true : editing,
    setEditing,
    busy,
    error,
    justSaved,
    present: asDraft ? value.trim() !== '' : hasKey,
    dirty: value.trim() !== '',
    commit,
    probeKey,
    remove,
    markAdded,
  }
}
