/**
 * 「用哪个模型」的下拉（翻译 / OCR）：分组列出离线模型、文字识别服务（PaddleOCR，只有 OCR 才有）与各 LLM 服务的模型。
 * 写入规则见 engine.ts：离线模型只写本机的 `offline.*`，其他写同步的 `defaults.*` 并清空本机的离线选择。
 *
 * - 离线模型没下载时也列出，右侧注明大小（下载中注明进度）；选中它不会立即生效，而是在下方就地确认「下载并使用」
 *   （设备内存不足、只能在 CPU 上运行或显卡还没检测时，确认条先说明原因）；确认后只开始后台下载，**装好之后**才由 Rust
 *   切换过去（`offline_model_download` 的 `select`）——下载期间原来的模型照常可用，不会每次划词都落到「正在下载」的卡片；
 *   选中一个正在下载的离线模型同样只是登记「装好后切换」；
 * - 选中离线模型时字段说明换成「仅在这台设备上生效」：其他设备仍按各自的选择；
 * - 没有离线模式（`offline = false`，引导里正在填写的服务）时只列这个服务的模型，值与写入都由调用方负责。
 */
import { HardDriveDownload } from 'lucide-react'
import { useState } from 'react'
import { useT } from '../../shared/i18n/useT'
import { formatBytes } from '../../shared/i18n/format'
import { api } from '../../shared/ipc'
import type { LocalModelId, LocalModelInfo, ModelRef, Provider } from '../../shared/ipc/types'
import { downloadSize, isActive, needsFitConfirm, progressOf } from '../../shared/offline/catalog'
import { setOfflineSnapshot, useOfflineModels } from '../../shared/offline/store'
import { Field, InlineConfirm, Select, type SelectOption } from '../../shared/ui'
import { ipcErrorText } from '../lib/errors'
import { notify, patchSettings, useSettings } from '../store'
import {
  choicePatch,
  cloudOptions,
  currentValue,
  offlineCandidates,
  offlineOfValue,
  offlineValue,
  type EngineTask,
} from './engine'
import { defaultIsValid, parseRefKey, refKey } from './model'

const PERCENT = 100

/**
 * 开始下载离线模型（之后的进度经 `offline_models:changed` 推送）；`select`：装好后由 Rust 按模型类别选用它。失败时提示
 */
export async function startOfflineDownload(id: LocalModelId, select: boolean): Promise<boolean> {
  try {
    await api.offlineModelDownload(id, select)
    return true
  } catch (error) {
    notify(ipcErrorText(error, 'offline_model_download'), 'danger')
    return false
  }
}

/** 离线模型在下拉右侧的说明：已下载不注明；下载中注明进度；其余注明要下载多少 */
function offlineHint(info: LocalModelInfo | null, t: ReturnType<typeof useT>): string | undefined {
  if (!info) return undefined
  const s = info.state
  if (s.state === 'installed') return undefined
  if (s.state === 'downloading' || s.state === 'paused') {
    const n = Math.round((progressOf(s) ?? 0) * PERCENT)
    return s.state === 'paused'
      ? t('settings.engine.hint_paused', { n })
      : t('settings.engine.hint_downloading', { n })
  }
  if (isActive(s)) return t('settings.engine.hint_downloading_short')
  return formatBytes(downloadSize(info))
}

export interface EngineFieldProps {
  task: EngineTask
  /** 只列这些服务（引导里正在填写的服务）；缺省为设置里的全部服务 */
  providers?: readonly Provider[]
  /** 列出离线模型并直接写入设置（设置页）；false 时由 value / onChange 受控（引导的表单） */
  offline?: boolean
  value?: ModelRef | null
  onChange?: (ref: ModelRef) => void
  label: string
  /** 宽下拉（「默认模型」两栏）；OCR 分组里是普通宽度 */
  wide?: boolean
  'data-testid'?: string
}

