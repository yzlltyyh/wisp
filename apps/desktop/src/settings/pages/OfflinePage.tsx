/**
 * 设置 → 离线模型：在本机运行的文字识别与翻译模型（下载、选用、运行状态、推理组件）。
 *
 * - 顶部一句说明（内容不离开这台设备；只有点击下载才保存到本机）；「本机」一组：内存 / 芯片 / 显卡（Windows / Linux 在
 *   推理组件下载后才能检测，可以重新检测）与占用空间、打开文件夹；
 * - 「文字识别」「翻译」两组模型卡（settings/offline/ModelRow）：下载、暂停 / 继续 / 取消、选用、删除、运行状态；
 * - 「运行」：闲置后释放内存（1 / 5 / 15 / 30 分钟）、使用 GPU 加速、立即释放内存（有模型占着内存时才可点）；
 * - 「下载」：下载源、推理组件（版本、大小，没有被已下载模型依赖时可以删除）、许可与官方页面。
 * 数据来自 `offline_models_get`（第一次打开这一页时 Rust 才读模型库存），之后随 `offline_models:changed` 更新
 * （shared/offline/store）；选用与偏好写进本机的 `offline.*`（不同步）。没有任何定时器：进度全靠推送。
 */
import '../offline/offline.css'
import { Cpu, ExternalLink, FolderOpen, HardDrive, RefreshCw, TriangleAlert } from 'lucide-react'
import { useState } from 'react'
import { formatBytes } from '../../shared/i18n/format'
import { useT } from '../../shared/i18n/useT'
import { api } from '../../shared/ipc'
import type {
  DownloadSource,
  LocalModelId,
  LocalModelInfo,
  LocalModelsSnapshot,
  LocalRuntimeInfo,
} from '../../shared/ipc/types'
import {
  MODEL_RUNTIMES,
  OCR_MODELS,
  TRANSLATE_MODELS,
  isActive,
  modelKind,
} from '../../shared/offline/catalog'
import {
  retryOfflineModels,
  setOfflineSnapshot,
  useOfflineError,
  useOfflineModels,
} from '../../shared/offline/store'
import {
  Button,
  EmptyState,
  Field,
  Icon,
  IconButton,
  Section,
  Skeleton,
  Spinner,
} from '../../shared/ui'
import { Page, SelectRow, SwitchRow } from '../components/Page'
import { ipcErrorText } from '../lib/errors'
import { formatList } from '../lib/format'
import { at } from '../lib/patch'
import { notify, patchSettings, useSettings } from '../store'
import { loadedMemory, loadedModels, runtimeName } from '../offline/model'
import { ModelRow } from '../offline/ModelRow'

const IDLE_MINUTES = ['1', '5', '15', '30'] as const
const SOURCES: readonly DownloadSource[] = ['auto', 'model_scope', 'hugging_face']
const MB = 1024 * 1024

/** 官方页面（许可与来源） */
const LINKS: ReadonlyArray<{ name: string; url: string }> = [
  { name: 'PaddleOCR', url: 'https://github.com/PaddlePaddle/PaddleOCR' }, // i18n-ignore
  { name: 'Index-Translate', url: 'https://github.com/bilibili/Index-Translate' }, // i18n-ignore
  { name: 'llama.cpp', url: 'https://github.com/ggml-org/llama.cpp' }, // i18n-ignore
  { name: 'ONNX Runtime', url: 'https://github.com/microsoft/onnxruntime' }, // i18n-ignore
]

export default function OfflinePage() {
  const t = useT()
  const settings = useSettings()
  const snap = useOfflineModels()
  const error = useOfflineError()
  if (!settings) return null

  let body
  if (!snap && error) {
    body = (
      <EmptyState
        icon={TriangleAlert}
        title={t('settings.offline.load_failed')}
        detail={t('settings.offline.load_failed_detail')}
        action={
          <Button size="sm" onClick={retryOfflineModels} data-testid="offline-retry">
            {t('common.action.retry')}
          </Button>
        }
        data-testid="offline-load-failed"
      />
    )
  } else if (!snap) {
    body = (
      <div className="st-om-skeleton" aria-busy="true">
        <Skeleton lines={4} />
      </div>
    )
  } else {
    const o = settings.offline
    const select = (id: LocalModelId, next: boolean) => {
      const key = modelKind(id) === 'ocr' ? 'offline.ocr_model' : 'offline.translate_model'
      void patchSettings(at(key, next ? id : null))
    }
    const rows = (ids: readonly LocalModelId[]) =>
      ids.map((id) => {
        const m = snap.models.find((x) => x.id === id)
        if (!m) return null
        const chosen = modelKind(id) === 'ocr' ? o.ocr_model : o.translate_model
        return (
          <ModelRow key={id} model={m} selected={chosen === id} onSelect={(v) => select(id, v)} />
        )
      })
    body = (
      <>
        <DeviceSection snap={snap} />
        <Section title={t('settings.offline.group_ocr')} data-testid="offline-group-ocr">
          {rows(OCR_MODELS)}
        </Section>
        <Section
          title={t('settings.offline.group_translate')}
          data-testid="offline-group-translate"
        >
          {rows(TRANSLATE_MODELS)}
        </Section>
        <RunSection snap={snap} />
        <DownloadSection snap={snap} />
      </>
    )
  }

  return (
    <Page
      title={t('settings.nav.offline')}
      description={t('settings.offline.description')}
      data-testid="offline-page"
    >
      {body}
    </Page>
  )
}

