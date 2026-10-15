/**
 * 剪贴板视图与 toast 视图的文案命名空间 `clipboard`。
 * 两个视图都从这里注册（toast 的文案也在 clipboard.json 的 `toast.*` 下）。
 */
import en from '../../locales/en-US/clipboard.json'
import zh from '../../locales/zh-CN/clipboard.json'
import { defineMessages, hasNamespace } from '../../shared/i18n/i18n'

declare module '../../shared/i18n/i18n' {
  interface I18nCatalog {
    clipboard: typeof zh
  }
}

/** 注册 `clipboard` 命名空间（幂等；两个视图在模块顶层各调用一次） */
export function loadClipboardMessages(): void {
  if (!hasNamespace('clipboard')) defineMessages('clipboard', zh, en)
}

/** 原始文案（测试校验 zh-CN / en-US 键集合一致） */
export const CLIPBOARD_MESSAGES = { 'zh-CN': zh, 'en-US': en } as const
