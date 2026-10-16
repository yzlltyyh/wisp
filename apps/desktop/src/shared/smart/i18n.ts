/**
 * 识别出的片段与「粘贴为…」变换的文案命名空间 `smart`：种类名、变换名、芯片的无障碍名称与提示。剪贴板视图、OCR 视图
 * 与固定窗口都用到它们，放在各视图自己的命名空间之外，用到芯片的窗口不必为此加载整个剪贴板的文案。
 */
import en from '../../locales/en-US/smart.json'
import zh from '../../locales/zh-CN/smart.json'
import { defineMessages, hasNamespace } from '../i18n/i18n'

declare module '../i18n/i18n' {
  interface I18nCatalog {
    smart: typeof zh
  }
}

/** 注册 `smart` 命名空间（幂等；展示注册表 specs.ts 加载时调用） */
export function loadSmartMessages(): void {
  if (!hasNamespace('smart')) defineMessages('smart', zh, en)
}

/** 原始文案（测试校验 zh-CN / en-US 键集合一致） */
export const SMART_MESSAGES = { 'zh-CN': zh, 'en-US': en } as const