/** 本机：内存 / 芯片 / 显卡 + 占用空间 */
function DeviceSection({ snap }: { snap: LocalModelsSnapshot }) {
  const t = useT()
  const [probing, setProbing] = useState(false)
  const d = snap.device
  const memory =
    d.total_memory === null
      ? t('settings.offline.memory_unknown')
      : t('settings.offline.memory', { size: formatBytes(d.total_memory) })
  let gpu: string
  if (d.apple_silicon) gpu = t('settings.offline.apple')
  else if (!d.gpu_probed) gpu = t('settings.offline.gpu_pending')
  else if (d.gpus.length === 0) gpu = t('settings.offline.gpu_none')
  else
    gpu = formatList(
      d.gpus.map((g) =>
        g.integrated
          ? t('settings.offline.gpu_integrated', { name: g.name })
          : t('settings.offline.gpu', { name: g.name, vram: formatBytes(g.vram_total_mb * MB) }),
      ),
    )
  const refresh = async () => {
    setProbing(true)
    try {
      setOfflineSnapshot(await api.offlineDevicesRefresh())
      notify(t('settings.offline.gpu_refreshed'), 'success')
    } catch (error) {
      notify(ipcErrorText(error, 'offline_devices_refresh'), 'danger')
    } finally {
      setProbing(false)
    }
  }
  // 只有装好 llama.cpp 之后才能检测显卡；Apple 芯片不需要
  const canProbe =
    !d.apple_silicon && snap.runtimes.some((r) => r.id === 'llama_cpp' && r.installed)
  return (
    <Section title={t('settings.offline.this_device')} data-testid="offline-device">
      <Field
        label={
          <span className="st-om-device">
            <Icon icon={Cpu} size={14} />
            {memory}
          </span>
        }
        description={gpu}
        data-testid="offline-device-info"
      >
        {canProbe ? (
          <IconButton
            icon={RefreshCw}
            label={t('settings.offline.gpu_refresh')}
            disabled={probing}
            glyph={probing ? <Spinner size={14} /> : undefined}
            onClick={() => void refresh()}
            data-testid="offline-gpu-refresh"
          />
        ) : null}
      </Field>
      <Field
        label={
          <span className="st-om-device">
            <Icon icon={HardDrive} size={14} />
            {snap.disk_used > 0
              ? t('settings.offline.storage_used', { used: formatBytes(snap.disk_used) })
              : t('settings.offline.storage_empty')}
          </span>
        }
        description={
          snap.disk_free !== null
            ? t('settings.offline.storage_free', { free: formatBytes(snap.disk_free) })
            : undefined
        }
        data-testid="offline-storage"
      >
        <Button
          size="sm"
          icon={FolderOpen}
          onClick={() =>
            void api
              .offlineReveal()
              .catch((e: unknown) => notify(ipcErrorText(e, 'offline_reveal'), 'danger'))
          }
          data-testid="offline-open-folder"
        >
          {t('settings.offline.open_folder')}
        </Button>
      </Field>
    </Section>
  )
}

