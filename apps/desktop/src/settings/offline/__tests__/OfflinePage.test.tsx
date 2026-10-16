/**
 * 设置 → 离线模型：各种状态的模型卡（未下载 / 排队 / 下载中 / 已暂停 / 校验中 / 已下载 / 损坏 / 失败）、操作发出的命令、
 * 内存不足 / 显卡待检测时下载先原地确认、检测后自动暂停的下载继续前先确认、删除原地确认、选用写本机的 offline.*、随
 * `offline_models:changed` 更新；以及「默认模型」下拉里选了未下载的离线模型时就地确认「下载并使用」（装好后才切换）。
 */
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { getAppearance, setAppearance } from '../../../shared/hooks/useAppearance'
import { boot } from '../../../shared/ipc'
import { LocalModelsSnapshot as Golden } from '../../../shared/ipc/fixtures'
import type {
  LocalModelId,
  LocalModelInfo,
  LocalModelsSnapshot,
  LocalModelState,
} from '../../../shared/ipc/types'
import { installHarness, snapshot, type Harness } from '../../__tests__/harness'
import { resetWrites } from '../../lib/writes'
import { SettingsApp } from '../../SettingsApp'
import { resetSettingsStore, setQueueDelay } from '../../store'

const appearance = getAppearance()
const MB = 1_000_000

function models(
  patch: Partial<Record<LocalModelId, Partial<LocalModelInfo>>> = {},
): LocalModelsSnapshot {
  const s = structuredClone(Golden) as LocalModelsSnapshot
  return {
    ...s,
    models: s.models.map((m) => ({
      ...m,
      state: { state: 'not_installed' },
      load: 'unloaded',
      fitness: { level: 'good', reason_key: null },
      ...patch[m.id],
    })),
  }
}

function open(snap: LocalModelsSnapshot, settings = snapshot()): Harness {
  const h = installHarness(settings)
  h.handle('offline_models_get', () => structuredClone(snap))
  boot.route = { kind: 'settings', section: 'offline' }
  render(<SettingsApp />)
  return h
}

const row = (id: LocalModelId) => screen.findByTestId(`offline-model-${id}`)

beforeEach(() => {
  resetSettingsStore()
  resetWrites()
  setQueueDelay(0)
  setAppearance({ ...appearance, os: 'macos', uiLanguage: 'zh-CN' })
})

afterEach(() => {
  boot.route = null
  setAppearance(appearance)
})

