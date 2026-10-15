/**
 * OCR 视图的文案命名空间 `ocr`。模块加载时注册一次（面板入口静态导入视图注册表，首屏前就绪）。
 */
import en from '../../locales/en-US/ocr.json'
import zh from '../../locales/zh-CN/ocr.json'
import { defineMessages, hasNamespace } from '../../shared/i18n/i18n'

declare module '../../shared/i18n/i18n' {
  interface I18nCatalog {
    ocr: typeof zh
  }
}

/** 注册 `ocr` 命名空间（幂等；视图模块加载时调用一次） */
export function registerOcrMessages(): void {
  if (!hasNamespace('ocr')) defineMessages('ocr', zh, en)
}

/** 文案原文（测试校验 zh-CN / en-US 键集合一致） */
export const OCR_MESSAGES = { 'zh-CN': zh, 'en-US': en } as const