/** 运行：闲置释放、GPU、立即释放 */
function RunSection({ snap }: { snap: LocalModelsSnapshot }) {
  const t = useT()
  const settings = useSettings()
  const [releasing, setReleasing] = useState(false)
  if (!settings) return null
  const o = settings.offline
  const loaded = loadedModels(snap)
  const idle = String(o.idle_release_min)
  const idleOptions = (IDLE_MINUTES as readonly string[]).includes(idle)
    ? IDLE_MINUTES
    : [...IDLE_MINUTES, idle]
  const release = async () => {
    setReleasing(true)
    try {
      await api.offlineRelease()
      notify(t('settings.offline.released'), 'success')
    } catch (error) {
      notify(ipcErrorText(error, 'offline_release'), 'danger')
    } finally {
      setReleasing(false)
    }
  }
  return (
    <Section title={t('settings.offline.run')} data-testid="offline-run">
      <SelectRow<string>
        label={t('settings.offline.idle_release')}
        description={t('settings.offline.idle_release_desc')}
        value={idle}
        options={idleOptions.map((v) => ({
          value: v,
          label: t('settings.offline.minutes', { n: v }),
        }))}
        onChange={(v) => void patchSettings(at('offline.idle_release_min', Number(v)))}
        ariaLabel={t('settings.offline.idle_release')}
        data-testid="offline-idle"
      />
      <SwitchRow
        label={t('settings.offline.use_gpu')}
        description={
          snap.device.apple_silicon
            ? t('settings.offline.use_gpu_desc_apple')
            : t('settings.offline.use_gpu_desc')
        }
        checked={o.use_gpu}
        onChange={(v) => void patchSettings(at('offline.use_gpu', v))}
        ariaLabel={t('settings.offline.use_gpu')}
        data-testid="offline-gpu"
      />
      <Field
        label={t('settings.offline.release')}
        description={
          loaded.length === 0
            ? t('settings.offline.release_idle')
            : t('settings.offline.release_busy', {
                models: formatList(loaded.map((m) => t(`common.offline_model.${m.id}`))),
                mem: formatBytes(loadedMemory(loaded)),
              })
        }
        data-testid="offline-release"
      >
        <Button
          size="sm"
          loading={releasing}
          disabled={loaded.length === 0}
          onClick={() => void release()}
          data-testid="offline-release-button"
        >
          {t('settings.offline.release_action')}
        </Button>
      </Field>
    </Section>
  )
}

/** 下载：下载源、推理组件、许可 */
function DownloadSection({ snap }: { snap: LocalModelsSnapshot }) {
  const t = useT()
  const settings = useSettings()
  if (!settings) return null
  const source = settings.offline.download_source
  return (
    <>
      <Section
        title={t('settings.offline.download_title')}
        description={t('settings.offline.source_note')}
        data-testid="offline-download"
      >
        <SelectRow<DownloadSource>
          label={t('settings.offline.source')}
          description={t(`settings.offline.source_desc.${source}`)}
          value={source}
          options={SOURCES.map((v) => ({ value: v, label: t(`settings.offline.sources.${v}`) }))}
          onChange={(v) => void patchSettings(at('offline.download_source', v))}
          ariaLabel={t('settings.offline.source')}
          data-testid="offline-source"
        />
      </Section>
      <Section
        title={t('settings.offline.runtimes')}
        description={t('settings.offline.runtimes_desc')}
        data-testid="offline-runtimes"
      >
        {snap.runtimes.map((r) => (
          <RuntimeRow key={r.id} runtime={r} models={snap.models} />
        ))}
      </Section>
      <p className="st-om-licenses" data-testid="offline-licenses">
        <span>{t('settings.offline.licenses')}</span>
        <span className="st-om-links">
          {LINKS.map((l) => (
            <button
              key={l.name}
              type="button"
              className="st-om-link"
              aria-label={t('settings.offline.license_link', { name: l.name })}
              onClick={() =>
                void api
                  .openUrl(l.url)
                  .catch((e: unknown) => notify(ipcErrorText(e, 'open_url'), 'danger'))
              }
            >
              {l.name}
              <Icon icon={ExternalLink} size={12} />
            </button>
          ))}
        </span>
      </p>
    </>
  )
}

function RuntimeRow({
  runtime: r,
  models,
}: {
  runtime: LocalRuntimeInfo
  models: readonly LocalModelInfo[]
}) {
  const t = useT()
  const [busy, setBusy] = useState(false)
  const name = runtimeName(r.id)
  // 有模型正在下载、并且要用到它时同样不能删（下载完就会用到）
  const downloading = models.some((m) => isActive(m.state) && MODEL_RUNTIMES[m.id].includes(r.id))
  const remove = async () => {
    setBusy(true)
    try {
      await api.offlineRuntimeDelete(r.id)
      notify(t('settings.offline.runtime_deleted', { name }), 'success')
    } catch (error) {
      notify(ipcErrorText(error, 'offline_runtime_delete'), 'danger')
    } finally {
      setBusy(false)
    }
  }
  return (
    <Field
      label={name}
      description={
        r.installed
          ? [
              t('settings.offline.runtime_meta', { version: r.version, size: formatBytes(r.size) }),
              r.in_use
                ? t('settings.offline.runtime_in_use')
                : downloading
                  ? t('settings.offline.runtime_downloading')
                  : null,
            ]
              .filter(Boolean)
              .join(' · ')
          : t('settings.offline.runtime_missing', { version: r.version, size: formatBytes(r.size) })
      }
      data-testid={`offline-runtime-${r.id}`}
    >
      {r.installed ? (
        <Button
          size="sm"
          variant="ghost"
          className="st-btn-danger"
          loading={busy}
          disabled={r.in_use || downloading}
          onClick={() => void remove()}
          data-testid={`offline-runtime-${r.id}-delete`}
        >
          {t('settings.offline.runtime_delete')}
        </Button>
      ) : null}
    </Field>
  )
}
