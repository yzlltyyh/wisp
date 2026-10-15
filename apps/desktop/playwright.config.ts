/**
 * 浏览器级用例（真实引擎里的焦点、组合按键、逐帧布局与样式——jsdom 复现不了）。
 *
 * 运行：`pnpm -C apps/desktop exec playwright test`（第一次先 `pnpm -C apps/desktop exec playwright install webkit chromium`）。
 * 用例是 `src/**\/__e2e__/*.e2e.ts`，放在被测代码旁边；先以 `--mode mock` 生产构建到 target/e2e，再在独占端口上 preview。
 *
 * 端口默认 1479，可用环境变量 `WISP_E2E_PORT` 换一个（同一台机器上同时跑多份检出时各用各的）。端口上已有服务时**不复用**、
 * 直接报错：复用会测到另一份检出的构建。
 *
 * CI：ci.yml 的 `e2e` 作业在 Linux 上的 Playwright 官方容器里跑（浏览器与系统依赖都在镜像里，版本与
 * `@playwright/test` 一致）。CI 上失败的用例重试 1 次（重试通过记为 flaky），保留失败那一次的 trace 与截图（重试通过的
 * flaky 用例也保留——retain-on-failure 按每一次执行判断）；另写一份 JSON 报告（artifacts/e2e-report.json），
 * scripts/ci/e2e-summary.mjs 据此把 flaky / 失败的用例列进 Actions 摘要，有 flaky 时同样上传 trace；`test.only` 一律拒绝。
 *
 * README 截图与视觉 QA 截图矩阵不经过这份配置：scripts/qa/screenshots.ts 直接调用 Playwright 的库接口。
 */
import { defineConfig, devices } from '@playwright/test'

const CI = !!process.env.CI
const PORT = Number(process.env.WISP_E2E_PORT) || 1479

export default defineConfig({
  testDir: './src',
  testMatch: /__e2e__\/.*\.e2e\.ts$/,
  outputDir: '../../artifacts/e2e',
  fullyParallel: true,
  forbidOnly: CI,
  retries: CI ? 1 : 0,
  reporter: CI
    ? [['line'], ['github'], ['json', { outputFile: '../../artifacts/e2e-report.json' }]]
    : 'list',
  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
    trace: CI ? 'retain-on-failure' : 'off',
    screenshot: CI ? 'only-on-failure' : 'off',
  },
  projects: [
    { name: 'webkit', use: { ...devices['Desktop Safari'] } },
    { name: 'chromium', use: { ...devices['Desktop Chrome'] } },
  ],
  // 工作目录是本文件所在的 apps/desktop（Playwright 的默认值）
  webServer: {
    command: `node node_modules/vite/bin/vite.js build --mode mock --outDir ../../target/e2e --emptyOutDir --logLevel warn && node node_modules/vite/bin/vite.js preview --mode mock --outDir ../../target/e2e --host 127.0.0.1 --port ${PORT} --strictPort`,
    url: `http://127.0.0.1:${PORT}/panel.html`,
    reuseExistingServer: false,
    timeout: 120_000,
  },
})
