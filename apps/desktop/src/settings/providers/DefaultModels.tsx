/**
 * 默认模型：翻译与 OCR 分开选择（EngineField）。设置页里两个下拉都分组列出离线模型、文字识别服务（OCR）与各 LLM 服务的
 * 模型；选中离线模型只在本机生效（engine.ts）。引导里正在填写的服务（`providers` 只有它、`offline = false`）只列它的
 * 模型，值由调用方保存。
 * OCR 只列已启用且 vision 为 Yes / Unknown 的模型（Unknown 注明「未确认是否支持图片」）与文字识别服务的模型；
 * 默认值指向的模型被删除 / 停用 / 改成不支持图片时标红提示。没有可选的模型时，禁用的下拉框里只显示「—」，原因写在
 * 字段说明里（正常对比度）。
 */
import { useT } from '../../shared/i18n/useT'
import type { ModelRef, Provider } from '../../shared/ipc/types'
import { EngineField } from './EngineField'

export function DefaultModelFields({
  providers,
  translate,
  ocr,
  onChange,
  offline = false,
  testId = 'defaults',
}: {
  /** 只列这些服务（引导的表单）；设置页传 undefined，列出全部服务与离线模型 */
  providers?: readonly Provider[]
  translate?: ModelRef | null
  ocr?: ModelRef | null
  onChange?: (which: 'translate' | 'ocr', ref: ModelRef) => void
  /** 列出离线模型并直接写入设置 */
  offline?: boolean
  testId?: string
}) {
  const t = useT()
  return (
    <>
      <EngineField
        task="translate"
        providers={providers}
        offline={offline}
        value={translate}
        onChange={(ref) => onChange?.('translate', ref)}
        label={t('settings.defaults.translate')}
        wide
        data-testid={`${testId}-translate`}
      />
      <EngineField
        task="ocr"
        providers={providers}
        offline={offline}
        value={ocr}
        onChange={(ref) => onChange?.('ocr', ref)}
        label={t('settings.defaults.ocr')}
        wide
        data-testid={`${testId}-ocr`}
      />
    </>
  )
}
