/**
 * 翻译 / 输入 / Pinned 视图的文案命名空间 `translate`。导入本模块即注册（幂等，`loadTranslateMessages`）；
 * 视图里用 `useT()` 取 `translate.*`，键的类型由 zh-CN 文件推导。
 */
import en from '../../locales/en-US/translate.json'
import zh from '../../locales/zh-CN/translate.json'
import { defineMessages, hasNamespace } from '../../shared/i18n/i18n'

declare module '../../shared/i18n/i18n' {
  interface I18nCatalog {
    translate: typeof zh
  }
}

/** 注册 `translate` 命名空间（幂等）。各模块在模块顶层调用一次 */
export function loadTranslateMessages(): void {
  if (!hasNamespace('translate')) defineMessages('translate', zh, en)
}

loadTranslateMessages()

/** 两种语言的原始文案（测试校验键集合与占位符一致） */
export const TRANSLATE_MESSAGES = { 'zh-CN': zh, 'en-US': en } as const
