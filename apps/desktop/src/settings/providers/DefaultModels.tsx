/**
 * 默认模型：翻译与 OCR 分开选择；OCR 只列已启用且 vision 为 Yes / Unknown 的模型（Unknown 注明
 * 「未确认是否支持图片」）；默认值指向的模型被删除 / 停用 / 改成不支持图片时标红提示。
 * 没有可选的模型时，禁用的下拉框里只显示「—」，原因写在字段说明里（正常对比度）：两项原因相同时措辞也相同（「先添加服务」）。
 */
import { useT } from '../../shared/i18n/useT'
import type { ModelRef, Provider } from '../../shared/ipc/types'
import { Field, Select } from '../../shared/ui'
import {
  defaultIsValid,
  ocrOptions,
  parseRefKey,
  refKey,
  translateOptions,
  type ModelOption,
} from './model'

export function DefaultModelFields({
  providers,
  translate,
  ocr,
  onChange,
  testId = 'defaults',
}: {
  providers: readonly Provider[]
  translate: ModelRef | null
  ocr: ModelRef | null
  onChange: (which: 'translate' | 'ocr', ref: ModelRef) => void
  testId?: string
}) {
  const t = useT()
  const tOpts = translateOptions(providers)
  const oOpts = ocrOptions(providers)
  const toSelect = (opts: ModelOption[], vision: boolean) =>
    opts.map((o) => ({
      value: o.value,
      label: o.label,
      hint:
        vision && o.vision === 'unknown'
          ? `${o.hint} · ${t('settings.models.vision_unconfirmed')}`
          : o.hint,
    }))
  const tValid = defaultIsValid(translate, tOpts)
  const oValid = defaultIsValid(ocr, oOpts)
  const unknownOcr = ocr && oOpts.find((o) => o.value === refKey(ocr))?.vision === 'unknown'
  const noProviders = tOpts.length === 0
  const ocrDescription = noProviders
    ? t('settings.defaults.none')
    : oOpts.length === 0
      ? t('settings.defaults.none_vision')
      : unknownOcr
        ? t('settings.models.vision_unconfirmed_long')
        : t('settings.defaults.ocr_desc')
  return (
    <>
      <Field
        label={t('settings.defaults.translate')}
        description={
          noProviders ? t('settings.defaults.none') : t('settings.defaults.translate_desc')
        }
        message={!tValid ? t('settings.defaults.invalid') : null}
        data-testid={`${testId}-translate`}
      >
        <Select<string>
          label={t('settings.defaults.translate')}
          value={tValid ? refKey(translate) : null}
          placeholder={tOpts.length ? t('settings.defaults.choose') : t('settings.defaults.empty')}
          options={toSelect(tOpts, false)}
          disabled={tOpts.length === 0}
          invalid={!tValid}
          onChange={(v) => onChange('translate', parseRefKey(v))}
          className="st-select st-select-wide"
          data-testid={`${testId}-translate-select`}
        />
      </Field>
      <Field
        label={t('settings.defaults.ocr')}
        description={ocrDescription}
        message={!oValid ? t('settings.defaults.invalid_ocr') : null}
        data-testid={`${testId}-ocr`}
      >
        <Select<string>
          label={t('settings.defaults.ocr')}
          value={oValid ? refKey(ocr) : null}
          placeholder={oOpts.length ? t('settings.defaults.choose') : t('settings.defaults.empty')}
          options={toSelect(oOpts, true)}
          disabled={oOpts.length === 0}
          invalid={!oValid}
          onChange={(v) => onChange('ocr', parseRefKey(v))}
          className="st-select st-select-wide"
          data-testid={`${testId}-ocr-select`}
        />
      </Field>
    </>
  )
}
