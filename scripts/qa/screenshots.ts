#!/usr/bin/env node
/**
 * scripts/qa/screenshots.ts：视觉 QA 截图矩阵与流畅度 trace。
 *
 * 用法（`pnpm qa:shots [选项]`，Node 原生类型剥离直接运行，只用可擦除的 TS 语法）：
 *   --engine webkit|chromium|all   默认 all（WebKit ≈ WKWebView / WebKitGTK，Chromium ≈ WebView2）
 *   --filter <子串[,子串…]>         只跑 id 含任一子串的场景（逗号分隔；一个都没匹配到时以非 0 退出）
 *   --quick                        只跑 浅色 × macOS × 正常动效（本地快速自查）
 *   --lang zh-CN|en-US|pseudo      界面语言，默认 zh-CN（pseudo 为伪本地化，用来发现硬编码文案）
 *   --perf                         另外对四个流畅度场景（PERF_SCENARIOS）跑 trace：Chromium 不允许 > 8 ms 的主线程任务，
 *                                  WebKit 用 rAF 采样要求帧间隔 p95 ≤ 17 ms（容差见 perfWebkit）；不达标、trace
 *                                  没有覆盖到场景负载、或必需场景缺失时以非 0 退出；在虚拟机上（CI 的 macos-15 runner）
 *                                  流畅度不达标与 data-settled 超时只告警（见 virtualMachine）
 *   --allow-missing-perf           --perf 时允许必需场景缺失（只告警；场景还没写好时用）
 *   --out <目录>                   默认 artifacts/qa
 *   --list                         只列出场景
 *   --dev                          用 vite dev server（默认先做 --mode mock 的生产构建再 preview，与真实产物一致）
 *   --port <n>                     自己起的页面服务用这个端口（默认由系统分配空闲端口，多个工作区同时运行互不冲突）
 *   --base <url>                   不自己起服务，复用已在运行的页面服务（例如带 HMR 的 `pnpm dev:mock`）。复用前核对
 *                                  它服务的是本工作区：dev server 必须能经 `/@fs/` 原样返回本工作区的 apps/desktop/package.json
 *                                  （别的工作区的 dev server 的 fs.allow 不含本工作区），preview 的 panel.html 必须与本工作区最近
 *                                  一次 `--mode mock` 构建（target/qa/dist 或浏览器级用例的 target/e2e*）逐字一致；核对不过就报错退出
 *   --readme                       README 档位：按 scripts/qa/readme-shots.ts 的清单（场景 id、主题、视口、裁切框、输出宽度）
 *                                  重新生成 docs/images/ 下的 README 截图（WebKit、macOS 外观、zh-CN，2× 渲染后缩小），再核对
 *                                  README.md 的图片引用。可配 --list（只列清单）、--filter（文件名或场景 id 的子串）、--out
 *                                  （先输出到别处预览）与 --dev / --port / --base；不接受 --engine / --lang / --quick / --perf
 *
 * 场景来自各视图维护的 `src/shared/ipc/mock/scenarios/*.ts`，经 `window.__WISP_MOCK__.listScenarios()` 读取。
 * 矩阵：场景 × {light, dark} × {macos, windows} × {正常动效, 减少动效}；等 `<html data-settled>`、再等画面静止（没有在途
 * 请求、DOM 连续 400 ms 不变，见 QUIET_PROBE）；设置页场景再核对没有骨架 / aria-busy、设置窗口已渲染（见 contentProbe），
 * 之后截图。场景自带的断言
 * 写在 `<html data-qa-assert="ok|fail">`（例如 clipboard.scroll-10k / focus-keyboard）：一出现 `fail` 就记为失败
 * （附上页面最后一条 console.error），不再等到超时。
 * `motion: true` 的场景（正常动效）另录视频（artifacts/qa/video/）并在 `data-motion-mid` 时截中间帧。
 * 输出 `<out>/<engine>/<scenario>__<theme>__<os>[__reduced].png` 与汇总页 `<out>/index.html`。
 * 浏览器：`pnpm -C apps/desktop exec playwright install webkit chromium`。
 */
/* oxlint-disable no-await-in-loop -- 轮询服务就绪、逐个引擎 / 场景运行都必须串行（并发由 pool 控制） */
import { spawn, spawnSync, type ChildProcess } from 'node:child_process'
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  renameSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { createRequire } from 'node:module'
import { createServer } from 'node:net'
import { dirname, join, relative } from 'node:path'
import { parseArgs } from 'node:util'
import { APP_DIR, ROOT, error, info, ok, run, warn } from '../lib/common.mjs'
import {
  checkReadmeRefs,
  expectedSize,
  MAX_BYTES,
  pngSize,
  README_ICON,
  README_SHOTS,
  readmeImageRefs,
  RENDER_SCALE,
  validateShots,
  type ReadmeShot,
} from './readme-shots.ts'

/** 页面服务的根地址（main 里按 --base / 自己起的服务确定；各函数读取它） */
let BASE = ''
/** 自己起页面服务时绑定的地址：只用 IPv4 回环，空闲端口的探测与 vite 的监听在同一个地址族上 */
const HOST = '127.0.0.1'
/** 自动分配的端口被别的进程抢先占用时重试的次数 */
const PORT_ATTEMPTS = 3
const VIEWPORT = { width: 1280, height: 800 }
const SETTLE_TIMEOUT = 20_000
const CONCURRENCY = 4
/**
 * 截图本身的时限（Playwright 默认 30 s）：没有 GPU 的虚拟机上，Chromium 给很长的整页（shell.gallery，2×）做软件光栅化会超过
 * 30 s（runner 上 8 张里 5 张 `page.screenshot: Timeout 30000ms exceeded`，本机几秒）
 */
const SHOT_TIMEOUT = 120_000
/** 流畅度场景等 data-settled 的时限：真机 60 s；虚拟机上开着 trace 的 translate.stress-stream 超过 60 s（本机约 43 s） */
const PERF_SETTLE_TIMEOUT = 60_000
const PERF_SETTLE_TIMEOUT_VM = 180_000
/** 必需的流畅度场景（由各视图的 mock 场景提供）与预算 */
const PERF_SCENARIOS = [
  'translate.stress-stream',
  'clipboard.scroll-10k',
  'overlay.drag',
  'shell.view-switch',
]
const MAX_TASK_MS = 8
const MAX_FRAME_P95_MS = 17
/** 相对引擎空闲帧间隔的容差（无头 WebKit 的 rAF 由定时器驱动，空闲时帧间隔本身就在 16–18 ms 抖动） */
const FRAME_JITTER = 1.15
/** 超过空闲帧间隔这么多倍的帧记为掉帧（只用于汇总页的掉帧率） */
const DROP_FACTOR = 1.5
/** 一个流畅度场景的采样窗口里至少要有这么多帧 / 主线程任务，否则视为没有覆盖到场景负载 */
const MIN_PERF_SAMPLES = 10
/**
 * 流畅度采样窗口（两个引擎同一口径）：从「应用挂载完成后的第一帧渲染完」（`qa-start`）到 `<html data-settled>`
 * 出现（`qa-settled`），以 User Timing 标记写进 Chromium 的 trace；同一段时间里用 rAF 记录每一帧的时间戳（WebKit 用）。
 * 模块求值、字体加载、mock 装配与首屏挂载（`#root` 第一次有内容那一帧的布局 / 绘制）不计——流畅度测的是交互与
 * 流式阶段；没有 `#root` 内容的页面在 load 后 2 秒内或 data-settled 时开始
 */
