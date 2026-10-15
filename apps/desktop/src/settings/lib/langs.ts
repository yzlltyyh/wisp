/**
 * 翻译的目标语言对：主目标语言与第二语言（原文已经是主目标语言时译成第二语言，wisp_prompts::target_for）。
 * 两者相同就没有意义——英文原文会被「译」成英文——所以改其中一个、而新值正好是另一个时，两者对调，一次写入。
 */
import type { Lang } from '../../shared/ipc/types'
import type { Patch } from './patch'

export interface LangPair {
  primary_lang: Lang
  secondary_lang: Lang
}

/** 把 `which` 改成 `value` 的补丁；与另一项相同时对调 */
export function langPairPatch(pair: LangPair, which: 'primary' | 'secondary', value: Lang): Patch {
  if (which === 'primary') {
    return value === pair.secondary_lang
      ? { translate: { primary_lang: value, secondary_lang: pair.primary_lang } }
      : { translate: { primary_lang: value } }
  }
  return value === pair.primary_lang
    ? { translate: { secondary_lang: value, primary_lang: pair.secondary_lang } }
    : { translate: { secondary_lang: value } }
}
