#!/usr/bin/env node
/**
 * scripts/perf/exercise.mjs（会话后测量）：measure-*.sh `--exercise` 里执行的「用一次 Wisp」——
 * 先往剪贴板灌一批有代表性的条目，再依次唤出、关闭面板视图，**每一步都由应用日志（debug 级别）核对**，结束时保证没有
 * 面板在屏幕上。
 *
 * 为什么要核对：关闭面板靠「同一个 --trigger 再按一次」（HideReason::Toggle），可 --trigger 的第二实例启动时可能让面板
 * 失焦，blur_hides 先把面板隐藏了，随后转发到的 --trigger 又把面板打开——exercise 结束时面板还在屏幕上，「回收前」测的
 * 就是可见的面板，「回收后」也会因为没有回收而误报。这里每次触发后看日志：面板自己隐藏了就不再触发；被重新打开了就再关
 * 一次（每个视图最多 3 轮）；最后核对没有面板在屏幕上、也没有正在进行的唤出，否则失败。
 *
 * 用法（环境变量 WISP_BIN / WISP_LOG 由 measure-*.sh 给出：被测实例的可执行文件与它的应用日志）：
 *   node scripts/perf/exercise.mjs [--views input,clipboard] [--hold 1.5] [--texts 30] [--images 3] [--seed-optional]
 *                                  [--restore-clipboard auto|yes|no] [--tail 1]
 *     --views      依次使用的视图：input（--trigger translate_input）、clipboard（--trigger clipboard）
 *     --hold       每个视图上屏后停留的秒数
 *     --texts / --images  灌入剪贴板的文本条数 / PNG 张数（0 表示不灌）；--seed-optional：写入失败或应用记录的条数不够时
 *                  只告警（默认失败）
 *     --restore-clipboard  灌完后把原来的剪贴板文本写回（auto：没有 CI 环境变量时，即本机运行时）。只能保存文本——
 *                  原来是图片等内容时会被覆盖
 *     --tail       最后一个面板隐藏、日志静下来（约 0.6–1 s）之后再等的秒数，默认 0.5——最后一次隐藏约在 exercise 结束前
 *                  1.8 s（记录的 idle.session.last_hide_s；macOS 的回收因此约在 exercise 结束后 43 s）
 *   node scripts/perf/exercise.mjs --write-settings <settings.json>   写入测量用的设置（见下）
 *   node scripts/perf/exercise.mjs --self-test
 *
 * 剪贴板的使用画像：短语、中英句子与段落、代码、链接、颜色值、多行清单、一段约 4 KB 的长文（应用按内容分成 text / code /
 * link / color 等类型），以及合成截图式的 PNG（1600×1000、800×600、400×300）。每条间隔 0.35 s（macOS 的 changeCount 轮询
 * 是 4 Hz）、图片 0.7 s，然后等应用日志里出现同样多的记录：`clipboard item inserted`，或者内容与历史里已有的一条相同时的
 * `clipboard item touched`（只置顶、不新增；有这种记录时告警——历史没有清空）。macOS 用 pbcopy / osascript，Linux（X11）
 * 用 xclip，Windows 用 powershell.exe（STA）。剪贴板视图因此渲染的是真实的列表、来源应用图标与缩略图，而不是空状态。
 * 没有覆盖的使用：翻译（流式渲染、markdown-lite）——CLI 只能唤出空的输入视图，没有带文本的触发方式。
 * 系统剪贴板是全局的：本机上其他剪贴板管理器（包括其他正在运行或测量的 Wisp 实例）同样会记录这些条目，并行测量时错开。
 *
 * 测量用的设置（--write-settings；measure-*.sh 在隔离 HOME 里启动前写入）：
 * - general.update_policy = off：更新检查（启动 30 s 后一次、面板显示时惰性一次）是一次性的网络请求与 TLS 初始化，
 *   不该落进 exercise 或空闲窗口；
 * - general.launch_at_login = false：不写登录项；
 * - advanced.log_level = debug：本脚本与 record.mjs（idle.session 的 panel / views / hides / clipboard）依赖的面板事件；
 * - advanced.unload_panel_after_idle_min = 本平台新配置的默认值（`defaults::unload_panel_default`，与全新安装一致；
 *   自检核对与 Rust 源码同步）。
 * 文件已存在时只改这几项。
 */
