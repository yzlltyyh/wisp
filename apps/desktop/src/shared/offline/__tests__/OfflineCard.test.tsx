/**
 * 面板里的「离线模型还没下载 / 正在下载」卡片（翻译与 OCR 共用）：就地下载、进度（排队时静止）、失败的具体原因、
 * 只缺推理组件时按缺的下载、下载完成只通知一次（每一轮下载最多自动重试一次）、切换模型。
 */
import { act, fireEvent, render, screen, waitFor } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { getAppearance, setAppearance } from '../../hooks/useAppearance'
import { setTransport, type Transport } from '../../ipc'
import { LocalModelsSnapshot as Golden } from '../../ipc/fixtures'
import type { Fitness, LocalModelState, LocalModelsSnapshot } from '../../ipc/types'
import { MotionRoot } from '../../motion/MotionRoot'
import { OfflineCard } from '../OfflineCard'
import { resetOfflineModels } from '../store'

let calls: string[]
let args: unknown[]
let listeners: Set<(p: unknown) => void>
let snap: LocalModelsSnapshot
const saved = getAppearance()

const withState = (
  state: LocalModelState,
  pending = 0,
  fitness: Fitness = { level: 'good', reason_key: null },
): LocalModelsSnapshot => ({
  ...snap,
  models: snap.models.map((m) =>
    m.id === 'index_translate_2b' ? { ...m, state, pending_runtime_bytes: pending, fitness } : m,
  ),
})

beforeEach(() => {
  calls = []
  args = []
  listeners = new Set()
  snap = structuredClone(Golden) as LocalModelsSnapshot
  snap = withState({ state: 'not_installed' })
  const t: Transport = {
    kind: 'mock',
    invoke: <T,>(cmd: string, a?: unknown) => {
      calls.push(cmd)
      args.push(a)
      return Promise.resolve(
        (cmd === 'offline_models_get' ? structuredClone(snap) : undefined) as T,
      )
    },
    openStream: () => Promise.reject(new Error('unused')),
    listen: (_event, cb) => {
      listeners.add(cb as (p: unknown) => void)
      return Promise.resolve(() => listeners.delete(cb as (p: unknown) => void))
    },
    window: {} as Transport['window'],
    protocolUrl: (p) => p,
  }
  setTransport(t)
  resetOfflineModels()
  setAppearance({ ...saved, uiLanguage: 'zh-CN' })
})
afterEach(() => {
  resetOfflineModels()
  setAppearance(saved)
})

const emit = (s: LocalModelsSnapshot) =>
  act(() => {
    for (const cb of listeners) cb(s)
  })

function show(onReady = vi.fn<() => void>(), onSwitchModel = vi.fn<() => void>()) {
  render(
    <MotionRoot>
      <OfflineCard
        notReady={{ reason: 'offline_model_missing', model: 'index_translate_2b' }}
        onReady={onReady}
        onSwitchModel={onSwitchModel}
      />
    </MotionRoot>,
  )
  return { onReady, onSwitchModel }
}

