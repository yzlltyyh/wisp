/**
 * 引导「接入模型」的离线入口：「不想填 API Key？使用离线模型」之后的简短选择——两档组合：
 * - 轻量：PP-OCRv6 Small + Index-Translate 2B（约 1.3 GB，几乎所有设备都能流畅运行）；
 * - 高质量：PaddleOCR-VL + Index-Translate 9B（约 7 GB，表格与复杂版面、更好的译文）。
 * 按本机的设备适配标注「推荐」（高质量这一档两个模型都适合本机时推荐它，否则推荐轻量），不适合时注明较慢 / 内存不足。
 * 选好后由这一步的底栏「下载并继续」开始后台下载并选用它们（`startPack`），引导继续往下走，下载在后台完成。
 */
import { HardDriveDownload } from 'lucide-react'
import { useT, useTKey } from '../../shared/i18n/useT'
import { formatBytes } from '../../shared/i18n/format'
import { api } from '../../shared/ipc'
import type { FitnessLevel, LocalModelsSnapshot } from '../../shared/ipc/types'
import {
  PACKS,
  packFitness,
  packSize,
  recommendedPack,
  type OfflinePack,
} from '../../shared/offline/catalog'
import { useOfflineModels } from '../../shared/offline/store'
import { Badge, Icon, RadioGroup, Skeleton } from '../../shared/ui'
import { ipcErrorText } from '../../settings/lib/errors'
import { notify, patchSettings } from '../../settings/store'

const ORDER: readonly OfflinePack[] = ['light', 'quality']
const FIT_TONE: Record<Exclude<FitnessLevel, 'good'>, 'warning' | 'danger' | 'neutral'> = {
  slow: 'warning',
  insufficient: 'danger',
  unknown: 'neutral',
}

/** 实际要用的那一档：用户选过的，否则推荐的 */
export function effectivePack(
  pick: OfflinePack | null,
  snap: LocalModelsSnapshot | null,
): OfflinePack {
  return pick ?? (snap ? recommendedPack(snap.models) : 'light')
}

/** 开始下载这一档的两个模型（Rust 排队，一次下一个文件）并在本机选用它们；返回是否成功 */
export async function startPack(pack: OfflinePack): Promise<boolean> {
  const { ocr, translate } = PACKS[pack]
  try {
    // 引导里没有别的模型可用：选择马上写入（下面的 patchSettings），不必等装好后再切换
    await api.offlineModelDownload(translate, false)
    await api.offlineModelDownload(ocr, false)
  } catch (error) {
    notify(ipcErrorText(error, 'offline_model_download'), 'danger')
    return false
  }
  return patchSettings({ offline: { ocr_model: ocr, translate_model: translate } })
}

export function OfflinePacks({
  pick,
  onPick,
}: {
  pick: OfflinePack | null
  onPick: (pack: OfflinePack) => void
}) {
  const t = useT()
  const tKey = useTKey()
  const snap = useOfflineModels()
  if (!snap) {
    return (
      <div className="ob-offline" aria-busy="true">
        <Skeleton lines={3} />
      </div>
    )
  }
  const recommended = recommendedPack(snap.models)
  const value = effectivePack(pick, snap)
  return (
    <div className="ob-offline" data-testid="onboarding-offline">
      <div className="ui-section-card ob-offline-card">
        <RadioGroup<OfflinePack>
          label={t('onboarding.model.offline.title')}
          value={value}
          onChange={onPick}
          className="ob-packs"
          options={ORDER.map((pack) => {
            const fit = packFitness(pack, snap.models)
            const { ocr, translate } = PACKS[pack]
            // 原因（内存偏少、没有显卡……）取较差的那个模型
            const reason =
              fit === 'good'
                ? null
                : ([ocr, translate]
                    .map((id) => snap.models.find((m) => m.id === id)?.fitness)
                    .find((f) => f?.level === fit)?.reason_key ?? null)
            return {
              value: pack,
              label: (
                <span className="ob-pack-title">
                  {t(`onboarding.model.offline.${pack}`)}
                  {pack === recommended && fit === 'good' ? (
                    <Badge tone="accent">{t('onboarding.model.offline.recommended')}</Badge>
                  ) : fit !== 'good' ? (
                    <Badge tone={FIT_TONE[fit]}>{t(`settings.offline.fit.${fit}`)}</Badge>
                  ) : null}
                </span>
              ),
              description: (
                <>
                  <span className="ob-pack-models">
                    {t('onboarding.model.offline.models', {
                      ocr: t(`common.offline_model.${ocr}`),
                      translate: t(`common.offline_model.${translate}`),
                    })}
                  </span>
                  <span className="ob-pack-meta">
                    {[
                      t(`onboarding.model.offline.${pack}_desc`),
                      t('onboarding.model.offline.size', {
                        size: formatBytes(packSize(pack, snap.models, snap.runtimes)),
                      }),
                      reason ? tKey(reason, undefined, '') : null,
                    ]
                      .filter(Boolean)
                      .join(' · ')}
                  </span>
                </>
              ),
            }
          })}
          data-testid="onboarding-offline-packs"
        />
      </div>
      <p className="ob-offline-note">
        <Icon icon={HardDriveDownload} size={14} />
        <span>{t('onboarding.model.offline.note')}</span>
      </p>
    </div>
  )
}
