#!/usr/bin/env node
/**
 * scripts/eval-ocr.mjs：一条命令跑 OCR 截图评测（`pnpm eval:ocr [选项] [-- <wisp-eval 的其他参数>]`）。
 *
 * 默认流程：用 Playwright（Chromium，2 倍像素密度）把 crates/wisp-prompts/eval/ocr-pages/ 下的页面渲染成截图
 * → 从 Wisp 的 settings.json（只读）选一个走模型服务的视觉模型 → 取得它的 Key → 调用评测器
 * `wisp-eval --ocr-only` 跑段落模式的四个样本（O5–O8），评测器把按样本的汇总打印到标准输出，报告写进 --out。
 *
 *   --all                   跑全部 OCR 样本（O1–O8）
 *   --samples <列表>         只跑这些 OCR 样本：完整 id 或第一个 `-` 之前的前缀，逗号分隔、大小写不敏感（例如 O5,O6）
 *   --model <服务商>/<模型>   指定模型（服务商 id 与模型 id，以第一个 `/` 分隔），不看设置里的默认模型
 *   --settings <文件>        改读这份设置文件（默认 Wisp 自己的 settings.json，位置见 defaultSettingsPath）
 *   --out <目录>             报告目录（默认 target/eval-runs），截图写到 <out>/ocr-samples/
 *   --dry-run               只渲染截图、装配提示词并列出计划：不取 Key、不发请求
 *   --render-only           只生成截图
 *   --self-test             检查选模型、样本过滤、Key 环境变量名等纯逻辑
 *
 * Key 依次取自：① 环境变量 WISP_EVAL_KEY_<ID>（评测器原有的写法，原样继承）或 WISP_EVAL_KEY；② 系统钥匙串里 Wisp
 * 自己保存的条目（service「wisp」、账户名为服务商 id；macOS 用 security，Linux 用 secret-tool，Windows 跳过）；
 * ③ 终端里隐藏输入。Key 只经子进程的环境变量 WISP_EVAL_KEY_<ID> 交给评测器：不打印、不写文件、不进命令行参数。
 * 交给评测器的服务商配置写在系统临时目录里的一份临时 JSON（取自设置文件，本来就不含 Key），结束时删除（含失败与 Ctrl+C）。
 */
import assert from 'node:assert/strict'
import { spawn, spawnSync } from 'node:child_process'
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { createRequire } from 'node:module'
import { homedir, tmpdir } from 'node:os'
import { join, posix, resolve, win32 } from 'node:path'
import { pathToFileURL } from 'node:url'
import { parseArgs } from 'node:util'
import { APP_DIR, ROOT, info, isMain, ok, rel, run, warn } from './lib/common.mjs'

const PAGES_DIR = join(ROOT, 'crates', 'wisp-prompts', 'eval', 'ocr-pages')
const SAMPLES_RS = join(ROOT, 'crates', 'wisp-prompts', 'src', 'eval', 'samples.rs')
const DEFAULT_OUT = join(ROOT, 'target', 'eval-runs')
/** 默认只跑段落模式的四张截图：这是最需要真实模型验证的模式 */
const DEFAULT_SAMPLES = ['O5', 'O6', 'O7', 'O8']
/** 能做视觉 OCR 的协议；paddle_ocr（PaddleOCR 云端）不是 LLM，评测器发不了请求 */
const LLM_PROTOCOLS = new Set(['openai_chat', 'openai_responses', 'anthropic_messages', 'gemini'])
/** Wisp 在钥匙串里保存模型服务 Key 用的 service 名（账户名是服务商 id） */
const KEYCHAIN_SERVICE = 'wisp'
/** 等待钥匙串授权框的上限：用户点「允许」之前 security 一直阻塞 */
const KEYCHAIN_TIMEOUT_MS = 5 * 60_000

const USAGE = `用法：pnpm eval:ocr [--all | --samples O5,O6] [--model <服务商>/<模型>] [--settings <文件>]
                     [--out <目录>] [--dry-run | --render-only] [-- <wisp-eval 的其他参数>]
       node scripts/eval-ocr.mjs --self-test`