const PERF_PROBE = `(() => {
  const settled = () => document.documentElement.hasAttribute('data-settled')
  let begun = false
  const start = () => {
    performance.mark('qa-start')
    const frames = []; window.__qaFrames = frames
    const tick = (now) => { frames.push(now); if (!settled()) requestAnimationFrame(tick) }
    requestAnimationFrame(tick)
    if (settled()) { performance.mark('qa-settled'); return }
    new MutationObserver((_, o) => { if (settled()) { performance.mark('qa-settled'); o.disconnect() } })
      .observe(document.documentElement, { attributes: true, attributeFilter: ['data-settled'] })
  }
  // 挂载那一帧渲染完之后再开始：rAF 回调在该帧布局之前，setTimeout 落在它之后的新任务里
  const begin = () => { if (!begun) { begun = true; requestAnimationFrame(() => setTimeout(start)) } }
  addEventListener('load', () => document.fonts.ready.then(() => {
    const root = document.getElementById('root')
    const mounted = () => settled() || (root !== null && root.childElementCount > 0)
    if (mounted()) return begin()
    const o = new MutationObserver(() => { if (mounted()) { o.disconnect(); begin() } })
    if (root) o.observe(root, { childList: true })
    o.observe(document.documentElement, { attributes: true, attributeFilter: ['data-settled'] })
    setTimeout(() => { o.disconnect(); begin() }, 2000)
  }))
})()`
/**
 * 截图前的「画面静止」判定（否则默认并发下设置页会截成空白内容区 / 骨架）。mock 的 data-settled 只看 mock
 * 自己的事件、流与定时器，不知道 `lazy()` 分组页的代码块与 React 的 Suspense 揭示：设置页先提交 PageSkeleton，React 19
 * 把 Suspense 内容的揭示节流到 fallback 出现 300 ms 之后（FALLBACK_THROTTLE_MS），data-settled 往往落在这之前（实测
 * 串行时也早约 200 ms），截图本身的耗时决定了截到的是骨架还是内容，并发越高越容易截到骨架。所以 data-settled 之后再等：
 * 没有在途的网络请求（懒加载的代码块）、字体已加载、DOM 连续 QUIET_MS 没有变化（子节点 / 文本 / 除 style 之外的属性；
 * style 是动效逐帧写的）。QUIET_MS 必须大于 React 的 300 ms 节流。QUIET_TIMEOUT 内等不到（持续变化的场景）时照常截图，
 * 在汇总页与日志里标注
 */
const QUIET_MS = 400
const QUIET_TIMEOUT = 5_000
const QUIET_PROBE = `(() => {
  window.__qaLastMutation = performance.now()
  const ignored = new Set(['style', 'data-settled', 'data-motion-mid', 'data-qa-assert'])
  new MutationObserver((list) => {
    for (const m of list) {
      if (m.type !== 'attributes' || !ignored.has(m.attributeName)) {
        window.__qaLastMutation = performance.now()
        return
      }
    }
  }).observe(document, { subtree: true, childList: true, characterData: true, attributes: true })
})()`
/** [距最后一次 DOM 变化的毫秒数, 字体是否都已加载] */
const QUIET_STATE =
  "[performance.now() - (window.__qaLastMutation ?? 0), document.fonts.status === 'loaded']"
/** PERF_PROBE 已经打下 qa-settled 标记（采样窗口结束） */
const SETTLED_MARK = "performance.getEntriesByName('qa-settled').length > 0"
/** 场景到达 data-settled，或场景自带的断言已判定失败（`<html data-qa-assert="fail">`） */
const SETTLED_OR_FAILED =
  "document.documentElement.hasAttribute('data-settled') || document.documentElement.dataset.qaAssert === 'fail'"

/**
 * 截图前的正向条件（画面静止判定的补充）。静止判定是启发式的：负载高时 React 的揭示节流定时器、懒加载
 * 代码块的求值都可能晚于 QUIET_MS 才发生，waitQuiet 就会在设置页的 PageSkeleton 还挂着时返回。所以截图前再核对页面状态：
 * - 设置页场景（`settings.loading` 本身就是加载态，除外）：没有分组页骨架（`settings-page-loading`）、没有 `aria-busy="true"`
 *   的元素、设置窗口已渲染（`settings-app`；入口的 `lazy()` 没加载完时是空白的 fallback）；
 * - README 截图（都是完成态）：没有骨架、没有 aria-busy。
 * 不满足就接着等（骨架换成内容本身是一次 DOM 变化，之后重新等静止）；QUIET_TIMEOUT 内仍不满足就报错，这张图不写入。
 * 返回页面里求值的表达式（满足时得到空串，否则是原因），不需要核对时返回 null
 */
function contentProbe(scenarioId: string, readme: boolean): string | null {
  const settings = scenarioId.startsWith('settings.') && scenarioId !== 'settings.loading'
  if (!settings && !readme) return null
  const app = settings
    ? `if (!document.querySelector('[data-testid="settings-app"]')) return '设置窗口没有渲染（空白）'`
    : ''
  return `(() => {
    if (document.querySelector('[data-testid="settings-page-loading"]')) return '设置分组页还是骨架（PageSkeleton）'
    const busy = document.querySelector('[aria-busy="true"]')
    if (busy) {
      const cls = typeof busy.className === 'string' && busy.className ? '.' + busy.className.trim().split(/\\s+/).join('.') : ''
      return '页面上还有 aria-busy 的元素（' + busy.tagName.toLowerCase() + cls + '）'
    }
    ${app}
    return ''
  })()`
}

interface ScenarioInfo {
  id: string
  label: string
  view: string
  motion?: boolean
  url: string
}
interface Shot {
  scenario: ScenarioInfo
  engine: string
  theme: 'light' | 'dark'
  os: 'macos' | 'windows'
  reduced: boolean
  file: string
  mid?: string
  video?: string
  error?: string
  /** QUIET_TIMEOUT 内画面没有静止（仍然截了图） */
  unsettled?: boolean
  /** 画面静止之后 contentProbe 仍不满足、多等了一会儿才截（静止判定漏掉的情形，截到的是内容） */
  late?: boolean
}
// Playwright 的类型来自 apps/desktop 的 devDependency，这里只声明用到的最小形状
interface Page {
  goto(url: string, opts?: object): Promise<unknown>
  waitForFunction(fn: string, arg?: unknown, opts?: object): Promise<unknown>
  waitForSelector(sel: string, opts?: object): Promise<unknown>
  screenshot(opts: object): Promise<unknown>
  evaluate<T>(fn: string): Promise<T>
  addInitScript(script: string): Promise<void>
  on(event: 'console', fn: (msg: { type(): string; text(): string }) => void): void
  on(event: 'request' | 'requestfinished' | 'requestfailed', fn: () => void): void
  video(): { path(): Promise<string> } | null
  close(): Promise<void>
}
interface Context {
  newPage(): Promise<Page>
  close(): Promise<void>
}
interface Browser {
  newContext(opts: object): Promise<Context>
  close(): Promise<void>
  startTracing?(page: Page, opts: object): Promise<void>
  stopTracing?(): Promise<Buffer>
}
interface BrowserType {
  launch(opts?: object): Promise<Browser>
}

function loadPlaywright(): Record<'webkit' | 'chromium', BrowserType> {
  const require = createRequire(join(APP_DIR, 'package.json'))
  return require('@playwright/test')
}

async function reachable(url: string): Promise<boolean> {
  try {
    const res = await fetch(url, { signal: AbortSignal.timeout(1500) })
    return res.ok
  } catch {
    return false
  }
}

async function fetchText(url: string): Promise<string | null> {
  try {
    const res = await fetch(url, { signal: AbortSignal.timeout(3000) })
    return res.ok ? await res.text() : null
  } catch {
    return null
  }
}

/** 系统分配的空闲端口（在 HOST 上探测） */
function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const probe = createServer()
    probe.unref()
    probe.once('error', reject)
    probe.listen(0, HOST, () => {
      const addr = probe.address()
      const port = typeof addr === 'object' && addr ? addr.port : 0
      probe.close(() => (port ? resolve(port) : reject(new Error('系统没有分配空闲端口'))))
    })
  })
}

