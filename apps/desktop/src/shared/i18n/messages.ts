/**
 * 五个基础命名空间（common / errors / panel / hotkeys / styles）。
 *
 * React 窗口经 `useT` 自动加载；不用 React 的轻量入口（Overlay）不要导入本文件，只注册自己的命名空间。
 * 其他模块照此写法注册自己的命名空间（例如 `views/translate/i18n.ts`）：
 * ```ts
 * import zh from '../../locales/zh-CN/translate.json'
 * import en from '../../locales/en-US/translate.json'
 * import { defineMessages } from '../../shared/i18n/i18n'
 * declare module '../../shared/i18n/i18n' { interface I18nCatalog { translate: typeof zh } }
 * defineMessages('translate', zh, en)
 * ```
 */
import enCommon from '../../locales/en-US/common.json'
import enErrors from '../../locales/en-US/errors.json'
import enHotkeys from '../../locales/en-US/hotkeys.json'
import enPanel from '../../locales/en-US/panel.json'
import enStyles from '../../locales/en-US/styles.json'
import zhCommon from '../../locales/zh-CN/common.json'
import zhErrors from '../../locales/zh-CN/errors.json'
import zhHotkeys from '../../locales/zh-CN/hotkeys.json'
import zhPanel from '../../locales/zh-CN/panel.json'
import zhStyles from '../../locales/zh-CN/styles.json'
import { defineMessages, hasNamespace } from './i18n'

declare module './i18n' {
  interface I18nCatalog {
    common: typeof zhCommon
    errors: typeof zhErrors
    panel: typeof zhPanel
    hotkeys: typeof zhHotkeys
    styles: typeof zhStyles
  }
}

/** 基础命名空间的原始文案（测试校验 zh-CN / en-US 键集合一致） */
export const CORE_MESSAGES = {
  common: { 'zh-CN': zhCommon, 'en-US': enCommon },
  errors: { 'zh-CN': zhErrors, 'en-US': enErrors },
  panel: { 'zh-CN': zhPanel, 'en-US': enPanel },
  hotkeys: { 'zh-CN': zhHotkeys, 'en-US': enHotkeys },
  styles: { 'zh-CN': zhStyles, 'en-US': enStyles },
} as const

/** 注册基础命名空间（幂等） */
export function loadCoreMessages(): void {
  if (hasNamespace('common')) return
  // 逐个注册：en-US 的形状由 MessageShape<zh-CN> 在编译期校验（缺键即类型错误）
  defineMessages('common', zhCommon, enCommon)
  defineMessages('errors', zhErrors, enErrors)
  defineMessages('panel', zhPanel, enPanel)
  defineMessages('hotkeys', zhHotkeys, enHotkeys)
  defineMessages('styles', zhStyles, enStyles)
}