// ---------------------------------------------------------------------------------------------
// 纯逻辑（--self-test 覆盖）

/**
 * Wisp 的 settings.json 所在位置：与 apps/desktop/src-tauri/src/app/paths.rs 一致，取 `dirs::config_dir()` 再拼 `wisp`，
 * 不用带 bundle id 的 Tauri 目录。macOS `~/Library/Application Support/wisp/`、Windows `%APPDATA%\wisp\`、
 * Linux `$XDG_CONFIG_HOME/wisp/`（未设置或不是绝对路径时为 `~/.config/wisp/`）
 */
export function defaultSettingsPath(
  platform = process.platform,
  env = process.env,
  home = homedir(),
) {
  if (platform === 'darwin') {
    return posix.join(home, 'Library', 'Application Support', 'wisp', 'settings.json')
  }
  if (platform === 'win32') {
    return env.APPDATA ? win32.join(env.APPDATA, 'wisp', 'settings.json') : null
  }
  const xdg = env.XDG_CONFIG_HOME
  const base = xdg && posix.isAbsolute(xdg) ? xdg : posix.join(home, '.config')
  return posix.join(base, 'wisp', 'settings.json')
}

/** 评测器读取 Key 的环境变量名：服务商 id 逐字符大写，ASCII 字母数字以外的字符换成 `_`（与 wisp-eval 的 key_env_name 一致） */
export function keyEnvName(providerId) {
  let id = ''
  for (const c of providerId) id += /^[A-Za-z0-9]$/.test(c) ? c.toUpperCase() : '_'
  return `WISP_EVAL_KEY_${id}`
}

/** 从 samples.rs 的源码里读出 OCR 样本的 id 与截图文件名（以评测器为准，页面与样本对不上时能及时发现） */
export function parseOcrSamples(source) {
  const block = source.slice(source.indexOf('OCR_SAMPLES'))
  const samples = []
  for (const m of block.matchAll(
    /OcrSample\s*\{[^}]*?\bid:\s*"([^"]+)"[^}]*?\bfile:\s*"([^"]+)"/g,
  )) {
    samples.push({ id: m[1], file: m[2] })
  }
  return samples
}

/** 按 --samples 的写法选样本（规则与评测器的 --ocr-samples 相同）；有一项一个都没选中时报错，免得白跑一趟 */
export function selectSamples(samples, tokens) {
  if (tokens.some((t) => t.toLowerCase() === 'all')) return samples
  const unknown = tokens.filter((t) => !samples.some((s) => sampleMatches(s.id, t)))
  if (unknown.length) {
    throw new Error(
      `没有这些 OCR 样本：${unknown.join(', ')}（可选：${samples.map((s) => s.id).join(', ')}）`,
    )
  }
  return samples.filter((s) => tokens.some((t) => sampleMatches(s.id, t)))
}

function sampleMatches(id, token) {
  const t = token.toLowerCase()
  return id.toLowerCase() === t || id.split('-')[0].toLowerCase() === t
}

function splitList(s) {
  return s
    .split(',')
    .map((p) => p.trim())
    .filter(Boolean)
}

/**
 * 选评测用的模型。`--model` 优先；否则用设置里的 OCR 默认模型（defaults.ocr），但它必须是 providers[] 里走 LLM 协议的
 * 服务、视觉能力不是 no（离线模型、PaddleOCR 云端都不满足）；不满足时退回第一个启用的、视觉能力是 yes 的模型，
 * 再退回第一个视觉未知的。返回 `{ provider, modelId, why }`，provider 是设置文件里的原样对象
 */