describe('模型卡的状态与操作', () => {
  it('未下载：「下载 · 大小」，另需推理组件时注明；点下载发出 offline_model_download', async () => {
    const h = open(
      models({ pp_ocrv6_small: { download_bytes: 31 * MB, pending_runtime_bytes: 33 * MB } }),
    )
    const card = await row('pp_ocrv6_small')
    expect(within(card).getByTestId('offline-model-pp_ocrv6_small-download')).toHaveTextContent(
      '下载 · 30 MB',
    )
    expect(card).toHaveTextContent('另需推理组件 31 MB')
    expect(within(card).getByTestId('offline-model-pp_ocrv6_small-fit')).toHaveTextContent(
      '适合本机',
    )
    fireEvent.click(within(card).getByTestId('offline-model-pp_ocrv6_small-download'))
    await waitFor(() =>
      expect(h.callsOf('offline_model_download')).toEqual([
        { model: 'pp_ocrv6_small', select: false },
      ]),
    )
  })

  it('下载中：进度条 + 百分比 + 速度 + 剩余时间；暂停 / 取消发出对应命令；推送的快照即时更新', async () => {
    const downloading: LocalModelState = {
      state: 'downloading',
      done: 420 * MB,
      total: 1000 * MB,
      bytes_per_sec: 10 * MB,
    }
    const h = open(models({ index_translate_2b: { state: downloading } }))
    const card = await row('index_translate_2b')
    expect(card).toHaveAttribute('data-state', 'downloading')
    expect(card).toHaveTextContent('42%')
    expect(card).toHaveTextContent('/s')
    expect(card).toHaveTextContent('剩余 1 分钟')
    expect(within(card).getByRole('progressbar')).toHaveAttribute('aria-valuenow', '42')
    fireEvent.click(within(card).getByTestId('offline-model-index_translate_2b-pause'))
    await waitFor(() =>
      expect(h.callsOf('offline_model_pause')).toEqual([{ model: 'index_translate_2b' }]),
    )
    fireEvent.click(within(card).getByTestId('offline-model-index_translate_2b-cancel'))
    await waitFor(() => expect(h.callsOf('offline_model_cancel')).toHaveLength(1))
    // Rust 推送：暂停了
    act(() =>
      h.emit(
        'offline_models:changed',
        models({
          index_translate_2b: { state: { state: 'paused', done: 420 * MB, total: 1000 * MB } },
        }),
      ),
    )
    expect(await row('index_translate_2b')).toHaveAttribute('data-state', 'paused')
    // 继续与暂停是同一位置、同一形式的图标按钮（名称在悬停说明里）
    expect(screen.getByTestId('offline-model-index_translate_2b-resume')).toHaveAccessibleName(
      '继续下载',
    )
    expect(screen.getByTestId('offline-model-index_translate_2b-progress')).toHaveAttribute(
      'data-paused',
      'true',
    )
  })

  it('排队中是静止的空进度条、校验中是不确定进度；失败说明原因并可以重试；损坏可以重新下载', async () => {
    open(
      models({
        pp_ocrv6_tiny: { state: { state: 'queued' } },
        pp_ocrv6_small: { state: { state: 'verifying' } },
        paddleocr_vl_1_6: { state: { state: 'failed', code: 'disk_full' } },
        index_translate_9b: { state: { state: 'broken' } },
      }),
    )
    const queued = await screen.findByTestId('offline-model-pp_ocrv6_tiny-progress')
    expect(queued).not.toHaveAttribute('data-indeterminate')
    expect(queued).toHaveAttribute('aria-valuenow', '0')
    expect(screen.getByTestId('offline-model-pp_ocrv6_tiny')).toHaveTextContent('排队中')
    expect(screen.getByTestId('offline-model-pp_ocrv6_small-progress')).toHaveAttribute(
      'data-indeterminate',
      'true',
    )
    expect(screen.getByTestId('offline-model-paddleocr_vl_1_6')).toHaveTextContent('磁盘空间不足')
    expect(screen.getByTestId('offline-model-paddleocr_vl_1_6-retry')).toBeInTheDocument()
    expect(screen.getByTestId('offline-model-index_translate_9b-redownload')).toBeInTheDocument()
  })

  it('已下载：「用于识别」写本机的 offline.ocr_model，再点一次改回默认；运行中显示内存占用与小圆点', async () => {
    const h = open(
      models({
        pp_ocrv6_small: { state: { state: 'installed' }, load: 'loaded', memory_estimate_mb: 400 },
        index_translate_2b: { state: { state: 'installed' }, load: 'loading' },
      }),
    )
    const card = await row('pp_ocrv6_small')
    expect(card).toHaveTextContent('已加载 · 约占用 400 MB 内存')
    expect(within(card).getByTestId('offline-model-pp_ocrv6_small-dot')).toHaveAttribute(
      'data-load',
      'loaded',
    )
    const loading = screen.getByTestId('offline-model-index_translate_2b-dot')
    expect(loading).toHaveAttribute('data-load', 'loading')
    // llama.cpp 的模型第一次加载要编译显卡着色器（下载完成后的预热也是这一步）：悬停说明为什么慢
    expect(loading).toHaveAttribute('title', expect.stringContaining('编译显卡着色器'))
    const use = within(card).getByTestId('offline-model-pp_ocrv6_small-use')
    expect(use).toHaveAttribute('aria-pressed', 'false')
    fireEvent.click(use)
    await waitFor(() =>
      expect(h.callsOf('settings_patch').at(-1)).toEqual({
        patch: { offline: { ocr_model: 'pp_ocrv6_small' } },
      }),
    )
    await waitFor(() =>
      expect(screen.getByTestId('offline-model-pp_ocrv6_small-use')).toHaveAttribute(
        'aria-pressed',
        'true',
      ),
    )
    fireEvent.click(screen.getByTestId('offline-model-pp_ocrv6_small-use'))
    await waitFor(() =>
      expect(h.callsOf('settings_patch').at(-1)).toEqual({
        patch: { offline: { ocr_model: null } },
      }),
    )
    // 有模型占着内存：「立即释放内存」可以点
    expect(screen.getByTestId('offline-release-button')).toBeEnabled()
    fireEvent.click(screen.getByTestId('offline-release-button'))
    await waitFor(() => expect(h.callsOf('offline_release')).toHaveLength(1))
  })

  it('内存不足时点下载先原地确认（说明原因），确认后才下载', async () => {
    const h = open(
      models({
        index_translate_9b: {
          fitness: { level: 'insufficient', reason_key: 'settings.offline.fitness.low_memory' },
        },
      }),
    )
    const card = await row('index_translate_9b')
    expect(within(card).getByTestId('offline-model-index_translate_9b-fit')).toHaveTextContent(
      '内存不足',
    )
    fireEvent.click(within(card).getByTestId('offline-model-index_translate_9b-download'))
    const confirm = await screen.findByTestId('offline-model-index_translate_9b-confirm')
    expect(confirm).toHaveTextContent('这台设备的内存偏少')
    expect(h.callsOf('offline_model_download')).toHaveLength(0)
    fireEvent.click(screen.getByTestId('offline-model-index_translate_9b-confirm-ok'))
    await waitFor(() =>
      expect(h.callsOf('offline_model_download')).toEqual([
        { model: 'index_translate_9b', select: false },
      ]),
    )
  })

  it('显卡还没检测时点下载同样先确认，并说明没有独立显卡会很慢', async () => {
    const h = open(
      models({
        index_translate_9b: {
          fitness: { level: 'unknown', reason_key: 'settings.offline.fitness.gpu_unknown' },
        },
      }),
    )
    fireEvent.click(await screen.findByTestId('offline-model-index_translate_9b-download'))
    const confirm = await screen.findByTestId('offline-model-index_translate_9b-confirm')
    expect(confirm).toHaveTextContent('显卡尚未检测，没有独立显卡时会很慢')
    expect(h.callsOf('offline_model_download')).toHaveLength(0)
    fireEvent.click(screen.getByTestId('offline-model-index_translate_9b-confirm-ok'))
    await waitFor(() => expect(h.callsOf('offline_model_download')).toHaveLength(1))
  })

  it('检测后自动暂停（较慢）：已暂停的这一行写出检测结论，点「继续」先确认；同一结论确认过后不再重复问', async () => {
    const slow = { level: 'slow', reason_key: 'settings.offline.fitness.integrated_gpu' } as const
    const h = open(
      models({
        index_translate_9b: {
          fitness: slow,
          state: { state: 'paused', done: 30 * MB, total: 5800 * MB },
        },
      }),
    )
    const card = await row('index_translate_9b')
    expect(card).toHaveTextContent('只有集成显卡，和在 CPU 上运行一样较慢')
    fireEvent.click(within(card).getByTestId('offline-model-index_translate_9b-resume'))
    const confirm = await screen.findByTestId('offline-model-index_translate_9b-confirm')
    expect(confirm).toHaveTextContent('仍要下载 Index-Translate 9B')
    expect(h.callsOf('offline_model_download')).toHaveLength(0)
    fireEvent.click(screen.getByTestId('offline-model-index_translate_9b-confirm-ok'))
    await waitFor(() => expect(h.callsOf('offline_model_download')).toHaveLength(1))
    // 用户手动暂停后再继续：刚确认过同一个结论，直接继续
    act(() =>
      h.emit(
        'offline_models:changed',
        models({
          index_translate_9b: {
            fitness: slow,
            state: { state: 'paused', done: 900 * MB, total: 5800 * MB },
          },
        }),
      ),
    )
    fireEvent.click(await screen.findByTestId('offline-model-index_translate_9b-resume'))
    await waitFor(() => expect(h.callsOf('offline_model_download')).toHaveLength(2))
    expect(screen.queryByTestId('offline-model-index_translate_9b-confirm')).toBeNull()
  })

  it('删除：⋯ 菜单 → 原地确认 → offline_model_delete', async () => {
    const h = open(models({ pp_ocrv6_tiny: { state: { state: 'installed' } } }))
    fireEvent.click(await screen.findByTestId('offline-model-pp_ocrv6_tiny-more'))
    const menu = await screen.findByTestId('offline-model-pp_ocrv6_tiny-menu')
    fireEvent.click(within(menu).getByText('删除…'))
    const confirm = await screen.findByTestId('offline-model-pp_ocrv6_tiny-confirm')
    expect(confirm).toHaveTextContent('删除 PP-OCRv6 Tiny？')
    fireEvent.click(screen.getByTestId('offline-model-pp_ocrv6_tiny-confirm-ok'))
    await waitFor(() =>
      expect(h.callsOf('offline_model_delete')).toEqual([{ model: 'pp_ocrv6_tiny' }]),
    )
  })

  it('本机与推理组件：内存与芯片、占用空间、打开文件夹；被依赖的组件不能删', async () => {
    const snap = models()
    snap.runtimes = snap.runtimes.map((r) => ({
      ...r,
      installed: true,
      in_use: r.id === 'onnx_runtime',
    }))
    const h = open(snap)
    expect(await screen.findByTestId('offline-device-info')).toBeInTheDocument()
    fireEvent.click(screen.getByTestId('offline-open-folder'))
    await waitFor(() => expect(h.callsOf('offline_reveal')).toHaveLength(1))
    expect(screen.getByTestId('offline-runtime-onnx_runtime-delete')).toBeDisabled()
    expect(screen.getByTestId('offline-runtime-onnx_runtime')).toHaveTextContent('不能单独删除')
    fireEvent.click(screen.getByTestId('offline-runtime-llama_cpp-delete'))
    await waitFor(() =>
      expect(h.callsOf('offline_runtime_delete')).toEqual([{ runtime: 'llama_cpp' }]),
    )
  })
})