/**
 * 本工作区最近一次 `--mode mock` 构建的 panel.html 所在目录：qa:shots 的 target/qa/dist，以及浏览器级用例
 * （apps/desktop/playwright.config.ts）构建到的 target/e2e*
 */
const MOCK_BUILDS = [
  join(ROOT, 'target', 'qa', 'dist'),
  ...(existsSync(join(ROOT, 'target'))
    ? readdirSync(join(ROOT, 'target'))
        .filter((name) => name.startsWith('e2e'))
        .map((name) => join(ROOT, 'target', name))
    : []),
]

/**
 * `--base` 复用别人起的页面服务之前，核对它服务的是本工作区（多个工作区（git worktree）同时在跑页面服务时，复用到别的
 * 工作区会截到别人的代码）。dev server：经 `/@fs/` 取本工作区的 apps/desktop/package.json，必须原样返回——别的工作区的
 * dev server 的 `server.fs.allow` 只含它自己的工作区，会返回 403；preview：panel.html 必须与本工作区某次 mock 构建逐字一致
 * （资源文件名带内容哈希，代码不同 panel.html 就不同）
 */
async function verifyExternalServer(base: string): Promise<string> {
  if (!(await reachable(`${base}/panel.html`))) {
    throw new Error(`--base ${base}：${base}/panel.html 无法访问（页面服务没有在运行？）`)
  }
  const pkg = join(APP_DIR, 'package.json')
  const viaFs = await fetchText(`${base}/@fs/${pkg.replaceAll('\\', '/').replace(/^\//, '')}`)
  if (viaFs !== null && viaFs === readFileSync(pkg, 'utf8')) return 'dev server，/@fs/ 核对通过'
  const served = await fetchText(`${base}/panel.html`)
  for (const dir of MOCK_BUILDS) {
    const html = join(dir, 'panel.html')
    if (served !== null && existsSync(html) && served === readFileSync(html, 'utf8')) {
      return `preview，与 ${relative(ROOT, dir)} 的构建一致`
    }
  }
  throw new Error(
    `--base ${base} 服务的不是本工作区（${ROOT}）：dev server 的 /@fs/ 取不到本工作区的 apps/desktop/package.json，` +
      `panel.html 也与本工作区的 mock 构建（${MOCK_BUILDS.map((d) => relative(ROOT, d)).join(' / ')}）不一致。` +
      '去掉 --base 让 qa:shots 自己在空闲端口上起服务',
  )
}

interface Server {
  base: string
  child: ChildProcess | null
}

/**
 * 准备页面服务。`--base` 时核对后复用（不起服务）；否则自己起一个：默认先以 `--mode mock` 做一次生产构建再
 * `vite preview`（React 生产包、压缩后的代码，截图与流畅度数据都和真实 WebView 加载的产物一致），`--dev` 改用 dev
 * server（可配合 HMR 迭代）。端口默认由系统分配（`--port` 可指定），只监听 127.0.0.1，从不隐式复用别的进程的服务——
 * 多个工作区同时跑 qa:shots / 浏览器级用例 / `pnpm dev` 互不干扰。直接用 node 运行 vite，结束时能可靠地停掉它。
 */
async function startServer(opts: { dev: boolean; port: number; base?: string }): Promise<Server> {
  if (opts.base) {
    const base = opts.base.replace(/\/+$/, '')
    info(`复用页面服务 ${base}（${await verifyExternalServer(base)}）`)
    return { base, child: null }
  }
  // vite 的 exports 没有导出 bin，从 package.json 的位置推出
  const viteBin = join(
    dirname(createRequire(join(APP_DIR, 'package.json')).resolve('vite/package.json')),
    'bin',
    'vite.js',
  )
  let args = [viteBin, '--mode', 'mock']
  if (!opts.dev) {
    const outDir = join(ROOT, 'target', 'qa', 'dist')
    const built = spawnSync(
      process.execPath,
      [
        viteBin,
        'build',
        '--mode',
        'mock',
        '--outDir',
        outDir,
        '--emptyOutDir',
        '--logLevel',
        'warn',
      ],
      { cwd: APP_DIR, stdio: 'inherit' },
    )
    if (built.status !== 0) throw new Error('vite build --mode mock 失败')
    args = [viteBin, 'preview', '--mode', 'mock', '--outDir', outDir]
  }
  for (let attempt = 1; ; attempt++) {
    const port = opts.port || (await freePort())
    const base = `http://${HOST}:${port}`
    const child = spawn(
      process.execPath,
      [...args, '--host', HOST, '--port', String(port), '--strictPort'],
      {
        cwd: APP_DIR,
        stdio: ['ignore', 'pipe', 'pipe'],
        env: { ...process.env, BROWSER: 'none' },
      },
    )
    let log = ''
    child.stdout?.on('data', (d: Buffer) => (log += d.toString()))
    child.stderr?.on('data', (d: Buffer) => (log += d.toString()))
    const deadline = Date.now() + 60_000
    let exited = false
    while (Date.now() < deadline) {
      if (child.exitCode !== null) {
        exited = true
        break
      }
      if (await reachable(`${base}/panel.html`)) {
        info(`页面服务 ${base}（${opts.dev ? 'vite dev' : 'vite preview，--mode mock 生产构建'}）`)
        return { base, child }
      }
      await new Promise((r) => setTimeout(r, 300))
    }
    stopServer(child)
    // 系统分配的端口在探测与 vite 监听之间被别的进程占用：换一个端口再试
    const taken = /already in use|EADDRINUSE/i.test(log)
    if (exited && taken && !opts.port && attempt < PORT_ATTEMPTS) {
      warn(`端口 ${port} 刚被占用，换一个端口重试（${attempt}/${PORT_ATTEMPTS}）`)
      continue
    }
    if (exited && taken)
      throw new Error(`端口 ${port} 已被占用（换一个 --port，或去掉它由系统分配）`)
    throw new Error(
      `vite ${exited ? '启动失败' : '60 s 内没有就绪'}（${base}）：\n${log.slice(-2000)}`,
    )
  }
}

function stopServer(child: ChildProcess | null): void {
  if (!child || child.exitCode !== null) return
  child.stdout?.destroy()
  child.stderr?.destroy()
  child.kill('SIGTERM')
}

async function listScenarios(browser: Browser): Promise<ScenarioInfo[]> {
  const ctx = await browser.newContext({ viewport: VIEWPORT })
  const page = await ctx.newPage()
  try {
    await page.goto(`${BASE}/panel.html?hud=0`, { waitUntil: 'load' })
    // 注意：`__WISP_MOCK__` 同时是 Vite 的 define 常量，dev 模式下 window.__WISP_MOCK__ 先是 true，mock 装好后才变成句柄
    await page.waitForFunction(
      "typeof window.__WISP_MOCK__?.listScenarios === 'function'",
      undefined,
      {
        timeout: 30_000,
      },
    )
    return await page.evaluate<ScenarioInfo[]>(
      '(async () => (await window.__WISP_MOCK__.listScenarios()).map(({ id, label, view, motion, url }) => ({ id, label, view, motion: !!motion, url })))()',
    )
  } finally {
    await ctx.close()
  }
}

/** 记下页面的 console.error（断言失败时附上最后一条，说明是哪条断言） */
function collectConsoleErrors(page: Page): string[] {
  const errors: string[] = []
  page.on('console', (msg) => {
    if (msg.type() === 'error') errors.push(msg.text())
  })
  return errors
}

/**
 * 等场景到达 `<html data-settled>`。场景自带的断言（`data-qa-assert`）判定 `fail` 时立即报错（附上页面最后一条
 * console.error），不再干等到超时；到达 data-settled 之后再核对一次（断言失败但仍 settled 的场景同样拦下）
 */
