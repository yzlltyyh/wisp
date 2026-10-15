#!/usr/bin/env node
/**
 * scripts/perf/run-smoke.mjs：启动构建好的应用跑一次 WISP_SMOKE 冒烟。
 *
 * 用法：
 *   node scripts/perf/run-smoke.mjs [--profile debug|release] [--bin <可执行文件>] [--out target/smoke-report.json]
 *                                   [--timeout 300] [--webview]
 *
 * 以 `WISP_SMOKE=1 WISP_FAKE_LLM=1 WISP_SMOKE_REPORT=<out 的绝对路径>` 启动 `target/<profile>/wisp-desktop`
 * （需带 `--features fake-llm` 构建），工作目录为仓库根；Linux 没有 DISPLAY 时自动套 `xvfb-run -a`。
 * 应用跑完流程后写报告并退出（约定见 README「冒烟报告」：报告写成功就以 0 退出，流程失败记在报告的
 * ok / failures 里；冒烟日志复制到报告旁边的同名 .log）；超时未退出视为卡死。应用的标准输出写到报告目录的
 * smoke-run.log。判定由 smoke-gate.mjs 完成，本脚本只保证「进程正常退出且报告存在」。
 * 应用在自己的进程组里启动（Linux 上是 xvfb-run → Xvfb + wisp-desktop）：超时或本脚本被中断时先对整个进程组发
 * SIGTERM、3 秒后 SIGKILL，Windows 上用 taskkill /T 结束整棵进程树，不会留下孤儿 Xvfb、卡死的应用或 WebView 进程。
 *
 * WebView 内存（`--webview` 打开，默认关闭）：冒烟期间每秒采样一次属于应用的 WebView 进程内存合计，应用退出后写进
 * 报告的 `webview` 段（应用自己不写这一段，见 smoke-report.schema.json）：`hot_kb` 是冒烟会话后空闲（post_session_idle，
 * 窗口全部隐藏 / 关闭之后的那 10 s，跳过前 2 s 的收尾）里的中位数，`peak_kb` 是全程峰值。**只记录、不判定**：冒烟 20 s 内
 * 连续触发 100 次、开关设置窗口与 Overlay（与面板共用一个 WebContent 进程），读数随 GC 时机摆动，WebView 内存预算由
 * measure-macos.sh 的会话后测量判定（smoke-gate 的 --expect-state）。默认关闭是因为采样本身有开销（macOS 每秒启动一次
 * /usr/bin/python3，约 50–100 ms CPU），会扰动同一次冒烟里被门禁判定的首次翻译、连续触发与冷启动；需要分析时再打开。
 * macOS 按「负责进程」归属（macos-webkit.py --scan；从终端 / CI 直接执行时是近似归属：同一负责进程、启动不早于应用），
 * 口径 phys_footprint；Linux 取应用后代中名字以 WebKit 开头的进程，口径 VmRSS。Windows 不采样（WebView2 由
 * measure-windows.ps1 覆盖），报告没有 `webview` 段。
 */
