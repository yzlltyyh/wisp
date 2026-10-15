/**
 * 引导窗口的文案命名空间 `onboarding`。服务配置相关的文案复用 `settings` 命名空间。
 */
import en from '../locales/en-US/onboarding.json'
import zh from '../locales/zh-CN/onboarding.json'
import { defineMessages, hasNamespace } from '../shared/i18n/i18n'
import { loadSettingsMessages } from '../settings/i18n'

declare module '../shared/i18n/i18n' {
  interface I18nCatalog {
    onboarding: typeof zh
  }
}

export const ONBOARDING_MESSAGES = { 'zh-CN': zh, 'en-US': en } as const

export function loadOnboardingMessages(): void {
  loadSettingsMessages()
  if (!hasNamespace('onboarding')) defineMessages('onboarding', zh, en)
}