async function waitSettled(page: Page, timeout: number, consoleErrors: string[]): Promise<void> {
  await page.waitForFunction(SETTLED_OR_FAILED, undefined, { timeout, polling: 100 })
  const verdict = await page.evaluate<string | null>(
    'document.documentElement.dataset.qaAssert ?? null',
  )
  if (verdict === 'fail') {
    const why = consoleErrors.at(-1)
    throw new Error(`场景断言失败（data-qa-assert="fail"）${why ? `：${why}` : ''}`)
  }
}

/** 记下页面在途的网络请求数（截图前的静止判定用；页面加载前调用） */
function trackRequests(page: Page): () => number {
  let inflight = 0
  page.on('request', () => inflight++)
  page.on('requestfinished', () => inflight--)
  page.on('requestfailed', () => inflight--)
  return () => inflight
}

/**
 * data-settled 之后等画面静止（见 QUIET_PROBE）：没有在途请求、字体已加载、DOM 连续 QUIET_MS 没有变化。
 * 页面要先经 addInitScript 装上 QUIET_PROBE。返回 false 表示 QUIET_TIMEOUT 内没有静止
 */
async function waitQuiet(page: Page, inflight: () => number): Promise<boolean> {
  const deadline = Date.now() + QUIET_TIMEOUT
  for (;;) {
    const [age, fonts] = await page.evaluate<[number, boolean]>(QUIET_STATE)
    if (inflight() <= 0 && fonts && age >= QUIET_MS) return true
    if (Date.now() >= deadline) return false
    // 离静止还差多少就等多少（至少 50 ms，至多 200 ms，期间的新变化会让下一轮重新计时）
    await new Promise((r) => setTimeout(r, Math.min(200, Math.max(50, QUIET_MS - age))))
  }
}

/**
 * 等 contentProbe 的条件成立（见其说明）。返回 true 表示确实等过——调用方要重新等一次画面静止；QUIET_TIMEOUT 内
 * 仍不成立时抛错（调用方据此不写入这张图）
 */
async function waitContent(page: Page, probe: string): Promise<boolean> {
  const deadline = Date.now() + QUIET_TIMEOUT
  let problem = await page.evaluate<string>(probe)
  if (!problem) return false
  for (;;) {
    if (Date.now() >= deadline) {
      throw new Error(
        `截图前${problem}：data-settled 与画面静止之后又等了 ${QUIET_TIMEOUT / 1000} s，不写入`,
      )
    }
    await new Promise((r) => setTimeout(r, 100))
    problem = await page.evaluate<string>(probe)
    if (!problem) return true
  }
}

function shotName(s: ScenarioInfo, theme: string, os: string, reduced: boolean): string {
  return `${s.id}__${theme}__${os}${reduced ? '__reduced' : ''}`
}

/** 在有限并发下依次执行任务 */
async function pool<T>(items: T[], n: number, fn: (item: T) => Promise<void>): Promise<void> {
  let next = 0
  const workers = Array.from({ length: Math.min(n, items.length) }, async () => {
    while (next < items.length) await fn(items[next++] as T)
  })
  await Promise.all(workers)
}

async function capture(browser: Browser, shot: Shot, lang: string, outDir: string): Promise<void> {
  const { scenario, theme, os, reduced } = shot
  const recordVideo = scenario.motion && !reduced
  const videoDir = join(outDir, 'video', shot.engine)
  const ctx = await browser.newContext({
    viewport: VIEWPORT,
    deviceScaleFactor: 2,
    colorScheme: theme,
    reducedMotion: reduced ? 'reduce' : 'no-preference',
    ...(recordVideo ? { recordVideo: { dir: videoDir, size: VIEWPORT } } : {}),
  })
  const page = await ctx.newPage()
  const consoleErrors = collectConsoleErrors(page)
  const inflight = trackRequests(page)
  const name = shotName(scenario, theme, os, reduced)
  try {
    await page.addInitScript(QUIET_PROBE)
    const url = `${BASE}${scenario.url}&theme=${theme}&os=${os}&reduced=${reduced ? 1 : 0}&lang=${lang}&hud=0`
    await page.goto(url, { waitUntil: 'load' })
    if (recordVideo) {
      await page
        .waitForSelector('html[data-motion-mid]', { state: 'attached', timeout: SETTLE_TIMEOUT })
        .then(async () => {
          shot.mid = join(shot.engine, `${name}__mid.png`)
          await page.screenshot({ path: join(outDir, shot.mid), animations: 'allow' })
        })
        .catch(() => undefined) // 没有标记中间帧的动效场景只录视频
    }
    await waitSettled(page, SETTLE_TIMEOUT, consoleErrors)
    let quiet = await waitQuiet(page, inflight)
    const probe = contentProbe(scenario.id, false)
    if (probe && (await waitContent(page, probe))) {
      shot.late = true
      quiet = await waitQuiet(page, inflight)
    }
    if (!quiet) shot.unsettled = true
    await page.screenshot({
      path: join(outDir, shot.file),
      fullPage: true,
      animations: 'allow',
      timeout: SHOT_TIMEOUT,
    })
  } catch (e) {
    shot.error = e instanceof Error ? e.message.split('\n')[0] : String(e)
  } finally {
    const video = page.video()
    await ctx.close()
    if (video) {
      // Playwright 用随机文件名，改成与截图同名，汇总页与人工查找都方便
      shot.video = join('video', shot.engine, `${name}.webm`)
      renameSync(await video.path(), join(outDir, shot.video))
    }
  }
}

/** Chrome trace 事件里用到的字段 */
interface TraceEvent {
  ph?: string
  name?: string
  cat?: string
  pid?: number
  tid?: number
  ts?: number
  dur?: number
  args?: { name?: string }
}

interface PerfResult {
  scenario: string
  engine: string
  ok: boolean
  detail: string
  /** 场景缺失、按 --allow-missing-perf 跳过 */
  skipped?: boolean
  /** 不达标的原因是场景本身（采样没有覆盖到负载、trace 缺标记）而不是机器快慢：虚拟机上也不放宽 */
  structural?: boolean
  /** 在虚拟机上不达标、按 virtualMachine() 的说明只告警（ok 仍是测量结果） */
  waived?: boolean
}

/**
 * 是否在虚拟机上运行，返回机型（不是虚拟机时为 null）。CI 的 qa-shots 跑在 GitHub 的 macos-15 runner 上——Apple 虚拟机
 * （`hw.model` = VirtualMac2,1）：没有 GPU、3 个共享 vCPU、软件合成。流畅度阈值（Chromium 不允许 > 8 ms 的主线程任务、
 * WebKit 帧间隔 p95 ≤ 19.5 ms）是按真机定的，在 runner 上四个场景两种引擎 7 / 8 项不达标、开着 trace 的
 * translate.stress-stream 60 s 内到不了 data-settled，同一提交在真机上 webkit 4 / 4、chromium 3 / 4 通过（chromium
 * overlay.drag 2 个 9.5 ms 的任务，负载约 5）。与冒烟门禁对 Apple 虚拟机的处理一致（smoke-gate.mjs 的 isAppleVm）：
 * 虚拟机上流畅度不达标与 data-settled 超时只告警（等 data-settled 的时限放宽到 PERF_SETTLE_TIMEOUT_VM），结果照常
 * 写进汇总页与 trace；**必需场景缺失、采样没有覆盖到场景负载仍然失败**，截图矩阵的判定不变。流畅度以真机为准（本机
 * `pnpm qa:shots --perf`）。Linux 的 Hyper-V 虚拟机（DMI 的 product_name = "Virtual Machine"）同样处理
 */
function virtualMachine(): string | null {
  if (process.platform === 'darwin') {
    const model = spawnSync('sysctl', ['-n', 'hw.model'], { encoding: 'utf8' }).stdout?.trim() ?? ''
    return model.startsWith('VirtualMac') ? model : null
  }
  if (process.platform === 'linux') {
    try {
      const product = readFileSync('/sys/devices/virtual/dmi/id/product_name', 'utf8').trim()
      return product === 'Virtual Machine' ? product : null
    } catch {
      return null
    }
  }
  return null
}