export function pickModel(settings, override) {
  const providers = Array.isArray(settings?.providers) ? settings.providers : []
  const isLlm = (p) => p && typeof p.id === 'string' && LLM_PROTOCOLS.has(p.protocol)
  const modelsOf = (p) => (Array.isArray(p.models) ? p.models : [])

  if (override) {
    const slash = override.indexOf('/')
    if (slash <= 0 || slash === override.length - 1) {
      throw new Error(`--model 的写法是 <服务商 id>/<模型 id>，收到 ${JSON.stringify(override)}`)
    }
    const providerId = override.slice(0, slash)
    const modelId = override.slice(slash + 1)
    const provider = providers.find((p) => p?.id === providerId)
    if (!provider) {
      throw new Error(
        `设置里没有服务商 ${JSON.stringify(providerId)}（有：${providers.map((p) => p?.id).join(', ') || '无'}）`,
      )
    }
    if (!isLlm(provider)) {
      throw new Error(
        `服务商 ${providerId} 的协议是 ${provider.protocol}，评测器只能用模型服务做视觉 OCR`,
      )
    }
    const known = modelsOf(provider).some((m) => m?.id === modelId)
    return {
      provider,
      modelId,
      why: known ? '--model 指定' : '--model 指定（不在服务商的模型列表里）',
    }
  }

  const ref = settings?.defaults?.ocr
  let skipped = 'Wisp 的设置里没有 OCR 默认模型'
  if (ref && typeof ref === 'object') {
    const provider = providers.find((p) => p?.id === ref.provider_id)
    const model = provider && modelsOf(provider).find((m) => m?.id === ref.model_id)
    if (!provider) {
      skipped = `OCR 默认模型的服务商 ${ref.provider_id} 不是模型服务（例如离线模型）`
    } else if (!isLlm(provider)) {
      skipped = `OCR 默认模型的服务商 ${provider.id} 的协议是 ${provider.protocol}，不是模型服务`
    } else if (!model) {
      skipped = `OCR 默认模型 ${ref.model_id} 不在服务商 ${provider.id} 的模型列表里`
    } else if (model.vision === 'no') {
      skipped = `OCR 默认模型 ${provider.id}/${model.id} 标记为不支持图片`
    } else {
      return { provider, modelId: model.id, why: 'Wisp 的 OCR 默认模型' }
    }
  }
  for (const want of ['yes', 'unknown']) {
    for (const provider of providers.filter(isLlm)) {
      const model = modelsOf(provider).find(
        (m) =>
          m && typeof m.id === 'string' && m.enabled !== false && (m.vision ?? 'unknown') === want,
      )
      if (model) {
        const kind = want === 'yes' ? '支持图片' : '视觉能力未知'
        return { provider, modelId: model.id, why: `${skipped}，改用第一个启用的、${kind}的模型` }
      }
    }
  }
  throw new Error(
    `${skipped}，也没有启用的、可能支持图片的模型；请用 --model <服务商 id>/<模型 id> 指定`,
  )
}

/** 把命令行拆成本脚本的参数与 `--` 之后原样交给 wisp-eval 的参数 */
export function splitPassthrough(argv) {
  const i = argv.indexOf('--')
  return i < 0 ? [argv, []] : [argv.slice(0, i), argv.slice(i + 1)]
}

/** PNG 的宽高（IHDR） */
function pngSize(file) {
  const buf = readFileSync(file)
  return { width: buf.readUInt32BE(16), height: buf.readUInt32BE(20) }
}

// ---------------------------------------------------------------------------------------------
// 截图

function loadPlaywright() {
  // Playwright 装在前端工程里（devDependencies），从那里解析，根目录不另装
  const require = createRequire(join(APP_DIR, 'package.json'))
  try {
    return require('@playwright/test')
  } catch (e) {
    throw new Error('找不到 Playwright：先在仓库根目录运行 pnpm install', { cause: e })
  }
}

/** 截图之后低于这个比例（最窄一行 / 栏宽）说明手工断行的行长差得太多，两端对齐后字距会被明显拉开 */
const MIN_SPREAD = 0.9

