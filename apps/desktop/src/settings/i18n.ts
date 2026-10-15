/**
 * 设置窗口的文案命名空间 `settings`。引导窗口复用其中的服务配置文案（模板名、区域名、表单字段），
 * 因此两边都导入本文件；注册是幂等的。
 *
 * `providers.template.*` / `providers.region.*` 是 wisp-llm 下发的 key，只能扩充、不能删改。
 */
import en from '../locales/en-US/settings.json'
import zh from '../locales/zh-CN/settings.json'
import { defineMessages, hasNamespace } from '../shared/i18n/i18n'
import { loadCoreMessages } from '../shared/i18n/messages'

declare module '../shared/i18n/i18n' {
  interface I18nCatalog {
    settings: typeof zh
  }
}

/** 原始文案（测试校验 zh-CN / en-US 键集合一致） */
export const SETTINGS_MESSAGES = { 'zh-CN': zh, 'en-US': en } as const

export function loadSettingsMessages(): void {
  loadCoreMessages()
  if (!hasNamespace('settings')) defineMessages('settings', zh, en)
}

loadSettingsMessages()