export function EngineField({
  task,
  providers: only,
  offline = false,
  value,
  onChange,
  label,
  wide = false,
  'data-testid': testId,
}: EngineFieldProps) {
  const t = useT()
  const settings = useSettings()
  const snap = useOfflineModels(offline)
  /** 选了还没下载的离线模型：在下方确认「下载并使用」 */
  const [pending, setPending] = useState<LocalModelId | null>(null)
  if (!settings) return null
  const providers = only ?? settings.providers
  const cloud = cloudOptions(providers, task)
  const offlineList = offline ? offlineCandidates(task, snap?.models ?? null) : []
  const current = offline ? currentValue(settings, task) : refKey(value ?? null)
  const localNow = current ? offlineOfValue(current) : null
  const cloudRef = localNow ? null : current ? parseRefKey(current) : null
  const cloudValid = localNow !== null || defaultIsValid(cloudRef, cloud)

  const offlineGroup = t('settings.engine.group_offline')
  const options: SelectOption<string>[] = [
    ...offlineList.map(({ id, info }) => ({
      value: offlineValue(id),
      label: t(`common.offline_model.${id}`),
      hint: offlineHint(info, t),
      group: offlineGroup,
    })),
    ...cloud.map((o) => ({
      value: o.value,
      label: o.label,
      hint:
        task === 'ocr' && o.vision === 'unknown'
          ? t('settings.models.vision_unconfirmed')
          : undefined,
      group: o.hint,
    })),
  ]

  const pick = (v: string) => {
    setPending(null)
    const local = offlineOfValue(v)
    if (!offline) {
      if (!local) onChange?.(parseRefKey(v))
      return
    }
    if (local && snap?.models.find((m) => m.id === local)?.state.state !== 'installed') {
      // 正在下载的：登记「装好后切换」，现在的选择不变；没下载的先确认
      const info = snap?.models.find((m) => m.id === local)
      if (info && isActive(info.state)) {
        void switchLater(local)
        return
      }
      setPending(local)
      return
    }
    void patchSettings(choicePatch(settings, task, v))
  }

  /** 下载（或接着下载）并在装好后切换过去；下拉的值保持原来的模型，底部提示一句什么时候切换 */
  const switchLater = async (id: LocalModelId) => {
    if (!(await startOfflineDownload(id, true))) return
    notify(t('settings.engine.switch_later', { model: t(`common.offline_model.${id}`) }), 'success')
    // 进度经事件推送；这里顺带刷新一次，让下拉右侧马上显示「下载中」
    api
      .offlineModelsGet()
      .then(setOfflineSnapshot)
      .catch(() => undefined)
  }
  const confirmDownload = () => {
    const id = pending
    if (!id) return
    setPending(null)
    void switchLater(id)
  }

  const pendingInfo = pending ? snap?.models.find((m) => m.id === pending) : undefined
  const pendingName = pending ? t(`common.offline_model.${pending}`) : ''
  const pendingSize = pendingInfo ? formatBytes(downloadSize(pendingInfo)) : ''
  const fit = pendingInfo?.fitness
  const confirmText = !pending
    ? ''
    : fit && needsFitConfirm(fit.level) && fit.reason_key
      ? t('settings.engine.confirm_unfit', {
          model: pendingName,
          size: pendingSize,
          reason: t(fit.reason_key as 'settings.offline.fitness.cpu_only'),
        })
      : t('settings.engine.confirm', { model: pendingName, size: pendingSize })

  let description: string
  if (localNow) {
    const info = snap?.models.find((m) => m.id === localNow)
    const state = info?.state.state
    description =
      state && state !== 'installed'
        ? t('settings.engine.local_not_ready')
        : t('settings.engine.local_only')
  } else if (task === 'translate') {
    description =
      cloud.length === 0 && !offline
        ? t('settings.defaults.none')
        : cloud.length === 0
          ? t('settings.engine.none_translate')
          : t('settings.defaults.translate_desc')
  } else {
    const unknown =
      cloudRef && cloud.find((o) => o.value === refKey(cloudRef))?.vision === 'unknown'
    description =
      providers.length === 0 && !offline
        ? t('settings.defaults.none')
        : cloud.length === 0 && !offline
          ? t('settings.defaults.none_vision')
          : unknown
            ? t('settings.models.vision_unconfirmed_long')
            : t('settings.engine.ocr_desc')
  }
  const empty = options.length === 0

  return (
    <>
      <Field
        label={label}
        description={description}
        message={
          !cloudValid
            ? t(task === 'ocr' ? 'settings.defaults.invalid_ocr' : 'settings.defaults.invalid')
            : null
        }
        data-testid={testId}
      >
        <Select<string>
          label={label}
          value={cloudValid ? current : null}
          placeholder={empty ? t('settings.defaults.empty') : t('settings.defaults.choose')}
          options={options}
          disabled={empty}
          invalid={!cloudValid}
          onChange={pick}
          className={wide ? 'st-select st-select-wide' : 'st-select'}
          data-testid={testId ? `${testId}-select` : undefined}
        />
      </Field>
      <InlineConfirm
        open={pending !== null}
        message={confirmText}
        confirmLabel={t('settings.engine.download_and_use')}
        tone={fit && needsFitConfirm(fit.level) ? 'warning' : 'default'}
        icon={fit && needsFitConfirm(fit.level) ? undefined : HardDriveDownload}
        timeoutMs={null}
        onCancel={() => setPending(null)}
        onConfirm={confirmDownload}
        data-testid={testId ? `${testId}-confirm` : undefined}
      />
    </>
  )
}