/** 把每个页面的 #shot 渲染成 `<outDir>/<页面名>.png`，返回 [{ file, width, height }] */
async function renderPages(outDir, samples) {
  const pages = readdirSync(PAGES_DIR)
    .filter((n) => n.endsWith('.html'))
    .toSorted()
  const missing = [...new Set(samples.map((s) => s.file))].filter(
    (f) => !pages.includes(f.replace(/\.png$/, '.html')),
  )
  if (missing.length) warn(`这些截图还没有对应的页面，需要手工放进截图目录：${missing.join(', ')}`)

  const { chromium } = loadPlaywright()
  let browser
  try {
    browser = await chromium.launch()
  } catch (e) {
    const first = String(e instanceof Error ? e.message : e).split('\n')[0]
    throw new Error(
      `无法启动 Chromium（${first}）。第一次使用先运行：pnpm -C apps/desktop exec playwright install chromium`,
      { cause: e },
    )
  }
  mkdirSync(outDir, { recursive: true })
  try {
    const context = await browser.newContext({
      viewport: { width: 1280, height: 900 },
      deviceScaleFactor: 2,
      colorScheme: 'light',
      locale: 'zh-CN',
      reducedMotion: 'reduce',
    })
    // 页面只能读本目录下的文件：任何网络请求都拒绝并记下来，渲染完报错，保证截图在离线时也一模一样
    const blocked = []
    await context.route('**/*', (route) => {
      const url = route.request().url()
      if (url.startsWith('file:')) return route.continue()
      blocked.push(url)
      return route.abort()
    })
    // 各页面互不相关，同一个浏览器里并行渲染
    const shots = await Promise.all(
      pages.map(async (name) => {
        const page = await context.newPage()
        await page.goto(pathToFileURL(join(PAGES_DIR, name)).href)
        await page.evaluate(() => document.fonts.ready.then(() => undefined))
        if ((await page.locator('script').count()) > 0) {
          await page.waitForSelector('html[data-ready]', { state: 'attached', timeout: 10_000 })
        }
        const spread = await page.evaluate(() => document.documentElement.dataset.spread)
        if (spread && Number(spread) < MIN_SPREAD) {
          warn(`${name}：最窄的一行只有最宽一行的 ${spread}，两端对齐后字距会被拉开，调整一下断行`)
        }
        const file = join(outDir, name.replace(/\.html$/, '.png'))
        const shot = page.locator('#shot')
        // 元素的宽高带小数时，截图会多出半行像素，把 body 的底色带进边缘；先取整到整数 CSS 像素（只会略微变大，不会多折行）
        await shot.evaluate((el) => {
          const box = el.getBoundingClientRect()
          el.style.width = `${Math.ceil(box.width)}px`
          el.style.height = `${Math.ceil(box.height)}px`
        })
        await shot.screenshot({ path: file, animations: 'disabled' })
        await page.close()
        return { file, ...pngSize(file) }
      }),
    )
    if (blocked.length) throw new Error(`页面不应访问网络，被拦下的请求：${blocked.join(', ')}`)
    return shots
  } finally {
    await browser.close()
  }
}

// ---------------------------------------------------------------------------------------------
// Key

/**
 * 取得 Key。返回 `{ key, source }`：key 为 null 表示子进程已经从环境变量继承了 WISP_EVAL_KEY_<ID>，为空串表示不带 Key
 * （只适用于本机不需要 Key 的服务）。任何分支都不打印 Key，也不把它放进错误信息
 */
async function resolveKey(provider) {
  const envName = keyEnvName(provider.id)
  if (process.env[envName]) return { key: null, source: `环境变量 ${envName}` }
  if (process.env.WISP_EVAL_KEY)
    return { key: process.env.WISP_EVAL_KEY, source: '环境变量 WISP_EVAL_KEY' }

  const fromKeychain = readKeychain(provider.id)
  if (fromKeychain) return { key: fromKeychain, source: '系统钥匙串' }

  if (process.stdin.isTTY && process.stderr.isTTY) {
    const label = provider.name ? `${provider.name}（${provider.id}）` : provider.id
    const key = await promptHidden(
      `粘贴 ${label} 的 API Key（输入不显示，回车确认；留空表示不带 Key）：`,
    )
    return { key: key.trim(), source: '终端输入' }
  }
  throw new Error(
    `没有拿到服务商 ${provider.id} 的 Key。任选一种方式：\n` +
      `  1. 设置环境变量 WISP_EVAL_KEY（或 ${envName}）后再运行；\n` +
      `  2. 在 Wisp 的设置里保存这个服务商的 Key（macOS 读钥匙串、Linux 读 Secret Service）；\n` +
      `  3. 在交互式终端里运行，按提示粘贴 Key。`,
  )
}