/* oxlint-disable no-await-in-loop -- 每一步都要等应用的响应（日志里的事件）再走下一步，必须串行 */
import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import {
  closeSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readFileSync,
  readSync,
  rmSync,
  statSync,
  writeFileSync,
  appendFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { parseArgs } from 'node:util'
import { crc32, deflateSync } from 'node:zlib'
import { ROOT, info, isMain, ok, run, warn } from '../lib/common.mjs'
import { PanelTracker, parsePanelEvents } from './lib.mjs'

/** 视图 → 唤出它的 CLI 动作（`--trigger <action>`） */
export const VIEW_ACTION = { input: 'translate_input', clipboard: 'clipboard' }

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

// ---------------------------------------------------------------------------
// 测量用的设置

/** 本平台（macos / windows / linux）新配置里 unload_panel_after_idle_min 的默认值（`defaults::unload_panel_default`） */
export const UNLOAD_PANEL_DEFAULT = { macos: null, windows: null, linux: null }

export function currentOs() {
  return { darwin: 'macos', win32: 'windows' }[process.platform] ?? 'linux'
}

/** 把测量用的几项设置合并进已有的 settings.json 内容（对象；不存在时为 {}） */
export function measurementSettings(existing, os) {
  const s = existing && typeof existing === 'object' && !Array.isArray(existing) ? existing : {}
  return {
    ...s,
    schema_version: s.schema_version ?? 1,
    general: { ...s.general, update_policy: 'off', launch_at_login: false },
    advanced: {
      ...s.advanced,
      log_level: 'debug',
      unload_panel_after_idle_min: UNLOAD_PANEL_DEFAULT[os] ?? null,
    },
  }
}

function writeSettings(path, os = currentOs()) {
  let existing = {}
  if (existsSync(path)) {
    try {
      existing = JSON.parse(readFileSync(path, 'utf8'))
    } catch (e) {
      throw new Error(`${path} 不是合法的 JSON，不覆盖：${e.message}`, { cause: e })
    }
  }
  mkdirSync(dirname(path), { recursive: true })
  writeFileSync(path, `${JSON.stringify(measurementSettings(existing, os), null, 2)}\n`)
  ok(`已写入测量用的设置 ${path}（update_policy=off、launch_at_login=false、log_level=debug）`)
}

// ---------------------------------------------------------------------------
// 剪贴板的使用画像

const PHRASES = [
  'Ship it when the numbers say so.',
  '把这一段翻译成更口语化的说法',
  'The quick brown fox jumps over the lazy dog',
  '会议改到周四下午三点，地点不变。',
  'Remember to rotate the logs before Friday.',
  '这个动画的回弹有点过头，阻尼再加一点。',
]
const PARAGRAPHS = [
  'Idle cost matters more than peak cost for a tray app: it runs all day, and every wakeup keeps the CPU out of its deepest sleep state. The panel should be ready in under a hundred milliseconds, yet cost nothing while hidden.',
  '常驻后台的小工具，空闲时的开销比峰值更重要：它整天都在运行，每一次唤醒都让处理器离开最深的睡眠状态。面板要在一百毫秒内出现，隐藏时又几乎不占资源。',
  'Spring animations feel natural because velocity is continuous: when a gesture hands off to the animation, the element keeps moving at the speed the finger left it, then settles without a visible seam.',
  '弹簧动画之所以自然，是因为速度连续：手势交给动画时，元素保持手指离开时的速度继续运动，然后无缝地停下来。',
]
const CODE = [
  'fn main() {\n    let items: Vec<u32> = (1..=10).filter(|n| n % 2 == 0).collect();\n    println!("{items:?}");\n}',
  'const debounce = (fn, ms) => {\n  let t\n  return (...args) => {\n    clearTimeout(t)\n    t = setTimeout(() => fn(...args), ms)\n  }\n}',
  'SELECT id, title, created_at\nFROM notes\nWHERE pinned = 1\nORDER BY created_at DESC\nLIMIT 20;',
  'git log --oneline --graph --decorate -n 20',
]
const LINKS = [
  'https://developer.apple.com/documentation/appkit/nspasteboard',
  'https://v2.tauri.app/concept/process-model/',
  'https://www.rust-lang.org/learn',
  'https://github.com/tauri-apps/wry/releases',
]
const COLORS = ['#3B82F6', '#F97316', 'rgb(16, 185, 129)', '#A855F7']
const LISTS = [
  '待办：\n1. 回复设计评审的意见\n2. 补齐 OCR 历史的空状态\n3. 复测冷唤出的延迟',
  'Groceries:\n- oat milk\n- coffee beans\n- lemons\n- basil',
]

/** 第 i 条文本（各类内容轮流出现；同一条不会重复——重复内容只会「置顶」已有条目而不新增） */
export function seedTexts(n) {
  const kinds = [PHRASES, PARAGRAPHS, LINKS, CODE, PHRASES, COLORS, LISTS, PARAGRAPHS]
  const out = []
  for (let i = 0; i < n; i++) {
    const pool = kinds[i % kinds.length]
    const round = Math.floor(i / kinds.length)
    const base = pool[(round + i) % pool.length]
    // 轮数后缀保证唯一（链接加查询参数之外的片段，颜色换成相近的色值）
    let text = base
    if (round > 0 || out.includes(text)) {
      if (pool === LINKS) text = `${base}#section-${round + 1}`
      else if (pool === COLORS)
        text = `#${((0x3b82f6 + (i + 1) * 0x0f1e2d) & 0xffffff).toString(16).padStart(6, '0').toUpperCase()}`
      else text = `${base}（${round + 1}）`
    }
    out.push(text)
  }
  // 一段约 4 KB 的长文（列表里的长文本截断与预览）
  if (n >= 10) {
    const parts = []
    for (let k = 0; Buffer.byteLength(parts.join('\n\n')) < 4096; k++)
      parts.push(PARAGRAPHS[k % PARAGRAPHS.length])
    out[n - 1] = parts.join('\n\n')
  }
  return out
}

/** 合成截图式的 PNG（RGB，渐变背景 + 窗口块 + 文字行），返回 Buffer */
export function makePng(width, height, variant = 0) {
  const row = width * 3 + 1
  const raw = Buffer.alloc(row * height)
  const hue = [
    [59, 130, 246],
    [249, 115, 22],
    [16, 185, 129],
    [168, 85, 247],
  ][variant % 4]
  const winX0 = Math.floor(width * 0.12)
  const winX1 = Math.floor(width * 0.88)
  const winY0 = Math.floor(height * 0.14)
  const winY1 = Math.floor(height * 0.86)
  const line = Math.max(6, Math.floor(height / 40))
  for (let y = 0; y < height; y++) {
    raw[y * row] = 0
    for (let x = 0; x < width; x++) {
      const o = y * row + 1 + x * 3
      const g = (x / width + y / height) / 2
      let r = Math.round(hue[0] * (1 - g) + 245 * g)
      let gg = Math.round(hue[1] * (1 - g) + 245 * g)
      let b = Math.round(hue[2] * (1 - g) + 250 * g)
      if (x >= winX0 && x < winX1 && y >= winY0 && y < winY1) {
        // 窗口：浅色底，标题栏，文字行（长短不一）
        r = gg = b = 250
        if (y < winY0 + line * 2) r = gg = b = 232
        else {
          const k = Math.floor((y - winY0 - line * 3) / (line * 2))
          const inLine = (y - winY0 - line * 3) % (line * 2) < line
          const len = 0.35 + ((k * 37 + variant * 11) % 50) / 100
          if (
            k >= 0 &&
            inLine &&
            x < winX0 + line * 2 + (winX1 - winX0 - line * 4) * len &&
            x > winX0 + line * 2
          ) {
            r = gg = b = 60 + ((k * 13) % 40)
          }
        }
      }
      raw[o] = r
      raw[o + 1] = gg
      raw[o + 2] = b
    }
  }
  const chunk = (type, data) => {
    const len = Buffer.alloc(4)
    len.writeUInt32BE(data.length)
    const body = Buffer.concat([Buffer.from(type, 'ascii'), data])
    const crc = Buffer.alloc(4)
    crc.writeUInt32BE(crc32(body))
    return Buffer.concat([len, body, crc])
  }
  const ihdr = Buffer.alloc(13)
  ihdr.writeUInt32BE(width, 0)
  ihdr.writeUInt32BE(height, 4)
  ihdr[8] = 8 // 位深
  ihdr[9] = 2 // RGB
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', deflateSync(raw, { level: 6 })),
    chunk('IEND', Buffer.alloc(0)),
  ])
}

