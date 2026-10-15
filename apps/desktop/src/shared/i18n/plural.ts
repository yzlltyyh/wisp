/**
 * 复数：按 `Intl.PluralRules` 取类别（zh-CN 只有 other；en-US 有 one / other）。
 * `t(key, { count })` 用它挑选 `key_one` / `key_other`。
 */
/** 与 i18n.ts 的 Locale 相同（避免循环导入，这里单独声明） */
type Locale = 'zh-CN' | 'en-US'

const rules = new Map<Locale, Intl.PluralRules>()

export function pluralCategory(n: number, locale: Locale): Intl.LDMLPluralRule {
  let r = rules.get(locale)
  if (!r) {
    r = new Intl.PluralRules(locale)
    rules.set(locale, r)
  }
  return r.select(n)
}

/** 简单的二选一（不经文案表时使用） */
export function plural(
  n: number,
  forms: { one: string; other: string },
  locale: Locale = 'en-US',
): string {
  return pluralCategory(n, locale) === 'one' ? forms.one : forms.other
}