/** 最近秩分位数（`sorted` 已升序且非空） */
function nearestRank(sorted: number[], p: number): number {
  return sorted[Math.min(sorted.length, Math.max(1, Math.ceil(p * sorted.length))) - 1] as number
}

/**
 * Chromium：devtools.timeline trace 中渲染主线程上 > 8 ms 的任务。trace 在导航之前开始，只统计 `qa-start` 与
 * `qa-settled` 两个 User Timing 标记之间开始的任务（见 PERF_PROBE）；这段时间里主线程任务太少（场景在首帧之前
 * 就结束了）判为失败，与 WebKit「帧样本太少」同一口径
 */
async function perfChromium(
  browser: Browser,
  s: ScenarioInfo,
  lang: string,
  outDir: string,
  settleTimeout: number,
): Promise<PerfResult> {
  const ctx = await browser.newContext({ viewport: VIEWPORT, deviceScaleFactor: 2 })
  const page = await ctx.newPage()
  const consoleErrors = collectConsoleErrors(page)
  const fail = (detail: string, structural = false): PerfResult => ({
    scenario: s.id,
    engine: 'chromium',
    ok: false,
    detail,
    ...(structural ? { structural } : {}),
  })
  try {
    await page.addInitScript(PERF_PROBE)
    await browser.startTracing?.(page, {
      categories: [
        'devtools.timeline',
        'disabled-by-default-devtools.timeline',
        'blink.user_timing',
      ],
    })
    await page.goto(`${BASE}${s.url}&lang=${lang}&hud=0`, { waitUntil: 'load' })
    try {
      await waitSettled(page, settleTimeout, consoleErrors)
    } catch (e) {
      await browser.stopTracing?.()
      return fail(e instanceof Error ? (e.message.split('\n')[0] ?? e.message) : String(e))
    }
    // 等 qa-settled 标记写进 trace（MutationObserver 回调在 data-settled 之后的微任务里）
    await page.waitForFunction(SETTLED_MARK, undefined, { timeout: 5_000 })
    const buf = (await browser.stopTracing?.()) ?? Buffer.from('{"traceEvents":[]}')
    writeFileSync(join(outDir, 'perf', `${s.id}.chromium.trace.json`), buf)
    const events = (JSON.parse(buf.toString()).traceEvents ?? []) as TraceEvent[]
    const markTs = (name: string) =>
      events.find((e) => e.name === name && (e.cat ?? '').includes('blink.user_timing'))?.ts
    const from = markTs('qa-start')
    const to = markTs('qa-settled')
    if (from === undefined || to === undefined) {
      return fail('trace 里没有 qa-start / qa-settled 标记（页面没有到达 data-settled）', true)
    }
    const mainTids = new Set(
      events
        .filter(
          (e) => e.ph === 'M' && e.name === 'thread_name' && e.args?.name === 'CrRendererMain',
        )
        .map((e) => `${e.pid}:${e.tid}`),
    )
    // 在 [qa-start, qa-settled] 之间开始的主线程任务
    const tasks = events.filter(
      (e) =>
        e.ph === 'X' &&
        e.name === 'RunTask' &&
        mainTids.has(`${e.pid}:${e.tid}`) &&
        (e.ts ?? 0) >= from &&
        (e.ts ?? 0) <= to,
    )
    const span = `${((to - from) / 1000).toFixed(0)} ms`
    if (tasks.length < MIN_PERF_SAMPLES) {
      return fail(
        `trace 没有覆盖到场景负载：首帧之后到 data-settled（${span}）只有 ${tasks.length} 个主线程任务（至少 ${MIN_PERF_SAMPLES} 个；负载应在 load 之后开始）`,
        true,
      )
    }
    const long = tasks.filter((e) => (e.dur ?? 0) > MAX_TASK_MS * 1000)
    const worst = long.reduce((m, e) => Math.max(m, (e.dur ?? 0) / 1000), 0)
    return {
      scenario: s.id,
      engine: 'chromium',
      ok: long.length === 0,
      detail: long.length
        ? `${long.length} 个主线程任务 > ${MAX_TASK_MS} ms（最长 ${worst.toFixed(1)} ms；${tasks.length} 个任务，${span}）`
        : `没有 > ${MAX_TASK_MS} ms 的任务（${tasks.length} 个任务，${span}）`,
    }
  } finally {
    await ctx.close()
  }
}

/**
 * 同一个 WebKit 实例在空白页上的中位帧间隔（ms）：流畅度判定的容差以它为基准，而不是被测场景自己的帧间隔——
 * 否则场景每帧都卡（例如整体降到 30 fps）时中位数跟着变大，容差也跟着放宽，持续掉帧反而测不出来
 */
async function idleFrameInterval(browser: Browser): Promise<number> {
  const ctx = await browser.newContext({ viewport: VIEWPORT, deviceScaleFactor: 2 })
  const page = await ctx.newPage()
  try {
    await page.goto('about:blank')
    const frames = await page.evaluate<number[]>(`new Promise((resolve) => {
      const t = []; let end = 0
      const tick = (now) => {
        if (!end) end = now + 1200
        t.push(now)
        if (now < end) requestAnimationFrame(tick); else resolve(t)
      }
      requestAnimationFrame(tick)
    })`)
    const gaps = frames
      .slice(1)
      .map((t, i) => t - (frames[i] as number))
      .toSorted((a, b) => a - b)
    if (gaps.length < MIN_PERF_SAMPLES) throw new Error(`WebKit 空闲帧样本太少（${gaps.length}）`)
    return nearestRank(gaps, 0.5)
  } finally {
    await ctx.close()
  }
}

/**
 * WebKit：页面内 rAF 采样（PERF_PROBE），首帧之后到 data-settled 的帧间隔 p95 ≤ max(17 ms, 空闲中位帧间隔 × 1.15)。
 * `idle` 是 idleFrameInterval 在同一浏览器实例上测得的空闲中位帧间隔
 */
async function perfWebkit(
  browser: Browser,
  s: ScenarioInfo,
  lang: string,
  idle: number,
  settleTimeout: number,
): Promise<PerfResult> {
  const ctx = await browser.newContext({ viewport: VIEWPORT, deviceScaleFactor: 2 })
  const page = await ctx.newPage()
  const consoleErrors = collectConsoleErrors(page)
  try {
    await page.addInitScript(PERF_PROBE)
    await page.goto(`${BASE}${s.url}&lang=${lang}&hud=0`, { waitUntil: 'load' })
    try {
      await waitSettled(page, settleTimeout, consoleErrors)
    } catch (e) {
      const detail = e instanceof Error ? (e.message.split('\n')[0] ?? e.message) : String(e)
      return { scenario: s.id, engine: 'webkit', ok: false, detail }
    }
    await page.waitForFunction(SETTLED_MARK, undefined, { timeout: 5_000 })
    const frames = await page.evaluate<number[]>('window.__qaFrames || []')
    const gaps = frames
      .slice(1)
      .map((t, i) => t - (frames[i] as number))
      .toSorted((a, b) => a - b)
    if (gaps.length < MIN_PERF_SAMPLES) {
      return {
        scenario: s.id,
        engine: 'webkit',
        ok: false,
        structural: true,
        detail: `帧样本太少（${gaps.length}，至少 ${MIN_PERF_SAMPLES}）：采样没有覆盖到场景负载（负载应在 load 之后开始）`,
      }
    }
    const p95 = nearestRank(gaps, 0.95)
    const limit = Math.max(MAX_FRAME_P95_MS, idle * FRAME_JITTER)
    const dropped = gaps.filter((g) => g > idle * DROP_FACTOR).length
    return {
      scenario: s.id,
      engine: 'webkit',
      ok: p95 <= limit,
      detail: `帧间隔 p95 ${p95.toFixed(1)} ms、中位 ${nearestRank(gaps, 0.5).toFixed(1)} ms（上限 ${limit.toFixed(1)} ms = max(${MAX_FRAME_P95_MS}, 空闲 ${idle.toFixed(1)} × ${FRAME_JITTER})）；掉帧 ${dropped}/${gaps.length}（> ${DROP_FACTOR}× 空闲帧间隔）`,
    }
  } finally {
    await ctx.close()
  }
}