const IMAGE_SIZES = [
  [1600, 1000],
  [800, 600],
  [400, 300],
]

/** 本平台的剪贴板写入方式：`{ writeAll(items, onEach), save(), restore(text) }`；items 为 `{ text }` 或 `{ png: 路径 }` */
function clipboardWriter(os, dir) {
  const check = (r, what) => {
    if (r.error) throw new Error(`${what}：${r.error.message}`)
    if (r.status !== 0)
      throw new Error(
        `${what} 退出码 ${r.status}：${String(r.stderr ?? '')
          .trim()
          .slice(0, 200)}`,
      )
  }
  if (os === 'macos') {
    // pbcopy 按 locale 编码：显式 UTF-8，中文不会乱码
    const env = { ...process.env, LANG: 'en_US.UTF-8', LC_ALL: 'en_US.UTF-8' }
    const one = (item) => {
      if (item.text !== undefined)
        check(spawnSync('pbcopy', [], { input: item.text, env }), 'pbcopy')
      else
        check(
          spawnSync('osascript', [
            '-e',
            `set the clipboard to (read (POSIX file "${item.png}") as «class PNGf»)`,
          ]),
          'osascript（PNG 写入剪贴板）',
        )
    }
    return {
      async writeAll(items, onEach) {
        for (const item of items) {
          one(item)
          await onEach(item)
        }
      },
      save: () => {
        const r = spawnSync('pbpaste', [], { env, encoding: 'utf8' })
        return r.status === 0 && r.stdout ? r.stdout : null
      },
      restore: (text) => one({ text }),
    }
  }
  if (os === 'linux') {
    const xclip = spawnSync('xclip', ['-version'], { stdio: 'ignore' })
    if (xclip.error) throw new Error('Linux 上灌剪贴板需要 xclip（apt install xclip）')
    // xclip 默认 fork 到后台持有选区，直到被下一次写入取代；标准输出 / 错误不接管道，spawnSync 才不会等后台进程
    const one = (item) => {
      let file = item.png
      let args = ['-selection', 'clipboard', '-t', 'image/png', '-i', file]
      if (item.text !== undefined) {
        file = join(dir, 'clip.txt')
        writeFileSync(file, item.text)
        args = ['-selection', 'clipboard', '-t', 'UTF8_STRING', '-i', file]
      }
      check(spawnSync('xclip', args, { stdio: 'ignore' }), 'xclip')
    }
    return {
      async writeAll(items, onEach) {
        for (const item of items) {
          one(item)
          await onEach(item)
        }
      },
      save: () => null,
      restore: (text) => one({ text }),
    }
  }
  // Windows：一个 powershell.exe（STA，System.Windows.Forms 的剪贴板 API 需要）按清单逐条写入、每条之间等待；
  // SetDataObject(…, $true) 让内容在进程退出后仍留在剪贴板上
  return {
    async writeAll(items, onEach, gapMs = 350) {
      const list = items.map((item, k) => {
        if (item.png) return `I\t${item.png}`
        const file = join(dir, `clip-${k}.txt`)
        writeFileSync(file, item.text, 'utf8')
        return `T\t${file}`
      })
      const listFile = join(dir, 'clip-list.txt')
      writeFileSync(listFile, list.join('\r\n'), 'utf8')
      // 脚本写成文件用 -File 执行（-Command 的多行参数要经过 Windows 的命令行转义，引号容易出错）
      const scriptFile = join(dir, 'seed-clipboard.ps1')
      const script = [
        'param([string]$List)',
        "$ErrorActionPreference = 'Stop'",
        'Add-Type -AssemblyName System.Windows.Forms, System.Drawing',
        'foreach ($l in [IO.File]::ReadAllLines($List)) {',
        '  $kind, $path = $l -split "`t", 2',
        '  if ($kind -eq "I") {',
        '    [System.Windows.Forms.Clipboard]::SetDataObject([System.Drawing.Image]::FromFile($path), $true)',
        `    Start-Sleep -Milliseconds ${gapMs * 2}`,
        '  } else {',
        '    [System.Windows.Forms.Clipboard]::SetDataObject([IO.File]::ReadAllText($path, [Text.Encoding]::UTF8), $true)',
        `    Start-Sleep -Milliseconds ${gapMs}`,
        '  }',
        '}',
      ].join('\r\n')
      // UTF-8 带 BOM：Windows PowerShell 5.1 读无 BOM 的脚本按系统代码页解码
      writeFileSync(scriptFile, `\ufeff${script}`, 'utf8')
      check(
        spawnSync(
          'powershell.exe',
          [
            '-NoProfile',
            '-NonInteractive',
            '-STA',
            '-ExecutionPolicy',
            'Bypass',
            '-File',
            scriptFile,
            '-List',
            listFile,
          ],
          { encoding: 'utf8', timeout: 120_000 },
        ),
        'powershell.exe（写入剪贴板）',
      )
      for (const item of items) await onEach(item, false)
    },
    save: () => null,
    restore: () => undefined,
  }
}