/** 从系统钥匙串读 Wisp 保存的 Key；没有、被拒绝或当前平台不支持时返回 null（只说明原因，不输出命令的原始错误） */
function readKeychain(providerId) {
  if (process.platform === 'darwin') {
    info(
      '从钥匙串读取 Key：macOS 可能会弹出钥匙串授权框，点「允许」即可（要求密码时填 Mac 的登录密码；点「拒绝」则改为手动输入）',
    )
    const r = spawnSync(
      'security',
      ['find-generic-password', '-s', KEYCHAIN_SERVICE, '-a', providerId, '-w'],
      { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], timeout: KEYCHAIN_TIMEOUT_MS },
    )
    if (r.status === 0 && r.stdout.trim()) return r.stdout.replace(/\r?\n$/, '')
    // 44 = errSecItemNotFound
    if (r.status === 44) info('钥匙串里没有这个服务商的 Key')
    else warn(`没能从钥匙串读到 Key（${r.error ? r.error.code : `退出码 ${r.status}`}）`)
    return null
  }
  if (process.platform === 'linux') {
    // keyring 的 Secret Service 后端用 service / username 两个属性定位条目
    const r = spawnSync(
      'secret-tool',
      ['lookup', 'service', KEYCHAIN_SERVICE, 'username', providerId],
      { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], timeout: KEYCHAIN_TIMEOUT_MS },
    )
    if (r.error?.code === 'ENOENT') {
      info('没有 secret-tool（libsecret-tools），跳过 Secret Service')
      return null
    }
    if (r.status === 0 && r.stdout.trim()) return r.stdout.replace(/\r?\n$/, '')
    info('Secret Service 里没有这个服务商的 Key')
    return null
  }
  // Windows 的凭据管理器没有自带的只读命令行，跳过
  info('Windows 上不读取凭据管理器')
  return null
}

/** 终端里隐藏输入（raw 模式逐字读取，不回显）；Ctrl+C 取消 */
function promptHidden(question) {
  return new Promise((resolvePromise, reject) => {
    const { stdin, stderr } = process
    stderr.write(question)
    let value = ''
    // 粘贴时终端可能夹带控制序列（ESC [ … 字母或 ~），整段丢弃
    let escape = false
    const finish = (err) => {
      stdin.off('data', onData)
      stdin.setRawMode(false)
      stdin.pause()
      stderr.write('\n')
      if (err) reject(err)
      else resolvePromise(value)
    }
    const onData = (chunk) => {
      for (const ch of chunk) {
        if (escape) {
          if (/[A-Za-z~]/.test(ch)) escape = false
          continue
        }
        if (ch === '\u001b') escape = true
        else if (ch === '\r' || ch === '\n') return finish()
        else if (ch === '\u0003') return finish(new Error('已取消'))
        else if (ch === '\u0004') {
          if (!value) return finish()
        } else if (ch === '\u007f' || ch === '\b') value = [...value].slice(0, -1).join('')
        else if (ch >= ' ') value += ch
      }
      return undefined
    }
    stdin.setRawMode(true)
    stdin.setEncoding('utf8')
    stdin.on('data', onData)
    stdin.resume()
  })
}

// ---------------------------------------------------------------------------------------------
// 运行

