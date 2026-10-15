/// <reference types="vite/client" />

/** 浏览器 mock 的编译期开关（vite.config.ts 的 define）：dev server / vitest / `--mode mock` 为 true，生产构建为 false */
declare const __WISP_MOCK__: boolean

interface ImportMetaEnv {
  /** `VITE_MOCK=1` 时生产构建也带上浏览器 mock（视觉 QA 的静态包） */
  readonly VITE_MOCK?: string
}

interface ImportMeta {
  readonly env: ImportMetaEnv
}