import { execFile, spawn, spawnSync } from 'node:child_process'
import {
  createWriteStream,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { dirname, isAbsolute, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { parseArgs } from 'node:util'
import { promisify } from 'node:util'
import assert from 'node:assert/strict'
import { ROOT, info, isMain, ok, rel, run, warn } from '../lib/common.mjs'

const KILL_GRACE_MS = 3000
const WEBVIEW_INTERVAL_MS = 1000
/** 会话后空闲窗口里跳过的开头（窗口隐藏 / 销毁之后 WebKit 进程退出、内存回收需要一点时间） */
const HOT_SKIP_MS = 2000
const execFileAsync = promisify(execFile)
const HELPER = fileURLToPath(new URL('./macos-webkit.py', import.meta.url))

// ---------------------------------------------------------------------------
// WebView 采样

/** Linux：/proc 里的进程表（pid → { ppid, comm, rssKib }）；读不到的进程跳过 */
export function readProcTable(procDir = '/proc') {
  const table = new Map()
  for (const name of readdirSync(procDir)) {
    if (!/^\d+$/.test(name)) continue
    try {
      const stat = readFileSync(join(procDir, name, 'stat'), 'utf8')
      // comm 在括号里，可能含空格与括号：取最后一个 ')' 之后的字段
      const open = stat.indexOf('(')
      const close = stat.lastIndexOf(')')
      const comm = stat.slice(open + 1, close)
      const ppid = Number(stat.slice(close + 2).split(' ')[1])
      const status = readFileSync(join(procDir, name, 'status'), 'utf8')
      const rss = /^VmRSS:\s+(\d+)\s+kB/m.exec(status)
      table.set(Number(name), { ppid, comm, rssKib: rss ? Number(rss[1]) : 0 })
    } catch {
      // 进程刚好退出
    }
  }
  return table
}

/** root 的全部后代里名字以 WebKit 开头的进程：`{ kib, n }` */
export function linuxWebviews(table, root) {
  const children = new Map()
  for (const [pid, p] of table) {
    if (!children.has(p.ppid)) children.set(p.ppid, [])
    children.get(p.ppid).push(pid)
  }
  let kib = 0
  let n = 0
  const stack = [root]
  const seen = new Set()
  while (stack.length) {
    const cur = stack.pop()
    if (seen.has(cur)) continue
    seen.add(cur)
    for (const c of children.get(cur) ?? []) {
      const p = table.get(c)
      if (p && p.comm.startsWith('WebKit')) {
        kib += p.rssKib
        n++
      }
      stack.push(c)
    }
  }
  return { kib, n }
}

/** macOS：属于 pid 的 WebKit XPC 进程的 phys_footprint 合计（一次 python3 调用完成归属与取值）；python3 不可用时抛错 */
async function macWebviews(pid) {
  const { stdout } = await execFileAsync(
    '/usr/bin/python3',
    ['-I', HELPER, '--scan', String(pid)],
    {
      timeout: 5000,
    },
  )
  return parseScan(stdout)
}

/** `macos-webkit.py --scan` 的输出 → `{ kib, n, mode }` */
export function parseScan(stdout) {
  const [mode, , pids, fp] = stdout.trim().split(/\s+/)
  if (!pids || pids === '-' || !fp || fp === '-') return { kib: 0, n: 0, mode }
  let kib = 0
  let n = 0
  for (const entry of fp.split(',')) {
    const [, v] = entry.split(':')
    if (v !== undefined && Number.isFinite(Number(v))) {
      kib += Number(v)
      n++
    }
  }
  return { kib, n, mode }
}

/**
 * 每秒采样一次 WebView 进程（macOS / Linux）。返回 `{ stop() }`，stop 之后得到 `{ samples, attribution, metric, notes }`
 * （samples：`{ t, kib, n }[]`）；平台不支持时返回 null
 */
export function startWebviewSampler(rootPid) {
  if (process.platform !== 'darwin' && process.platform !== 'linux') return null
  const samples = []
  const notes = new Set()
  let attribution = process.platform === 'linux' ? 'descendants' : null
  let timer = null
  let stopped = false
  let pending = Promise.resolve()
  const tick = async () => {
    try {
      if (process.platform === 'darwin') {
        const r = await macWebviews(rootPid)
        attribution = r.mode === 'exact' ? 'responsible' : 'responsible-approx'
        samples.push({ t: Date.now(), kib: r.kib, n: r.n })
      } else {
        const r = linuxWebviews(readProcTable(), rootPid)
        samples.push({ t: Date.now(), kib: r.kib, n: r.n })
      }
    } catch (e) {
      notes.add(`WebView 采样失败：${e instanceof Error ? e.message.split('\n')[0] : e}`)
    }
  }
  const loop = () => {
    if (stopped) return
    pending = tick().finally(() => {
      if (!stopped) timer = setTimeout(loop, WEBVIEW_INTERVAL_MS)
    })
  }
  loop()
  return {
    async stop() {
      stopped = true
      clearTimeout(timer)
      await pending
      return {
        samples,
        attribution,
        metric: process.platform === 'darwin' ? 'phys_footprint' : 'rss',
        notes: [...notes],
      }
    },
  }
}

/** 中位数（最近秩法，偶数个时取较小的那个）；没有样本时为 null */
function median(values) {
  if (values.length === 0) return null
  const xs = values.toSorted((a, b) => a - b)
  return xs[Math.ceil(xs.length / 2) - 1]
}

/**
 * 采样结果 → 报告的 `webview` 段。热状态窗口 = 会话后空闲（报告写出前 post_session_idle.seconds 秒，跳过开头 2 s）；
 * 报告没有 post_session_idle（旧版本）时取写报告前 5 s
 */
export function webviewSection(report, sampled) {
  const notes = [...sampled.notes]
  const { samples } = sampled
  const end = report.ts
  const seconds = report.post_session_idle?.seconds
  const from = seconds ? end - seconds * 1000 + HOT_SKIP_MS : end - 5000
  const hot = samples.filter((s) => s.t >= from && s.t <= end)
  const withWv = samples.filter((s) => s.n > 0)
  if (withWv.length === 0) notes.push('冒烟期间没有找到属于应用的 WebView 进程')
  else if (hot.length === 0) notes.push('会话后空闲的时间窗里没有样本')
  const found = withWv.length > 0
  return {
    memory_metric: sampled.metric,
    attribution: found ? sampled.attribution : null,
    samples: samples.length,
    hot_samples: hot.length,
    hot_kb: found ? median(hot.map((s) => s.kib)) : null,
    peak_kb: found ? Math.max(...withWv.map((s) => s.kib)) : null,
    processes: found && hot.length ? Math.max(...hot.map((s) => s.n)) : null,
    // 逐秒曲线：[距写报告的秒数（负数）, KiB, 进程数]，便于分析各阶段（预热 / 连续触发 / 设置窗口 / 会话后）
    series: samples.map((x) => [Math.round((x.t - end) / 100) / 10, x.kib, x.n]),
    notes,
  }
}

function selfTest() {
  // Linux 的后代进程（xvfb-run → wisp-desktop → WebKitWebProcess / WebKitNetworkProcess；无关进程不计）
  const table = new Map([
    [10, { ppid: 1, comm: 'xvfb-run', rssKib: 1000 }],
    [11, { ppid: 10, comm: 'Xvfb', rssKib: 9000 }],
    [12, { ppid: 10, comm: 'wisp-desktop', rssKib: 40000 }],
    [13, { ppid: 12, comm: 'WebKitWebProces', rssKib: 30000 }],
    [14, { ppid: 12, comm: 'WebKitNetworkPr', rssKib: 8000 }],
    [20, { ppid: 1, comm: 'WebKitWebProces', rssKib: 50000 }],
  ])
  assert.deepEqual(linuxWebviews(table, 10), { kib: 38000, n: 2 })
  assert.deepEqual(linuxWebviews(table, 99), { kib: 0, n: 0 })
  // macos-webkit.py --scan 的输出（一次调用：归属 + phys_footprint；已退出的进程不在第四列里）
  assert.deepEqual(parseScan('exact 501 11,12,13 11:20480,13:4096\n'), {
    kib: 24576,
    n: 2,
    mode: 'exact',
  })
  assert.deepEqual(parseScan('approx 77 - -'), { kib: 0, n: 0, mode: 'approx' })
  // 热状态窗口：报告写出前 10 s（跳过开头 2 s）的中位数；峰值取全程
  const ts = 100_000
  const samples = [
    { t: 80_000, kib: 0, n: 0 },
    { t: 85_000, kib: 90_000, n: 4 },
    { t: 90_500, kib: 70_000, n: 4 }, // 会话后空闲开头的 2 s 里：跳过
    { t: 93_000, kib: 52_000, n: 3 },
    { t: 95_000, kib: 51_000, n: 3 },
    { t: 97_000, kib: 51_500, n: 3 },
    { t: 101_000, kib: 1_000, n: 1 }, // 写报告之后、退出途中：不计
  ]
  const report = { ts, post_session_idle: { seconds: 10 } }
  const sec = webviewSection(report, {
    samples,
    attribution: 'responsible-approx',
    metric: 'phys_footprint',
    notes: [],
  })
  assert.equal(sec.hot_kb, 51_500)
  assert.equal(sec.peak_kb, 90_000)
  assert.equal(sec.processes, 3)
  assert.equal(sec.hot_samples, 3)
  assert.equal(sec.attribution, 'responsible-approx')
  const none = webviewSection(report, {
    samples: [{ t: 95_000, kib: 0, n: 0 }],
    attribution: 'responsible-approx',
    metric: 'phys_footprint',
    notes: [],
  })
  assert.equal(none.hot_kb, null)
  assert.equal(none.attribution, null)
  assert.ok(none.notes.some((n) => n.includes('没有找到')))
  ok('run-smoke 自检通过（WebView 归属与热状态窗口）')
}

/**
 * 结束子进程及其后代：POSIX 上子进程是进程组组长（spawn 的 detached），对整个组发信号；Windows 上用 taskkill /T。
 * 进程组已经不存在时静默返回
 */
function killTree(child, signal) {
  if (child.pid === undefined) return
  if (process.platform === 'win32') {
    spawnSync('taskkill', ['/pid', String(child.pid), '/T', '/F'], { stdio: 'ignore' })
    return
  }
  try {
    process.kill(-child.pid, signal)
  } catch {
    // ESRCH：整个进程组都已退出
  }
}

function main() {
  const { values } = parseArgs({
    options: {
      profile: { type: 'string', default: 'debug' },
      bin: { type: 'string' },
      out: { type: 'string', default: 'target/smoke-report.json' },
      timeout: { type: 'string', default: '300' },
      webview: { type: 'boolean', default: false },
      'self-test': { type: 'boolean', default: false },
      help: { type: 'boolean', short: 'h', default: false },
    },
  })
  if (values.help) {
    console.log(
      '用法：node scripts/perf/run-smoke.mjs [--profile debug|release] [--bin <exe>] [--out <报告>] [--timeout 秒] [--webview] | --self-test',
    )
    return
  }
  if (values['self-test']) return selfTest()
  if (!['debug', 'release'].includes(values.profile))
    throw new Error('--profile 只能是 debug 或 release')
  const timeoutS = Number(values.timeout)
  if (!(timeoutS > 0)) throw new Error('--timeout 必须是正数（秒）')
  const exe = process.platform === 'win32' ? 'wisp-desktop.exe' : 'wisp-desktop'
  const bin = resolve(ROOT, values.bin ?? join('target', values.profile, exe))
  if (!existsSync(bin)) {
    throw new Error(
      `${rel(bin)} 不存在：先执行 pnpm tauri build${values.profile === 'debug' ? ' --debug' : ''} --no-bundle --features fake-llm --config src-tauri/tauri.no-updater.conf.json`,
    )
  }
  const out = isAbsolute(values.out) ? values.out : join(ROOT, values.out)
  mkdirSync(dirname(out), { recursive: true })
  rmSync(out, { force: true })
  const logPath = join(dirname(out), 'smoke-run.log')

  let cmd = bin
  let args = []
  if (process.platform === 'linux' && !process.env.DISPLAY && !process.env.WAYLAND_DISPLAY) {
    if (spawnSync('xvfb-run', ['--help'], { stdio: 'ignore' }).error) {
      throw new Error(
        '没有显示器且找不到 xvfb-run（sudo apt-get install xvfb，或 scripts/ci/linux-deps.sh smoke）',
      )
    }
    cmd = 'xvfb-run'
    args = ['-a', '-s', '-screen 0 1920x1080x24', bin]
  }

  info(`启动 ${rel(bin)}（WISP_SMOKE=1 WISP_FAKE_LLM=1，超时 ${timeoutS} s）`)
  const log = createWriteStream(logPath)
  const started = Date.now()
  return new Promise((resolvePromise, reject) => {
    const child = spawn(cmd, args, {
      cwd: ROOT,
      env: {
        ...process.env,
        WISP_SMOKE: '1',
        WISP_FAKE_LLM: '1',
        WISP_SMOKE_REPORT: out,
        RUST_BACKTRACE: '1',
      },
      stdio: ['ignore', 'pipe', 'pipe'],
      // 自成进程组，超时时能连同 Xvfb、WebView 等后代一起结束（Windows 上 detached 会新开控制台，不用）
      detached: process.platform !== 'win32',
    })
    for (const stream of [child.stdout, child.stderr]) {
      stream.on('data', (d) => {
        log.write(d)
        process.stdout.write(d)
      })
    }
    // Linux 上 child 是 xvfb-run：WebView 进程是它的后代，按后代查找同样成立
    const sampler =
      values.webview && child.pid !== undefined ? startWebviewSampler(child.pid) : null
    let timedOut = false
    let forceTimer = null
    const terminate = () => {
      killTree(child, 'SIGTERM')
      forceTimer = setTimeout(() => killTree(child, 'SIGKILL'), KILL_GRACE_MS)
    }
    const timer = setTimeout(() => {
      timedOut = true
      terminate()
    }, timeoutS * 1000)
    // 本脚本被中断（Ctrl-C、CI 取消）时同样清理进程组：detached 的子进程收不到终端的信号
    const onSignal = (sig) => {
      killTree(child, 'SIGKILL')
      process.exit(sig === 'SIGINT' ? 130 : 143)
    }
    process.once('SIGINT', onSignal)
    process.once('SIGTERM', onSignal)
    const settle = () => {
      clearTimeout(timer)
      clearTimeout(forceTimer)
      process.removeListener('SIGINT', onSignal)
      process.removeListener('SIGTERM', onSignal)
    }
    child.on('error', (e) => {
      settle()
      reject(new Error(`无法启动 ${cmd}：${e.message}`))
    })
    child.on('exit', async (code, signal) => {
      settle()
      // 组长退出后组里可能还有残留（超时被结束时的 Xvfb、没跟着退出的 WebView 进程）：一并结束。
      // Windows 上进程退出后 pid 可能被复用，不再 taskkill
      if (process.platform !== 'win32') killTree(child, 'SIGKILL')
      log.end()
      const sampled = sampler ? await sampler.stop() : null
      if (sampled && !timedOut && code === 0 && existsSync(out)) {
        try {
          const report = JSON.parse(readFileSync(out, 'utf8'))
          report.webview = webviewSection(report, sampled)
          writeFileSync(out, `${JSON.stringify(report, null, 2)}\n`)
          const wv = report.webview
          if (wv.hot_kb === null) warn(`WebView 内存没有采到：${wv.notes.join('；')}`)
          else
            info(
              `WebView 合计：会话后（热）${(wv.hot_kb / 1024).toFixed(1)} MiB（${wv.processes} 个进程），全程峰值 ${(wv.peak_kb / 1024).toFixed(1)} MiB（${wv.attribution}，${wv.samples} 个样本）`,
            )
        } catch (e) {
          warn(`写入 WebView 采样失败：${e instanceof Error ? e.message : e}`)
        }
      }
      const secs = ((Date.now() - started) / 1000).toFixed(1)
      if (timedOut) {
        reject(
          new Error(
            `冒烟超时：${timeoutS} s 内应用没有退出（流程卡死，或 WISP_SMOKE 未实现）；日志 ${rel(logPath)}`,
          ),
        )
      } else if (code !== 0) {
        reject(
          new Error(`应用以 ${signal ?? `退出码 ${code}`} 结束（${secs} s）；日志 ${rel(logPath)}`),
        )
      } else if (!existsSync(out)) {
        reject(
          new Error(
            `应用正常退出但没有写出 ${rel(out)}（WISP_SMOKE_REPORT 约定见 scripts/perf/README.md）`,
          ),
        )
      } else {
        ok(`冒烟完成（${secs} s），报告 ${rel(out)}`)
        resolvePromise()
      }
    })
  })
}

if (isMain(import.meta.url)) await run(main)
