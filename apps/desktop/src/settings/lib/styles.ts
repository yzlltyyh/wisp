/**
 * 风格的显示名与模型绑定的有效性。
 */
import { tKey } from '../../shared/i18n/i18n'
import type { ModelRef, Provider, Style } from '../../shared/ipc/types'

/** 内置风格按 `styles.<id>` 显示（name 为空），自定义风格显示用户起的名字 */
export function styleName(style: Pick<Style, 'id' | 'name' | 'builtin'>): string {
  if (style.builtin || !style.name)
    return tKey(`styles.${style.id}`, undefined, style.name || style.id)
  return style.name
}

export type ModelRefState = 'ok' | 'missing' | 'disabled'

/** 风格绑定的模型是否还可用：Provider 或模型被删除 → missing，模型被停用 → disabled（两者都会回退到默认模型） */
export function modelRefState(ref: ModelRef, providers: readonly Provider[]): ModelRefState {
  const p = providers.find((x) => x.id === ref.provider_id)
  const model = p?.models.find((x) => x.id === ref.model_id)
  if (!p || !model) return 'missing'
  return model.enabled ? 'ok' : 'disabled'
}

/** 风格模板里允许的变量；其余 `{{…}}` 原样保留并提示 */
export const STYLE_VARIABLES = ['source_lang', 'target_lang'] as const

/** 模板里不认识的变量名（去重，按出现顺序） */
export function unknownVariables(template: string): string[] {
  const out: string[] = []
  for (const m of template.matchAll(/\{\{\s*([^{}]*?)\s*\}\}/g)) {
    const name = m[1] ?? ''
    if (!(STYLE_VARIABLES as readonly string[]).includes(name) && !out.includes(name))
      out.push(name)
  }
  return out
}