describe('OfflineCard', () => {
  it('没下载：说明注明会自动继续，主按钮就地下载，次按钮切换模型', async () => {
    const { onSwitchModel } = show()
    const button = await screen.findByTestId('offline-card-download')
    expect(button).toHaveTextContent('下载 ·')
    expect(screen.getByTestId('offline-card')).toHaveTextContent('下载完成后会自动继续')
    fireEvent.click(button)
    await waitFor(() => expect(calls).toContain('offline_model_download'))
    // 就地下载不改选择（这次请求本来就指向这个模型）
    expect(args[calls.indexOf('offline_model_download')]).toEqual({
      model: 'index_translate_2b',
      select: false,
    })
    fireEvent.click(screen.getByTestId('offline-card-switch'))
    expect(onSwitchModel).toHaveBeenCalledOnce()
  })

  it('下载中：进度条与百分比；已暂停时按钮是「继续下载」', async () => {
    show()
    await screen.findByTestId('offline-card')
    emit(withState({ state: 'downloading', done: 30, total: 100, bytes_per_sec: 1 }))
    expect(await screen.findByTestId('offline-card-progress')).toHaveTextContent('30%')
    expect(screen.queryByTestId('offline-card-download')).toBeNull()
    emit(withState({ state: 'paused', done: 30, total: 100 }))
    expect(await screen.findByTestId('offline-card-download')).toHaveTextContent('继续下载')
  })

  it('排队中是静止的空进度条；校验中是不确定进度', async () => {
    show()
    await screen.findByTestId('offline-card')
    emit(withState({ state: 'queued' }))
    const queued = (await screen.findByTestId('offline-card-progress')).querySelector(
      '[role="progressbar"]',
    )
    expect(queued).not.toHaveAttribute('data-indeterminate')
    expect(queued).toHaveAttribute('aria-valuenow', '0')
    emit(withState({ state: 'verifying' }))
    expect(
      screen.getByTestId('offline-card-progress').querySelector('[role="progressbar"]'),
    ).toHaveAttribute('data-indeterminate', 'true')
  })

  it('下载失败：说明写出具体原因，主按钮是「重试」', async () => {
    show()
    await screen.findByTestId('offline-card')
    emit(withState({ state: 'failed', code: 'disk_full' }))
    expect(await screen.findByTestId('offline-card-download')).toHaveTextContent('重试')
    expect(screen.getByTestId('offline-card')).toHaveTextContent('磁盘空间不足')
  })

  it('模型文件在、推理组件缺：不当作已下载去重试，按钮只下载缺的推理组件', async () => {
    const { onReady } = show()
    await screen.findByTestId('offline-card')
    emit(withState({ state: 'installed' }, 31 * 1024 * 1024))
    expect(await screen.findByTestId('offline-card-download')).toHaveTextContent('下载 · 31 MB')
    expect(onReady).not.toHaveBeenCalled()
    emit(withState({ state: 'installed' }))
    await waitFor(() => expect(onReady).toHaveBeenCalledOnce())
  })

  it('不完全适合本机：点下载先在卡片里确认，说明原因（面板自己的文案）', async () => {
    snap = withState({ state: 'not_installed' }, 0, {
      level: 'unknown',
      reason_key: 'settings.offline.fitness.gpu_unknown',
    })
    show()
    fireEvent.click(await screen.findByTestId('offline-card-download'))
    const confirm = await screen.findByTestId('offline-card-confirm')
    expect(confirm).toHaveTextContent('显卡尚未检测')
    expect(calls).not.toContain('offline_model_download')
    expect(screen.queryByTestId('offline-card-download')).toBeNull()
    // 取消：回到下载按钮，什么都没发
    fireEvent.click(screen.getByRole('button', { name: '取消' }))
    fireEvent.click(await screen.findByTestId('offline-card-download'))
    fireEvent.click(await screen.findByRole('button', { name: '仍要下载' }))
    await waitFor(() => expect(calls).toContain('offline_model_download'))
  })

  it('检测后自动暂停（较慢）：继续之前先确认；确认过同一个结论后不再问', async () => {
    const slow: Fitness = { level: 'slow', reason_key: 'settings.offline.fitness.integrated_gpu' }
    snap = withState({ state: 'paused', done: 30, total: 100 }, 0, slow)
    show()
    fireEvent.click(await screen.findByTestId('offline-card-download'))
    expect(await screen.findByTestId('offline-card-confirm')).toHaveTextContent('只有集成显卡')
    fireEvent.click(screen.getByRole('button', { name: '仍要下载' }))
    await waitFor(() => expect(calls.filter((c) => c === 'offline_model_download')).toHaveLength(1))
    // 手动暂停之后再继续：同一个结论，不再确认
    emit(withState({ state: 'paused', done: 40, total: 100 }, 0, slow))
    fireEvent.click(await screen.findByTestId('offline-card-download'))
    await waitFor(() => expect(calls.filter((c) => c === 'offline_model_download')).toHaveLength(2))
    // 适合本机、只是暂停：直接继续
    emit(withState({ state: 'paused', done: 40, total: 100 }))
    fireEvent.click(await screen.findByTestId('offline-card-download'))
    await waitFor(() => expect(calls.filter((c) => c === 'offline_model_download')).toHaveLength(3))
  })

  it('下载完成只通知一次（之后的推送不再重复重试）', async () => {
    const { onReady } = show()
    await screen.findByTestId('offline-card')
    emit(withState({ state: 'installed' }))
    emit(withState({ state: 'installed' }))
    await waitFor(() => expect(onReady).toHaveBeenCalledOnce())
    expect(screen.getByTestId('offline-card')).toHaveTextContent('Index-Translate 2B 已下载')
  })
})