function escapeHtml(s: string): string {
  return s.replace(
    /[&<>"]/g,
    (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c] as string,
  )
}

/** 汇总页：每个场景一节，按引擎 × 主题 × 平台 × 动效排成缩略图网格 */
function writeIndex(
  outDir: string,
  shots: Shot[],
  perf: PerfResult[],
  meta: string,
  calibration: string,
): void {
  const byScenario = new Map<string, Shot[]>()
  for (const s of shots)
    byScenario.set(s.scenario.id, [...(byScenario.get(s.scenario.id) ?? []), s])
  const cell = (s: Shot) => {
    const cap = `${s.engine} · ${s.theme} · ${s.os}${s.reduced ? ' · 减少动效' : ''}${s.unsettled ? ` · ${QUIET_TIMEOUT / 1000} s 内画面未静止` : ''}`
    if (s.error)
      return `<figure class="err"><figcaption>${cap}</figcaption><p>${escapeHtml(s.error)}</p></figure>`
    const extra = [
      s.mid ? `<a href="${s.mid}">中间帧</a>` : '',
      s.video ? `<a href="${s.video}">视频</a>` : '',
    ].filter(Boolean)
    return `<figure><a href="${s.file}"><img loading="lazy" src="${s.file}" alt=""></a><figcaption>${cap}${extra.length ? ` · ${extra.join(' · ')}` : ''}</figcaption></figure>`
  }
  const sections = [...byScenario.entries()]
    .map(
      ([id, list]) =>
        `<section><h2>${escapeHtml(id)} <small>${escapeHtml(list[0]?.scenario.label ?? '')}</small></h2><div class="grid">${list.map(cell).join('')}</div></section>`,
    )
    .join('\n')
  const perfRows = perf
    .map(
      (p) =>
        `<tr class="${p.ok ? '' : p.waived ? 'waived' : 'bad'}"><td>${escapeHtml(p.scenario)}</td><td>${p.engine}</td><td>${escapeHtml(p.detail)}${p.waived ? '（虚拟机：只告警）' : ''}</td></tr>`,
    )
    .join('')
  const html = `<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Wisp 视觉 QA</title>
<style>
:root { color-scheme: light dark; --bg: #f6f6f7; --fg: #1d1d1f; --muted: #6e6e73; --card: #fff; --bad: #c62828; }
@media (prefers-color-scheme: dark) { :root { --bg: #141416; --fg: #f2f2f4; --muted: #9a9aa0; --card: #1f1f23; --bad: #ff6b6b; } }
body { margin: 0; padding: 24px 16px; font: 14px/1.5 system-ui, -apple-system, "PingFang SC", sans-serif; background: var(--bg); color: var(--fg); }
h1 { margin: 0 0 4px; font-size: 20px; } h2 { font-size: 16px; margin: 32px 0 12px; } small { color: var(--muted); font-weight: 400; }
.meta { color: var(--muted); margin-bottom: 16px; }
.grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(260px, 1fr)); gap: 12px; }
figure { margin: 0; background: var(--card); border-radius: 10px; padding: 8px; box-shadow: 0 1px 2px rgb(0 0 0 / .08); }
figure img { width: 100%; height: auto; border-radius: 6px; display: block; }
figcaption { color: var(--muted); font-size: 12px; margin-top: 6px; }
figure.err { outline: 2px solid var(--bad); } figure.err p { color: var(--bad); font-size: 12px; }
table { border-collapse: collapse; } td { padding: 4px 12px 4px 0; } tr.bad td { color: var(--bad); }
</style></head><body>
<h1>Wisp 视觉 QA</h1><div class="meta">${escapeHtml(meta)}</div>
${perf.length ? `<h2>流畅度</h2><p class="meta">${escapeHtml(calibration)}</p><table>${perfRows}</table>` : ''}
${sections || '<p>没有场景（在 src/shared/ipc/mock/scenarios/*.ts 中添加）。</p>'}
</body></html>
`
  writeFileSync(join(outDir, 'index.html'), html)
}

/**
 * 在浏览器里把 PNG 高质量缩小到 width × height（canvas 的 imageSmoothingQuality = high）。README 截图按 2× 渲染后用它
 * 缩到输出宽度——与手工制作时「2× 截图再缩放」同一做法，只是不依赖 sips 之类的平台工具
 */
async function resamplePng(
  browser: Browser,
  png: Buffer,
  width: number,
  height: number,
): Promise<Buffer> {
  const ctx = await browser.newContext({
    viewport: { width: 64, height: 64 },
    deviceScaleFactor: 1,
  })
  const page = await ctx.newPage()
  try {
    const out = await page.evaluate<string>(`(async () => {
      const img = new Image()
      img.src = 'data:image/png;base64,${png.toString('base64')}'
      await img.decode()
      const c = document.createElement('canvas')
      c.width = ${width}
      c.height = ${height}
      const g = c.getContext('2d')
      g.imageSmoothingEnabled = true
      g.imageSmoothingQuality = 'high'
      g.drawImage(img, 0, 0, ${width}, ${height})
      return c.toDataURL('image/png').slice('data:image/png;base64,'.length)
    })()`)
    return Buffer.from(out, 'base64')
  } finally {
    await ctx.close()
  }
}

/** README 截图：一张（等 data-settled 与画面静止后按裁切框以 2× 截取、缩小到输出宽度，先写临时文件再改名） */
async function captureReadme(browser: Browser, shot: ReadmeShot, url: string, outDir: string) {
  const ctx = await browser.newContext({
    viewport: shot.viewport,
    deviceScaleFactor: RENDER_SCALE,
    colorScheme: shot.theme,
    reducedMotion: 'no-preference',
  })
  const page = await ctx.newPage()
  const consoleErrors = collectConsoleErrors(page)
  const inflight = trackRequests(page)
  const tmp = join(outDir, `.${shot.file}.tmp`)
  try {
    await page.addInitScript(QUIET_PROBE)
    await page.goto(`${BASE}${url}&theme=${shot.theme}&os=macos&reduced=0&lang=zh-CN&hud=0`, {
      waitUntil: 'load',
    })
    await waitSettled(page, SETTLE_TIMEOUT, consoleErrors)
    // README 的图要可复现：画面一直在变就不写入（不像矩阵那样照常截取）
    if (!(await waitQuiet(page, inflight)))
      throw new Error(`data-settled 之后 ${QUIET_TIMEOUT / 1000} s 内画面没有静止`)
    if (shot.hold) {
      await new Promise((r) => setTimeout(r, shot.hold))
      if (!(await waitQuiet(page, inflight)))
        throw new Error(`等待 ${shot.hold} ms 之后 ${QUIET_TIMEOUT / 1000} s 内画面没有静止`)
    }
    // 静止判定之外的正向条件：没有骨架 / aria-busy（设置页还要求窗口已渲染）；等过的话重新等静止
    const probe = contentProbe(shot.scenario, true) as string
    if ((await waitContent(page, probe)) && !(await waitQuiet(page, inflight)))
      throw new Error(`骨架消失之后 ${QUIET_TIMEOUT / 1000} s 内画面没有静止`)
    if (consoleErrors.length) throw new Error(`页面报错：${consoleErrors.at(-1)}`)
    // 无限循环的 CSS 动画回到初始状态、有限的快进到结束（插入光标默认隐藏）
    const raw = (await page.screenshot({
      clip: shot.clip,
      animations: 'disabled',
      type: 'png',
    })) as Buffer
    const want = expectedSize(shot)
    const buf =
      want.width === shot.clip.width * RENDER_SCALE
        ? raw
        : await resamplePng(browser, raw, want.width, want.height)
    const got = pngSize(buf)
    if (!got || got.width !== want.width || got.height !== want.height) {
      throw new Error(
        `尺寸 ${got ? `${got.width}×${got.height}` : '无法读取'}，应为 ${want.width}×${want.height}`,
      )
    }
    if (buf.length > MAX_BYTES) {
      throw new Error(
        `${(buf.length / 1024).toFixed(0)} KB，超过单张上限 ${MAX_BYTES / 1024} KB（收窄裁切框或换内容更少的场景）`,
      )
    }
    writeFileSync(tmp, buf)
    renameSync(tmp, join(outDir, shot.file))
    return { bytes: buf.length, size: got }
  } finally {
    rmSync(tmp, { force: true })
    await ctx.close()
  }
}

/**
 * `--readme`：按 readme-shots.ts 的清单重新生成 README 截图（默认直接写进 docs/images/，只覆盖清单里的文件与图标），
 * 再核对 README 的引用：引用了却不存在、不由清单生成、清单生成了却没被引用的文件都报出来（前两种是错误）
 */
async function readmeMain(opts: {
  outDir: string
  filters: string[]
  list: boolean
  dev: boolean
  port: number
  base?: string
}): Promise<void> {
  const problems = validateShots(README_SHOTS)
  if (problems.length) throw new Error(`readme-shots.ts 的清单有误：\n${problems.join('\n')}`)
  const shots = opts.filters.length
    ? README_SHOTS.filter((s) =>
        opts.filters.some((f) => s.file.includes(f) || s.scenario.includes(f)),
      )
    : README_SHOTS
  if (shots.length === 0) throw new Error('--filter 没有匹配 README 清单里的任何文件或场景')
  if (opts.list) {
    for (const s of shots) {
      const { width, height } = expectedSize(s)
      const c = s.clip
      console.log(
        `${s.file.padEnd(28)} ${s.scenario.padEnd(20)} ${s.theme.padEnd(5)} 视口 ${s.viewport.width}×${s.viewport.height}  裁切 ${c.x},${c.y} ${c.width}×${c.height}  → ${width}×${height}`,
      )
    }
    return
  }
  mkdirSync(opts.outDir, { recursive: true })
  const pw = loadPlaywright()
  const server = await startServer({ dev: opts.dev, port: opts.port, base: opts.base })
  BASE = server.base
  const failed: string[] = []
  try {
    const browser = await pw.webkit.launch()
    try {
      const scenarios = new Map((await listScenarios(browser)).map((s) => [s.id, s]))
      const absent = [...new Set(shots.map((s) => s.scenario))].filter((id) => !scenarios.has(id))
      if (absent.length)
        throw new Error(`README 清单里的场景不存在（改名了？）：${absent.join('、')}`)
      // 等画面静止之后才截，并发不再影响结果；仍限并发，避免机器负载拖慢到超时
      await pool([...shots], CONCURRENCY, async (shot) => {
        const scenario = scenarios.get(shot.scenario) as ScenarioInfo
        try {
          const r = await captureReadme(browser, shot, scenario.url, opts.outDir)
          ok(
            `${shot.file}（${shot.scenario}，${shot.theme}）${r.size.width}×${r.size.height}，${(r.bytes / 1024).toFixed(0)} KB`,
          )
        } catch (e) {
          failed.push(shot.file)
          error(
            `${shot.file}（${shot.scenario}，${shot.theme}）：${e instanceof Error ? e.message.split('\n')[0] : String(e)}`,
          )
        }
      })
    } finally {
      await browser.close()
    }
  } finally {
    stopServer(server.child)
  }
  const icon = join(ROOT, README_ICON.from)
  const iconOut = join(opts.outDir, README_ICON.file)
  if (!existsSync(iconOut) || !readFileSync(iconOut).equals(readFileSync(icon))) {
    copyFileSync(icon, iconOut)
    ok(`${README_ICON.file}（复制自 ${README_ICON.from}）`)
  }
  if (failed.length)
    throw new Error(`${failed.length} 张 README 截图失败，没有写入：${failed.join('、')}`)
  // README 引用的是 docs/images/：--out 指到别处（先预览）时仍按仓库里的 docs/images/ 核对
  const imagesDir = join(ROOT, 'docs', 'images')
  const readme = readFileSync(join(ROOT, 'README.md'), 'utf8')
  const existing = new Set(existsSync(imagesDir) ? readdirSync(imagesDir) : [])
  const refs = checkReadmeRefs(readme, README_SHOTS, existing)
  for (const f of refs.unreferenced) warn(`README.md 没有引用 ${f}`)
  const referenced = new Set(readmeImageRefs(readme))
  const stale = [...existing].filter(
    (f) =>
      f.endsWith('.png') &&
      f !== README_ICON.file &&
      !README_SHOTS.some((s) => s.file === f) &&
      !referenced.has(f),
  )
  if (stale.length)
    warn(`docs/images/ 里有清单之外、README 也没有引用的图片（可以删除）：${stale.join('、')}`)
  if (refs.missing.length || refs.unlisted.length) {
    throw new Error(
      [
        refs.missing.length ? `README.md 引用了不存在的图片：${refs.missing.join('、')}` : '',
        refs.unlisted.length
          ? `README.md 引用的图片不由 readme-shots.ts 生成（加进清单，或改引用）：${refs.unlisted.join('、')}`
          : '',
      ]
        .filter(Boolean)
        .join('；'),
    )
  }
  ok(
    `README 截图完成：${shots.length} 张写入 ${relative(ROOT, opts.outDir) || '.'}，README.md 的图片引用核对通过`,
  )
}

async function main(): Promise<void> {
  const { values } = parseArgs({
    options: {
      engine: { type: 'string' },
      filter: { type: 'string' },
      quick: { type: 'boolean', default: false },
      lang: { type: 'string' },
      perf: { type: 'boolean', default: false },
      'allow-missing-perf': { type: 'boolean', default: false },
      out: { type: 'string' },
      list: { type: 'boolean', default: false },
      readme: { type: 'boolean', default: false },
      dev: { type: 'boolean', default: false },
      port: { type: 'string' },
      base: { type: 'string' },
      help: { type: 'boolean', short: 'h', default: false },
    },
  })
  if (values.help) {
    console.log(
      '用法：pnpm qa:shots [--engine webkit|chromium|all] [--filter 子串[,子串…]] [--quick] [--lang zh-CN|en-US|pseudo] [--perf [--allow-missing-perf]] [--out 目录] [--list] [--dev] [--port n | --base url]\n' +
        '      pnpm qa:shots --readme [--filter 子串[,子串…]] [--out 目录] [--list] [--dev] [--port n | --base url]',
    )
    return
  }
  // 0 = 由系统分配空闲端口
  const port = values.port === undefined ? 0 : Number(values.port)
  if (values.port !== undefined && !(Number.isInteger(port) && port >= 1 && port <= 65535)) {
    throw new Error('--port 必须是 1–65535 的整数')
  }
  if (values.base !== undefined && (values.port !== undefined || values.dev)) {
    throw new Error('--base 复用已有的页面服务，不能与 --port / --dev 同时使用')
  }
  if (values.base !== undefined && !/^https?:\/\/[^/]+/.test(values.base)) {
    throw new Error('--base 应为 http(s)://主机:端口')
  }
  // --filter 支持逗号分隔的多个子串（例如 `--filter shell,design`）
  const filters = ((values.filter as string | undefined) ?? '')
    .split(',')
    .map((f) => f.trim())
    .filter(Boolean)
  const matchesFilter = (id: string) => filters.some((f) => id.includes(f))
  if (values.readme) {
    // README 档位的引擎、语言、平台外观与动效都是固定的（见 readme-shots.ts），不接受矩阵的选项
    const conflicts: string[] = []
    if (values.engine !== undefined && values.engine !== 'webkit') conflicts.push('--engine')
    if (values.lang !== undefined && values.lang !== 'zh-CN') conflicts.push('--lang')
    if (values.quick) conflicts.push('--quick')
    if (values.perf) conflicts.push('--perf')
    if (values['allow-missing-perf']) conflicts.push('--allow-missing-perf')
    if (conflicts.length)
      throw new Error(
        `--readme 固定 WebKit、zh-CN、macOS 外观，不能与 ${conflicts.join(' / ')} 同时使用`,
      )
    await readmeMain({
      outDir: values.out ?? join(ROOT, 'docs', 'images'),
      filters,
      list: values.list,
      dev: values.dev,
      port,
      base: values.base,
    })
    return
  }
  const lang = values.lang ?? 'zh-CN'
  const engineOpt = values.engine ?? 'all'
  const engines = engineOpt === 'all' ? ['webkit', 'chromium'] : [engineOpt]
  if (!engines.every((e) => e === 'webkit' || e === 'chromium'))
    throw new Error('--engine 只能是 webkit / chromium / all')
  const outDir = values.out ?? join(ROOT, 'artifacts', 'qa')
  const pw = loadPlaywright()
  const server = await startServer({ dev: values.dev, port, base: values.base })
  BASE = server.base
  const shots: Shot[] = []
  const perf: PerfResult[] = []
  const calibration: string[] = []
  const started = Date.now()
  try {
    if (!values.list) {
      rmSync(outDir, { recursive: true, force: true })
      mkdirSync(join(outDir, 'perf'), { recursive: true })
    }
    for (const engine of engines) {
      const browser = await pw[engine as 'webkit' | 'chromium'].launch()
      try {
        let scenarios = await listScenarios(browser)
        if (filters.length > 0) {
          scenarios = scenarios.filter((s) => matchesFilter(s.id))
          // 写错过滤条件时不能以「0 张截图、通过」收场
          if (scenarios.length === 0 && !values.list)
            throw new Error(
              `--filter ${values.filter} 没有匹配任何场景（用 --list 查看全部场景 id）`,
            )
        }
        if (values.list) {
          for (const s of scenarios)
            console.log(
              `${s.id.padEnd(36)} ${s.view.padEnd(10)} ${s.motion ? '动效' : ''}  ${s.label}`,
            )
          return
        }
        if (scenarios.length === 0)
          warn(`${engine}：没有场景（在 src/shared/ipc/mock/scenarios/*.ts 中添加）`)
        mkdirSync(join(outDir, engine), { recursive: true })
        const jobs: Shot[] = []
        for (const scenario of scenarios) {
          for (const theme of values.quick ? (['light'] as const) : (['light', 'dark'] as const)) {
            for (const os of values.quick
              ? (['macos'] as const)
              : (['macos', 'windows'] as const)) {
              for (const reduced of values.quick ? [false] : [false, true]) {
                const file = join(engine, `${shotName(scenario, theme, os, reduced)}.png`)
                jobs.push({ scenario, engine, theme, os, reduced, file })
              }
            }
          }
        }
        await pool(jobs, CONCURRENCY, (shot) => capture(browser, shot, lang, outDir))
        shots.push(...jobs)
        info(`${engine}：${jobs.length} 张（${scenarios.length} 个场景）`)

        if (values.perf) {
          // 必需场景：PERF_SCENARIOS 的四个（--filter 时只要求与过滤条件匹配的那些）
          const required = PERF_SCENARIOS.filter((id) => filters.length === 0 || matchesFilter(id))
          const vm = virtualMachine()
          if (vm) {
            warn(
              `${engine}：在虚拟机上运行（${vm}）：流畅度不达标与 data-settled 超时只告警（时限 ${PERF_SETTLE_TIMEOUT_VM / 1000} s），场景缺失与采样没有覆盖到负载仍然失败；流畅度以真机为准`,
            )
            calibration.push(`虚拟机 ${vm}：流畅度只告警`)
          }
          const settleTimeout = vm ? PERF_SETTLE_TIMEOUT_VM : PERF_SETTLE_TIMEOUT
          let idle = 0
          if (engine === 'webkit') {
            idle = await idleFrameInterval(browser)
            calibration.push(`WebKit 空闲中位帧间隔 ${idle.toFixed(1)} ms`)
            info(`webkit：空闲中位帧间隔 ${idle.toFixed(1)} ms（流畅度容差的基准）`)
          }
          for (const id of required) {
            const s = scenarios.find((x) => x.id === id)
            if (!s) {
              const detail = '场景不存在（由对应视图在 mock/scenarios 中提供）'
              if (values['allow-missing-perf']) {
                warn(`${engine}：流畅度场景 ${id} 还不存在，按 --allow-missing-perf 跳过`)
                perf.push({
                  scenario: id,
                  engine,
                  ok: true,
                  skipped: true,
                  detail: `${detail}，已按 --allow-missing-perf 跳过`,
                })
              } else {
                error(`${engine} ${id}：${detail}（场景还没写好时可加 --allow-missing-perf）`)
                perf.push({ scenario: id, engine, ok: false, detail })
              }
              continue
            }
            const r =
              engine === 'chromium'
                ? await perfChromium(browser, s, lang, outDir, settleTimeout)
                : await perfWebkit(browser, s, lang, idle, settleTimeout)
            if (!r.ok && !r.structural && vm) r.waived = true
            perf.push(r)
            ;(r.ok ? ok : r.waived ? warn : error)(
              `${engine} ${id}：${r.detail}${r.waived ? '（虚拟机：只告警）' : ''}`,
            )
          }
        }
      } finally {
        await browser.close()
      }
    }
  } finally {
    stopServer(server.child)
  }
  const failed = shots.filter((s) => s.error)
  const meta = `${new Date().toISOString()} · ${engines.join(' + ')} · 语言 ${lang} · ${shots.length} 张 · ${((Date.now() - started) / 1000).toFixed(0)} s · ${BASE}`
  writeIndex(outDir, shots, perf, meta, calibration.join(' · '))
  for (const s of failed) error(`${s.engine} ${s.file}：${s.error}`)
  const unsettled = shots.filter((s) => s.unsettled && !s.error)
  if (unsettled.length) {
    warn(
      `${unsettled.length} 张截图在 data-settled 之后 ${QUIET_TIMEOUT / 1000} s 内画面没有静止（仍有 DOM 变化或在途请求），照常截取：` +
        unsettled
          .map((s) => `${s.engine}/${shotName(s.scenario, s.theme, s.os, s.reduced)}`)
          .join('、'),
    )
  }
  const late = shots.filter((s) => s.late && !s.error)
  if (late.length) {
    info(
      `${late.length} 张截图在画面静止之后骨架 / aria-busy 才消失，已多等再截（截到的是内容）：` +
        late.map((s) => `${s.engine}/${shotName(s.scenario, s.theme, s.os, s.reduced)}`).join('、'),
    )
  }
  info(`汇总页：${relative(ROOT, join(outDir, 'index.html'))}`)
  if (failed.length)
    throw new Error(
      `${failed.length} 张截图失败（场景断言失败、没有在 ${SETTLE_TIMEOUT / 1000} s 内到达 data-settled、页面报错，或设置页截图前仍是骨架 / 空白）`,
    )
  const skipped = perf.filter((p) => p.skipped).length
  const waived = perf.filter((p) => p.waived).length
  if (perf.some((p) => !p.ok && !p.waived))
    throw new Error('流畅度不达标、trace 没有覆盖到场景负载，或必需的流畅度场景缺失')
  ok(
    `视觉 QA 完成：${shots.length} 张截图${perf.length ? `，${perf.filter((p) => p.ok && !p.skipped).length} 项流畅度检查通过` : ''}${waived ? `，${waived} 项在虚拟机上不达标（只告警）` : ''}${skipped ? `，${skipped} 项流畅度场景缺失（已按 --allow-missing-perf 跳过）` : ''}`,
  )
}

await run(main)
