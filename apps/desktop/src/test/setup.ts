/**
 * Vitest 全局 setup：jest-dom 断言、每个用例后的清理，并加载各模块自己的
 * `src/**\/__tests__/setup.local.ts`（eager glob，各模块在自己的目录里追加 stub，不改本文件）。
 *
 * jsdom 缺失 API 的基础 stub 在 `stubs.ts`，由 vitest.config.ts 的 `setupFiles` 排在本文件之前执行。
 * eager glob 会被编译成提升到模块顶部的静态 import——所以 local 文件早于本文件的其余代码执行，但一定晚于 stubs.ts，
 * 可以放心使用和覆写基础 stub。
 */
import '@testing-library/jest-dom/vitest'
import { cleanup } from '@testing-library/react'
import { afterEach } from 'vitest'

afterEach(() => {
  cleanup()
  document.documentElement.removeAttribute('data-settled')
})

// 各模块的补充 setup（例如某个视图需要的额外 stub）
import.meta.glob('../**/__tests__/setup.local.ts', { eager: true })