function readSettings(path, explicit) {
  let text
  try {
    text = readFileSync(path, 'utf8')
  } catch (e) {
    const reason = e?.code === 'ENOENT' ? '文件不存在' : (e?.code ?? String(e))
    const hint = explicit
      ? ''
      : '。先在 Wisp 里添加一个模型服务，或用 --settings <文件> 指定一份设置文件'
    throw new Error(`读不到设置文件 ${path}（${reason}）${hint}`, { cause: e })
  }
  try {
    return JSON.parse(text)
  } catch (e) {
    throw new Error(`设置文件 ${path} 不是合法的 JSON（${e instanceof Error ? e.message : e}）`, {
      cause: e,
    })
  }
}

/** 运行评测器；Ctrl+C 由终端同时发给子进程，这里只等它退出，之后照常清理 */
function runEvaluator(args, env) {
  return new Promise((resolvePromise, reject) => {
    const child = spawn('cargo', args, { cwd: ROOT, env, stdio: 'inherit' })
    const onSigint = () => {}
    const onSigterm = () => child.kill('SIGTERM')
    process.on('SIGINT', onSigint)
    process.on('SIGTERM', onSigterm)
    const cleanup = () => {
      process.off('SIGINT', onSigint)
      process.off('SIGTERM', onSigterm)
    }
    child.on('error', (e) => {
      cleanup()
      reject(new Error(`无法运行 cargo（${e.code ?? e.message}）`))
    })
    child.on('exit', (code, signal) => {
      cleanup()
      resolvePromise(code ?? (signal === 'SIGINT' ? 130 : 1))
    })
  })
}

/** 列出本次运行写出的报告 */
function newReports(outDir, since) {
  try {
    return readdirSync(outDir)
      .filter((n) => /\.(md|jsonl)$/.test(n))
      .map((n) => join(outDir, n))
      .filter((p) => statSync(p).mtimeMs >= since)
      .toSorted()
  } catch {
    return []
  }
}

async function main() {
  const [own, passthrough] = splitPassthrough(process.argv.slice(2))
  let values
  try {
    ;({ values } = parseArgs({
      args: own,
      options: {
        all: { type: 'boolean', default: false },
        samples: { type: 'string' },
        model: { type: 'string' },
        settings: { type: 'string' },
        out: { type: 'string' },
        'dry-run': { type: 'boolean', default: false },
        'render-only': { type: 'boolean', default: false },
        'self-test': { type: 'boolean', default: false },
        help: { type: 'boolean', short: 'h', default: false },
      },
    }))
  } catch (e) {
    throw new Error(`${e instanceof Error ? e.message : e}\n${USAGE}`, { cause: e })
  }
  if (values.help) return info(USAGE)
  if (values['self-test']) return selfTest()
  if (values.all && values.samples) throw new Error('--all 与 --samples 只能选一个')
  if (values['dry-run'] && values['render-only']) {
    throw new Error('--dry-run 与 --render-only 只能选一个')
  }

  const started = Date.now()
  const samples = parseOcrSamples(readFileSync(SAMPLES_RS, 'utf8'))
  if (!samples.length) throw new Error(`没能从 ${rel(SAMPLES_RS)} 读出 OCR 样本`)
  const tokens = values.all ? ['all'] : values.samples ? splitList(values.samples) : DEFAULT_SAMPLES
  const selected = selectSamples(samples, tokens)

  const outDir = values.out ? resolve(values.out) : DEFAULT_OUT
  const shotDir = join(outDir, 'ocr-samples')
  const shots = await renderPages(shotDir, samples)
  for (const s of shots) info(`  ${rel(s.file)}  ${s.width}×${s.height}`)
  ok(`已渲染 ${shots.length} 张截图到 ${rel(shotDir)}`)
  if (values['render-only']) return undefined

  const settingsPath = values.settings ? resolve(values.settings) : defaultSettingsPath()
  if (!settingsPath)
    throw new Error('找不到 %APPDATA%，请用 --settings <文件> 指定 Wisp 的 settings.json')
  const settings = readSettings(settingsPath, Boolean(values.settings))
  const { provider, modelId, why } = pickModel(settings, values.model)
  const label = provider.name ? `${provider.name}（${provider.id}）` : provider.id
  info(`模型：${label} / ${modelId}  ——  ${why}`)
  if (settings?.offline?.ocr_model && !values.model) {
    info('提示：Wisp 当前用离线模型做文字识别；评测只能用模型服务，所以用的是上面这个')
  }
  info(`样本：${selected.map((s) => s.id).join(', ')}`)

  const env = { ...process.env }
  delete env.WISP_EVAL_KEY
  const envName = keyEnvName(provider.id)
  if (!values['dry-run']) {
    const { key, source } = await resolveKey(provider)
    if (key === null) {
      info(`Key：${source}`)
    } else if (key === '') {
      delete env[envName]
      info('Key：不带 Key')
    } else {
      env[envName] = key
      info(`Key：${source}`)
    }
  }

  // 临时文件只放服务商配置（设置文件里本来就没有 Key），放进私有的临时目录，退出时一定删掉
  const tmp = mkdtempSync(join(tmpdir(), 'wisp-eval-'))
  const removeTmp = () => rmSync(tmp, { recursive: true, force: true })
  process.on('exit', removeTmp)
  try {
    const providerJson = join(tmp, 'provider.json')
    writeFileSync(providerJson, JSON.stringify({ provider, models: [modelId] }), { mode: 0o600 })
    const args = [
      'run',
      '-p',
      'wisp-prompts',
      '--features',
      'eval',
      '--bin',
      'wisp-eval',
      '--',
      '--ocr-only',
      '--provider-json',
      providerJson,
      '--ocr-dir',
      shotDir,
      '--out',
      outDir,
    ]
    if (!values.all) args.push('--ocr-samples', selected.map((s) => s.id).join(','))
    if (values['dry-run']) args.push('--dry-run')
    args.push(...passthrough)
    const code = await runEvaluator(args, env)
    const reports = newReports(outDir, started)
    if (reports.length) info(`报告：\n${reports.map((p) => `  ${rel(p)}`).join('\n')}`)
    else info(`报告目录：${rel(outDir)}`)
    process.exitCode = code
  } finally {
    removeTmp()
    process.off('exit', removeTmp)
  }
  return undefined
}