// ---------------------------------------------------------------------------
// 应用日志的增量读取

/** 从构造时文件的末尾开始，增量读取新写出的完整行，喂给 PanelTracker */
export class LogTail {
  constructor(path) {
    this.path = path
    this.offset = existsSync(path) ? statSync(path).size : 0
    this.rest = Buffer.alloc(0)
    this.tracker = new PanelTracker()
    this.debug = false
    /** 读到的面板 / 剪贴板事件数（「一段时间内没有新事件」用） */
    this.seq = 0
  }

  poll() {
    let size
    try {
      size = statSync(this.path).size
    } catch {
      return
    }
    // 日志滚动（2 MB × 5）：新文件从头读
    if (size < this.offset) {
      this.offset = 0
      this.rest = Buffer.alloc(0)
    }
    if (size === this.offset) return
    const buf = Buffer.alloc(size - this.offset)
    const fd = openSync(this.path, 'r')
    try {
      readSync(fd, buf, 0, buf.length, this.offset)
    } finally {
      closeSync(fd)
    }
    this.offset = size
    // 按字节找最后一个换行：多字节字符被切在两次读取之间时，不完整的部分留到下一次
    const all = Buffer.concat([this.rest, buf])
    const cut = all.lastIndexOf(0x0a)
    if (cut < 0) {
      this.rest = all
      return
    }
    this.rest = all.subarray(cut + 1)
    const { events, debug } = parsePanelEvents(all.subarray(0, cut).toString('utf8'))
    if (debug) this.debug = true
    for (const e of events) {
      this.tracker.add(e)
      this.seq++
    }
  }
}

// ---------------------------------------------------------------------------
// exercise