describe('默认模型下拉里的离线模型', () => {
  it('选了未下载的离线翻译模型：就地确认「下载并使用」，确认后只开始下载（装好后由 Rust 切换），下拉保持原来的模型', async () => {
    const h = installHarness()
    h.handle('offline_models_get', () => models())
    boot.route = { kind: 'settings', section: 'providers' }
    render(<SettingsApp />)
    fireEvent.click(await screen.findByTestId('defaults-translate-select'))
    const menu = await screen.findByTestId('defaults-translate-select-menu')
    expect(within(menu).getByText('离线模型')).toBeInTheDocument()
    fireEvent.click(within(menu).getByText('Index-Translate 2B'))
    const confirm = await screen.findByTestId('defaults-translate-confirm')
    expect(confirm).toHaveTextContent('下载完成后自动切换')
    fireEvent.click(within(confirm).getByText('下载并使用'))
    await waitFor(() =>
      expect(h.callsOf('offline_model_download')).toEqual([
        { model: 'index_translate_2b', select: true },
      ]),
    )
    expect(await screen.findByTestId('settings-notice')).toHaveTextContent(
      'Index-Translate 2B 下载完成后会自动切换',
    )
    expect(h.callsOf('settings_patch')).toHaveLength(0)
    expect(screen.getByTestId('defaults-translate')).not.toHaveTextContent('仅在这台设备上生效')
  })

  it('选了正在下载的离线模型：不弹确认，只登记装好后切换', async () => {
    const h = installHarness()
    h.handle('offline_models_get', () =>
      models({
        index_translate_9b: {
          state: { state: 'downloading', done: 10 * MB, total: 100 * MB, bytes_per_sec: MB },
        },
      }),
    )
    boot.route = { kind: 'settings', section: 'providers' }
    render(<SettingsApp />)
    fireEvent.click(await screen.findByTestId('defaults-translate-select'))
    const menu = await screen.findByTestId('defaults-translate-select-menu')
    fireEvent.click(within(menu).getByText('Index-Translate 9B'))
    await waitFor(() =>
      expect(h.callsOf('offline_model_download')).toEqual([
        { model: 'index_translate_9b', select: true },
      ]),
    )
    expect(screen.queryByTestId('defaults-translate-confirm')).toBeNull()
    expect(h.callsOf('settings_patch')).toHaveLength(0)
  })
})