// ---------------------------------------------------------------------------------------------

function selfTest() {
  assert.equal(keyEnvName('openai-main'), 'WISP_EVAL_KEY_OPENAI_MAIN')
  assert.equal(keyEnvName('Qwen.云2'), 'WISP_EVAL_KEY_QWEN__2', '非 ASCII 字符按一个字符换成一个 _')

  assert.equal(
    defaultSettingsPath('darwin', {}, '/Users/u'),
    '/Users/u/Library/Application Support/wisp/settings.json',
  )
  assert.equal(
    defaultSettingsPath('win32', { APPDATA: 'C:\\Users\\u\\AppData\\Roaming' }, 'C:\\Users\\u'),
    'C:\\Users\\u\\AppData\\Roaming\\wisp\\settings.json',
  )
  assert.equal(defaultSettingsPath('win32', {}, 'C:\\Users\\u'), null)
  assert.equal(defaultSettingsPath('linux', {}, '/home/u'), '/home/u/.config/wisp/settings.json')
  assert.equal(
    defaultSettingsPath('linux', { XDG_CONFIG_HOME: '/x/cfg' }, '/home/u'),
    '/x/cfg/wisp/settings.json',
  )
  assert.equal(
    defaultSettingsPath('linux', { XDG_CONFIG_HOME: 'relative' }, '/home/u'),
    '/home/u/.config/wisp/settings.json',
    '与 dirs 一致：相对路径的 XDG_CONFIG_HOME 不算',
  )

  const parsed = parseOcrSamples(readFileSync(SAMPLES_RS, 'utf8'))
  assert.ok(parsed.length >= 10, 'samples.rs 里至少有 10 个 OCR 样本')
  for (const s of parsed) {
    assert.ok(
      readdirSync(PAGES_DIR).includes(s.file.replace(/\.png$/, '.html')),
      `${s.id} 的 ${s.file} 没有页面`,
    )
  }
  const ids = (list) => list.map((s) => s.id)
  const fake = [
    { id: 'O3-formula-markdown', file: 'formula.png' },
    { id: 'O3-formula-translate', file: 'formula.png' },
    { id: 'O5-chat-paragraphs', file: 'chat.png' },
    { id: 'O6-pdf-paragraphs', file: 'pdf.png' },
  ]
  assert.deepEqual(ids(selectSamples(fake, ['o3'])), [
    'O3-formula-markdown',
    'O3-formula-translate',
  ])
  assert.deepEqual(ids(selectSamples(fake, ['O6-PDF-paragraphs', 'O5'])), [
    'O5-chat-paragraphs',
    'O6-pdf-paragraphs',
  ])
  assert.equal(selectSamples(fake, ['all']).length, 4)
  assert.throws(() => selectSamples(fake, ['O9']), /O9/)
  assert.throws(() => selectSamples(fake, ['O3-formula']), /O3-formula/, '只认完整 id 或前缀')

  const model = (id, extra = {}) => ({ id, enabled: true, vision: 'unknown', ...extra })
  const settings = {
    providers: [
      { id: 'paddle', protocol: 'paddle_ocr', models: [model('pp', { vision: 'yes' })] },
      {
        id: 'main',
        name: '主力',
        protocol: 'openai_chat',
        models: [
          model('text-only', { vision: 'no' }),
          model('maybe'),
          model('off', { vision: 'yes', enabled: false }),
          model('eyes', { vision: 'yes' }),
        ],
      },
    ],
    defaults: { ocr: { provider_id: 'main', model_id: 'maybe' } },
  }
  assert.equal(pickModel(settings).modelId, 'maybe', 'OCR 默认模型可用时直接用它')
  const withDefault = (ocr) => ({ ...settings, defaults: { ocr } })
  for (const ocr of [
    null,
    { provider_id: '@offline', model_id: 'pp_ocrv6_small' },
    { provider_id: 'paddle', model_id: 'pp' },
    { provider_id: 'main', model_id: 'text-only' },
    { provider_id: 'main', model_id: 'gone' },
  ]) {
    const picked = pickModel(withDefault(ocr))
    assert.equal(
      `${picked.provider.id}/${picked.modelId}`,
      'main/eyes',
      `默认模型 ${JSON.stringify(ocr)} 不可用时退回第一个启用的视觉模型`,
    )
    assert.match(picked.why, /改用/)
  }
  assert.equal(
    pickModel({
      providers: [{ id: 'p', protocol: 'gemini', models: [{ id: 'g' }] }],
    }).modelId,
    'g',
    '没有视觉能力为 yes 的模型时退回视觉未知（字段缺省即未知）',
  )
  assert.throws(
    () => pickModel({ providers: [settings.providers[0]], defaults: {} }),
    /--model/,
    '只有 PaddleOCR 时报错并提示 --model',
  )
  assert.throws(() => pickModel({}), /--model/)
  assert.deepEqual(
    [pickModel(settings, 'main/vendor/model-x').modelId, pickModel(settings, 'main/eyes').why],
    ['vendor/model-x', '--model 指定'],
    '模型 id 里可以有 /（以第一个 / 分隔）',
  )
  assert.throws(() => pickModel(settings, 'nope/x'), /nope/)
  assert.throws(() => pickModel(settings, 'paddle/pp'), /paddle_ocr/)
  assert.throws(() => pickModel(settings, 'main'), /写法/)
  assert.throws(() => pickModel(settings, 'main/'), /写法/)

  assert.deepEqual(splitPassthrough(['--all', '--', '--concurrency', '2']), [
    ['--all'],
    ['--concurrency', '2'],
  ])
  assert.deepEqual(splitPassthrough(['--dry-run']), [['--dry-run'], []])
  ok('eval-ocr 自检通过')
}

if (isMain(import.meta.url)) await run(main)