export class Exercise {
  /**
   * @param {object} o
   * @param {LogTail} o.tail
   * @param {(action: string) => void} o.trigger  经第二实例转发一个动作（失败时抛错）
   * @param {number} [o.holdMs]  每个视图上屏后停留
   * @param {number} [o.quietMs]  「静下来」：没有进行中的唤出、这么久没有新的面板事件
   * @param {number} [o.timeoutMs]  等上屏 / 隐藏的上限
   * @param {(msg: string) => void} [o.log]
   */
  constructor({ tail, trigger, holdMs = 1500, quietMs = 600, timeoutMs = 10_000, log = info }) {
    Object.assign(this, { tail, trigger, holdMs, quietMs, timeoutMs, log })
  }

  get tracker() {
    return this.tail.tracker
  }

  /** 轮询日志直到 pred() 为真，返回它的值；超时抛错 */
  async waitFor(pred, what, timeoutMs = this.timeoutMs) {
    const deadline = Date.now() + timeoutMs
    for (;;) {
      this.tail.poll()
      const v = pred()
      if (v) return v
      if (Date.now() > deadline) {
        const hint = this.tail.debug
          ? ''
          : '；日志里没有 debug 级别的行——advanced.log_level 不是 debug？（measure-*.sh 在隔离 HOME 里会预置；--home keep / Windows 的真实目录要自己打开）'
        throw new Error(`${what}（等了 ${timeoutMs / 1000} s）${hint}`)
      }
      await sleep(50)
    }
  }

  /** 静下来：没有进行中的唤出，且 quietMs 内没有新的面板事件 */
  async settle() {
    let seq = -1
    let since = Date.now()
    await this.waitFor(() => {
      if (this.tail.seq !== seq) {
        seq = this.tail.seq
        since = Date.now()
      }
      return this.tracker.pending().length === 0 && Date.now() - since >= this.quietMs
    }, '面板事件没有静下来（一直有唤出在进行）')
  }

  /** 触发 action，等一个新会话上屏，返回它（上屏的不是这个视图时失败） */
  async open(view) {
    const mark = this.tracker.shown().length
    this.trigger(VIEW_ACTION[view])
    const s = await this.waitFor(
      () => this.tracker.shown().slice(mark).at(-1),
      `--trigger ${VIEW_ACTION[view]} 之后 ${view} 面板没有上屏`,
    )
    if (s.view !== null && s.view !== view) {
      throw new Error(`--trigger ${VIEW_ACTION[view]} 之后上屏的是 ${s.view} 视图（会话 ${s.id}）`)
    }
    return s
  }

  /** 使用一个视图：唤出 → 停留 → 关闭；被重新打开（失焦先隐藏了、触发又把它打开）时再关，直到没有面板在屏幕上 */
  async use(view) {
    if (!VIEW_ACTION[view])
      throw new Error(`不支持的视图 ${view}（可用：${Object.keys(VIEW_ACTION).join(' / ')}）`)
    let s = await this.open(view)
    this.log(`${view}：会话 ${s.id} 上屏`)
    for (let round = 1; ; round++) {
      await sleep(this.holdMs)
      this.tail.poll()
      if (s.ended !== null) {
        this.log(`${view}：会话 ${s.id} 在停留期间已隐藏（${s.reason ?? '原因未记录'}），不再触发`)
      } else {
        const mark = this.tracker.shown().length
        this.trigger(VIEW_ACTION[view])
        await this.waitFor(
          () => s.ended !== null || this.tracker.shown().length > mark,
          `再次 --trigger ${VIEW_ACTION[view]} 之后，${view} 面板（会话 ${s.id}）既没有隐藏也没有重新打开`,
        )
      }
      await this.settle()
      const visible = this.tracker.visibleAt()
      if (visible.length === 0) return
      if (round >= 3) {
        throw new Error(
          `${view} 面板关了 3 轮仍在屏幕上（会话 ${visible.map((v) => v.id).join('、')}）`,
        )
      }
      const prev = s
      s = visible.at(-1)
      this.log(
        `${view}：会话 ${prev.id} 先因 ${prev.reason ?? '未知原因'} 隐藏、触发又打开了会话 ${s.id}（第 ${round} 轮），再关一次`,
      )
    }
  }

  /** 依次使用 views，最后核对没有面板在屏幕上 */
  async run(views) {
    for (const [k, view] of views.entries()) {
      // 视图之间留一点间隔（最后一个视图之后不等：最后一次隐藏尽量贴近 exercise 的结束，回收的时刻随之可预期）
      if (k > 0) await sleep(Math.min(this.holdMs, 1000))
      await this.use(view)
    }
    await this.settle()
    const visible = this.tracker.visibleAt()
    if (visible.length)
      throw new Error(
        `exercise 结束时仍有面板在屏幕上（会话 ${visible.map((v) => v.id).join('、')}）`,
      )
  }

