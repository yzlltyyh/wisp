import { defineConfig, mergeConfig } from 'vitest/config'
import viteConfig from './vite.config.ts'

// 复用 vite.config.ts（插件、define 常量），只追加测试相关配置
export default defineConfig((env) =>
  mergeConfig(viteConfig(env), {
    test: {
      environment: 'jsdom',
      // 顺序有意义：stubs.ts（jsdom 缺失 API 的基础 stub）先执行完，setup.ts 再加载各模块的 setup.local.ts
      setupFiles: ['./src/test/stubs.ts', './src/test/setup.ts'],
      sequence: { setupFiles: 'list' },
      include: ['src/**/*.test.{ts,tsx}'],
      restoreMocks: true,
      unstubGlobals: true,
      css: false,
      // CI 的 Windows runner 比开发机慢数倍：穷举快捷键的核对与同步设置页的交互用例在那里要 4–5 s，会偶尔撞上默认的
      // 5 s 而失败。本机保持默认值，测试变慢时照样能及时发现
      testTimeout: process.env.CI ? 20_000 : 5_000,
    },
  }),
)