  /** 一行摘要 */
  summary() {
    const shown = this.tracker.shown()
    const hides = Object.entries(this.tracker.hideReasons()).map(([k, n]) => `${k} ×${n}`)
    const clips = Object.entries(this.tracker.clips).map(([k, n]) => `${k} ${n}`)
    return `唤出 ${shown.length} 次（${shown.map((s) => s.view ?? '?').join('、')}），隐藏 ${hides.join('、') || '—'}；剪贴板记录 ${this.tracker.clipTotal()}${clips.length ? `（${clips.join('、')}）` : ''}`
  }
}

/**
 * 灌剪贴板：写入 items，等应用日志里出现同样多的记录（新条目，或与历史里已有内容相同而被置顶的 `touched`）。
 * 有 touched 时告警：历史没有清空，剪贴板视图渲染的不是全新的列表（scripts/ci/panel-recycle-windows.ps1 每段之前清空历史）
 */
async function seedClipboard(ex, { texts, images, optional, restore, os }) {
  const dir = mkdtempSync(join(tmpdir(), 'wisp-exercise-'))
  try {
    const items = seedTexts(texts).map((text) => ({ text }))
    for (let k = 0; k < images; k++) {
      const [w, h] = IMAGE_SIZES[k % IMAGE_SIZES.length]
      const png = join(dir, `shot-${k}.png`)
      writeFileSync(png, makePng(w, h, k))
      items.push({ png })
    }
    if (items.length === 0) return
    const writer = clipboardWriter(os, dir)
    const saved = restore ? writer.save() : null
    const before = ex.tracker.clipTotal()
    const touchedBefore = ex.tracker.clipTouched()
    const t0 = Date.now()
    let first = true
    await writer.writeAll(items, async (item, wait = true) => {
      ex.tail.poll()
      if (wait && first) {
        // 第一条等应用记下再继续：macOS 的 changeCount 轮询在没人碰键盘时放慢到 1 s / 5 s 一次，
        // 看到变化后回到 4 Hz；不等的话，第一次 tick 之前写入的几条会被后面的覆盖、永远记不到
        first = false
        await ex.waitFor(
          () => ex.tracker.clipTotal() - before >= 1,
          '剪贴板写入的第一条应用没有记录',
          12_000,
        )
      } else if (wait) await sleep(item.png ? 700 : 350)
    })
    const want = items.length
    try {
      await ex.waitFor(
        () => ex.tracker.clipTotal() - before >= want,
        `剪贴板写入了 ${want} 条，应用只记录了 ${ex.tracker.clipTotal() - before} 条`,
      )
      ex.log(
        `剪贴板：写入 ${texts} 条文本、${images} 张图片，应用全部记录（${((Date.now() - t0) / 1000).toFixed(1)} s）`,
      )
      const touched = ex.tracker.clipTouched() - touchedBefore
      if (touched > 0) {
        warn(
          `剪贴板：其中 ${touched} 条与历史里已有的条目相同，应用只把它们置顶（clipboard item touched）、没有新增——` +
            '历史没有清空（例如 Windows 上用真实用户目录连续测量），剪贴板视图渲染的不是全新的列表',
        )
      }
    } catch (e) {
      if (!optional) throw e
      warn(`${e.message}（--seed-optional：继续）`)
    }
    if (saved !== null) {
      writer.restore(saved)
      ex.log('剪贴板：已写回原来的文本')
    } else if (restore) {
      warn('原来的剪贴板里没有文本（或无法读取），没有写回')
    }
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
}

// ---------------------------------------------------------------------------
// 自检

/** Rust 源码里新配置的平台默认值（自检核对 UNLOAD_PANEL_DEFAULT 没有漂移） */
function unloadDefaultsFromRust() {
  const src = readFileSync(join(ROOT, 'crates', 'wisp-types', 'src', 'defaults.rs'), 'utf8')
  const n = Number(/pub const UNLOAD_PANEL_SUGGESTED_MIN: u32 = (\d+);/.exec(src)?.[1])
  const body =
    /pub const fn unload_panel_default\(os: OsKind\) -> Option<u32> \{([\s\S]*?)\n\}/.exec(
      src,
    )?.[1] ?? ''
  const out = {}
  for (const [, oses, value] of body.matchAll(/((?:OsKind::\w+\s*\|?\s*)+)=>\s*([^,\n]+)/g)) {
    const v = value.trim()
    const parsed =
      v === 'None'
        ? null
        : v === 'Some(UNLOAD_PANEL_SUGGESTED_MIN)'
          ? n
          : Number(/Some\((\d+)\)/.exec(v)?.[1])
    for (const [, os] of oses.matchAll(/OsKind::(\w+)/g)) out[os.toLowerCase()] = parsed
  }
  return out
}

async function selfTest() {
  // 设置：平台默认值与 Rust 同步；合并保留其他字段
  assert.deepEqual(
    unloadDefaultsFromRust(),
    UNLOAD_PANEL_DEFAULT,
    'UNLOAD_PANEL_DEFAULT 与 defaults::unload_panel_default 不一致',
  )
  const merged = measurementSettings(
    { general: { ui_language: 'en_us', update_policy: 'notify' }, glossary: 1 },
    'macos',
  )
  assert.equal(merged.general.ui_language, 'en_us')
  assert.equal(merged.general.update_policy, 'off')
  assert.equal(merged.general.launch_at_login, false)
  assert.equal(merged.advanced.log_level, 'debug')
  assert.equal(merged.advanced.unload_panel_after_idle_min, UNLOAD_PANEL_DEFAULT.macos)
  assert.equal(merged.glossary, 1)
  assert.equal(measurementSettings(null, 'linux').advanced.unload_panel_after_idle_min, null)

  // 文本：条数、唯一、含长文
  const texts = seedTexts(30)
  assert.equal(texts.length, 30)
  assert.equal(new Set(texts).size, 30, '文本有重复（重复只会置顶已有条目）')
  assert.ok(Buffer.byteLength(texts.at(-1)) >= 4096)
  assert.ok(texts.some((t) => t.startsWith('https://')) && texts.some((t) => t.includes('\n')))

  // PNG：签名、IHDR、每个块的 CRC
  const png = makePng(64, 40, 1)
  assert.deepEqual([...png.subarray(0, 8)], [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])
  let off = 8
  const types = []
  while (off < png.length) {
    const len = png.readUInt32BE(off)
    const body = png.subarray(off + 4, off + 8 + len)
    assert.equal(png.readUInt32BE(off + 8 + len), crc32(body), 'PNG 块的 CRC')
    types.push(body.subarray(0, 4).toString('ascii'))
    off += 12 + len
  }
  assert.deepEqual(types, ['IHDR', 'IDAT', 'IEND'])
  assert.equal(png.readUInt32BE(16), 64)

  // 假的应用：按触发写 debug 日志。第一次触发上屏会话 1；第二次触发之前会话 1 已因失焦隐藏，触发又打开了会话 2；
  // 第三次触发 Toggle 关闭会话 2。剪贴板视图第一次就正常关闭
  const dir = mkdtempSync(join(tmpdir(), 'wisp-exercise-test-'))
  try {
    const log = join(dir, 'wisp.log')
    writeFileSync(log, '2026-10-12T00:00:00.000Z INFO  wisp_desktop_lib::app: 以前的内容\n')
    const tail = new LogTail(log)
    let session = 0
    let visible = null
    const write = (lines) => {
      const ts = new Date().toISOString()
      appendFileSync(
        log,
        lines.map((l) => `${ts} DEBUG wisp_desktop_lib::windows::panel: ${l}\n`).join(''),
      )
    }
    const views = { translate_input: 'Input', clipboard: 'Clipboard' }
    let inputTriggers = 0
    const calls = []
    const trigger = (action) => {
      calls.push(action)
      if (action === 'translate_input') inputTriggers++
      if (action === 'translate_input' && inputTriggers === 2) {
        // 第二实例启动时面板失焦先隐藏，转发到的触发又把面板打开
        write([
          `panel hide session=${visible} reason=Blur origin=frontend`,
          `panel session=${visible} ended`,
        ])
        visible = null
      }
      if (visible !== null) {
        write([
          `panel hide session=${visible} reason=Toggle origin=rust`,
          `panel session=${visible} ended`,
        ])
        visible = null
        return
      }
      session++
      visible = session
      write([
        `panel_prepare session=${session} view=${views[action]} placement=Center width=420 max_height=600`,
        // 多字节字符跨两次读取：LogTail 按字节切行
        `panel session=${session} shown 40 ms after prepare（上屏）`,
      ])
    }
    const lines = []
    const ex = new Exercise({
      tail,
      trigger,
      holdMs: 30,
      quietMs: 80,
      timeoutMs: 2000,
      log: (m) => lines.push(m),
    })
    await ex.run(['input', 'clipboard'])
    assert.deepEqual(calls, [
      'translate_input',
      'translate_input',
      'translate_input',
      'clipboard',
      'clipboard',
    ])
    assert.equal(ex.tracker.visibleAt().length, 0)
    assert.deepEqual(
      ex.tracker.shown().map((s) => s.view),
      ['input', 'input', 'clipboard'],
    )
    assert.deepEqual(ex.tracker.hideReasons(), { blur: 1, toggle: 2 })
    assert.ok(
      lines.some((l) => l.includes('再关一次')),
      lines.join('\n'),
    )

    // 剪贴板：新条目与「置顶已有条目」都算应用记录了这次复制（面板回收对照的 recycled 段：历史没清空时 33 条全是 touched）
    const engine = (l) =>
      `${new Date().toISOString()} DEBUG wisp_desktop_lib::clipboard_svc::engine: ${l}\n`
    appendFileSync(
      log,
      engine('clipboard item inserted id=7 kind=Link') + engine('clipboard item touched id=3'),
    )
    tail.poll()
    assert.deepEqual(ex.tracker.clips, { link: 1, touched: 1 })
    assert.equal(ex.tracker.clipTotal(), 2)
    assert.equal(ex.tracker.clipTouched(), 1)

    // 面板始终关不掉：3 轮后失败
    const stuck = new LogTail(log)
    let n = 100
    const reopen = () => {
      n++
      write([`panel session=${n - 1} ended`, `panel session=${n} shown 40 ms after prepare`])
    }
    const ex2 = new Exercise({
      tail: stuck,
      trigger: reopen,
      holdMs: 10,
      quietMs: 50,
      timeoutMs: 1000,
      log: () => {},
    })
    await assert.rejects(ex2.use('input'), /关了 3 轮仍在屏幕上/)

    // 没有 debug 日志：超时信息提示日志级别
    const silent = new Exercise({
      tail: new LogTail(log),
      trigger: () => {},
      timeoutMs: 200,
      log: () => {},
    })
    await assert.rejects(silent.open('input'), /log_level 不是 debug/)

    // --write-settings：合并已有文件
    const settings = join(dir, 'cfg', 'settings.json')
    mkdirSync(dirname(settings), { recursive: true })
    writeFileSync(settings, JSON.stringify({ schema_version: 1, appearance: { theme: 'dark' } }))
    writeSettings(settings, 'windows')
    const written = JSON.parse(readFileSync(settings, 'utf8'))
    assert.equal(written.appearance.theme, 'dark')
    assert.equal(written.advanced.log_level, 'debug')
    assert.equal(written.advanced.unload_panel_after_idle_min, null)
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
  ok(
    'exercise 自检通过（测量设置与 Rust 默认值同步、剪贴板画像、PNG、失焦后被重新打开时再关一次、关不掉时失败、置顶的已有条目计入记录）',
  )
}

// ---------------------------------------------------------------------------

async function main() {
  const { values } = parseArgs({
    options: {
      views: { type: 'string', default: 'input,clipboard' },
      hold: { type: 'string', default: '1.5' },
      texts: { type: 'string', default: '30' },
      images: { type: 'string', default: '3' },
      'seed-optional': { type: 'boolean', default: false },
      'restore-clipboard': { type: 'string', default: 'auto' },
      tail: { type: 'string', default: '0.5' },
      'write-settings': { type: 'string' },
      'self-test': { type: 'boolean', default: false },
      help: { type: 'boolean', short: 'h', default: false },
    },
  })
  if (values.help) {
    console.log(readFileSync(new URL(import.meta.url), 'utf8').split('*/')[0])
    return
  }
  if (values['self-test']) return selfTest()
  if (values['write-settings']) return writeSettings(values['write-settings'])

  const num = (name, min) => {
    const v = Number(values[name])
    if (!Number.isFinite(v) || v < min) throw new Error(`--${name} 必须是 ≥ ${min} 的数`)
    return v
  }
  const bin = process.env.WISP_BIN
  const logPath = process.env.WISP_LOG
  if (!bin) throw new Error('WISP_BIN 未设置（在 measure-*.sh 的 --exercise 里运行）')
  if (!logPath)
    throw new Error(
      'WISP_LOG 未设置：exercise 要读被测实例的应用日志核对每一步（measure-*.sh 会给出）',
    )
  const views = values.views
    .split(',')
    .map((v) => v.trim())
    .filter(Boolean)
  for (const v of views)
    if (!VIEW_ACTION[v])
      throw new Error(`不支持的视图 ${v}（可用：${Object.keys(VIEW_ACTION).join(' / ')}）`)
  const restoreOpt = values['restore-clipboard']
  if (!['auto', 'yes', 'no'].includes(restoreOpt))
    throw new Error('--restore-clipboard 只能是 auto / yes / no')
  const os = currentOs()

  const trigger = (action) => {
    // 第二实例转发完就退出；GUI 子系统程序在 Windows 上同样能等到退出码
    const r = spawnSync(bin, ['--trigger', action], { stdio: 'ignore', timeout: 20_000 })
    if (r.error) throw new Error(`${bin} --trigger ${action}：${r.error.message}`)
    if (r.status !== 0) throw new Error(`${bin} --trigger ${action} 退出码 ${r.status}`)
  }
  const ex = new Exercise({ tail: new LogTail(logPath), trigger, holdMs: num('hold', 0) * 1000 })
  await seedClipboard(ex, {
    texts: Math.floor(num('texts', 0)),
    images: Math.floor(num('images', 0)),
    optional: values['seed-optional'],
    restore: restoreOpt === 'yes' || (restoreOpt === 'auto' && !process.env.CI),
    os,
  })
  await ex.run(views)
  ok(`exercise：${ex.summary()}`)
  await sleep(num('tail', 0) * 1000)
}

if (isMain(import.meta.url)) await run(main)
