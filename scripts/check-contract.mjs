#!/usr/bin/env node
/**
 * scripts/check-contract.mjs：IPC 契约与代码约束的静态检查。
 *
 * 用法：node scripts/check-contract.mjs [--rust] [--no-cargo] | --self-test
 *   --rust      另外运行 Rust 侧的契约测试（`cargo test -p wisp-desktop --lib ipc::contract_tests` 与
 *               `cargo test -p wisp-types`），相当于提交前的完整契约检查；CI 的 test 作业已经覆盖，默认不跑
 *   --no-cargo  跳过需要 cargo 的 crate 依赖边界检查（只改前端时可用）
 *
 * 检查项（秒级完成，失败时给出文件与行号）：
 *   ① 每个 `#[tauri::command]` 都写成 `#[tauri::command(rename_all = "snake_case")]`；
 *   ② `ipc/mod.rs` 的 `commands![…]` 列表 == 全部 `#[tauri::command]` 函数 == 前端 `COMMAND_NAMES`
 *      （参数键、流式命令、参数校验等更细的比对由 Rust 的 `ipc::contract_tests` 负责，见 --rust）；
 *   ③ `@tauri-apps/*` 只能在 `shared/ipc/tauri.ts`（core / event / window）与 `shared/ipc/runtime.ts`（core）中
 *      引用（Tauri capability 只放行事件监听与窗口拖动 / 关闭，其余窗口能力一律走 Rust 命令）；
 *   ④ 动效时长 / 曲线不得出现魔法数：只能引用 `shared/motion/tokens.ts`、`presets.ts` 或 CSS 变量
 *      `--dur-*` / `--ease-*`（动效数值集中在令牌里统一调整；Tailwind 任意值 `duration-[var(--dur-*)]`、`ease-[var(--ease-*)]` 是正确写法）。
 *      Web Animations API（`el.animate(kf, options)`，Overlay 用）的数字时长与字面量缓动同样算；Tailwind 内置动画
 *      `animate-spin` / `pulse` / `ping` / `bounce`（自带 1 s / 2 s 与固定曲线）只有在项目 CSS 用 `--animate-<名>:` 以
 *      令牌重新定义后才能用，`--animate-*` 的定义本身按 CSS 动画规则检查。确有必要时在同一行或上一行写注释
 *      `motion-ok: <原因>` 豁免；
 *   ⑤ crate 依赖边界：除 wisp-desktop 外，任何 workspace crate（含 dev / build 依赖）都不能依赖
 *      tauri / tao / wry（保证 crates/* 无头可测）。
 * 事件名由 wisp-types 的黄金测试 `event_names_match_frontend` 比对，不在这里重复。
 */
import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { parseArgs } from 'node:util'
import {
  APP_DIR,
  ROOT,
  TAURI_DIR,
  error,
  isMain,
  lineOf,
  ok,
  rel,
  run,
  stripJsComments,
  walk,
  warn,
} from './lib/common.mjs'

const SRC = join(APP_DIR, 'src')
const skipDirs = (name) => name === 'node_modules' || name === 'dist' || name === 'target'

// ---------------------------------------------------------------------------
// ① ② 命令

/** Rust 源码中的全部命令：`{ name, file, line, attr }` */
export function rustCommands(files) {
  const out = []
  for (const file of files) {
    const text = readFileSync(file, 'utf8')
    for (const m of text.matchAll(
      /#\[tauri::command(\([^)]*\))?\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([a-z0-9_]+)/g,
    )) {
      out.push({ name: m[2], attr: m[1] ?? '', file, line: lineOf(text, m.index) })
    }
  }
  return out
}

/** `commands![ module::name, … ]` 中的命令名 */
export function macroCommands(modRs) {
  const m = /^commands!\[([\s\S]*?)^\];/m.exec(modRs)
  if (!m) return null
  const body = m[1].replace(/\/\/[^\n]*/g, '')
  return [...body.matchAll(/\b([a-z0-9_]+)::([a-z0-9_]+)\b/g)].map((x) => x[2])
}

/** TS 文件中 `COMMAND_NAMES = [ '…', … ]` 或 `OVERLAY_COMMAND_NAMES` 的字符串项 */
export function tsCommandNames(text) {
  const names = []
  for (const m of text.matchAll(/\b(?:[A-Z_]*COMMAND_NAMES)\s*(?::[^=]+)?=\s*\[([\s\S]*?)\]/g)) {
    for (const s of m[1].matchAll(/'([a-z0-9_]+)'|"([a-z0-9_]+)"/g)) names.push(s[1] ?? s[2])
  }
  return names
}

function checkCommands(problems) {
  const rsFiles = walk(join(TAURI_DIR, 'src'), skipDirs).filter((f) => f.endsWith('.rs'))
  const cmds = rustCommands(rsFiles)
  for (const c of cmds) {
    if (!/rename_all\s*=\s*"snake_case"/.test(c.attr)) {
      problems.push({
        file: rel(c.file),
        line: c.line,
        msg: `命令 ${c.name} 必须写成 #[tauri::command(rename_all = "snake_case")]（前后端参数键统一用 snake_case）`,
      })
    }
  }
  const modPath = join(TAURI_DIR, 'src', 'ipc', 'mod.rs')
  const listed = macroCommands(readFileSync(modPath, 'utf8'))
  if (!listed) {
    problems.push({ file: rel(modPath), msg: '找不到 commands![…] 命令清单' })
    return 0
  }
  const tsFile = join(SRC, 'shared', 'ipc', 'commands.ts')
  const ts = new Set(tsCommandNames(readFileSync(tsFile, 'utf8')))
  const rust = new Set(cmds.map((c) => c.name))
  const reg = new Set(listed)
  const diff = (a, b) => [...a].filter((x) => !b.has(x)).toSorted()
  const report = (names, file, msg) => {
    if (names.length) problems.push({ file, msg: `${msg}：${names.join('、')}` })
  }
  report(diff(rust, reg), rel(modPath), '有 #[tauri::command] 但没有登记在 commands![…]')
  report(diff(reg, rust), rel(modPath), 'commands![…] 里登记了不存在的命令')
  report(diff(reg, ts), rel(tsFile), 'Rust 有、前端 COMMAND_NAMES 没有的命令')
  report(diff(ts, reg), rel(tsFile), '前端 COMMAND_NAMES 有、Rust 没有的命令')
  const dup = listed.filter((n, i) => listed.indexOf(n) !== i)
  report(dup, rel(modPath), 'commands![…] 里重复登记')
  return reg.size
}

// ---------------------------------------------------------------------------
// ③ @tauri-apps 导入边界（与 shared/ipc/__tests__/tauri-imports.test.ts 同一份白名单）

export const TAURI_ALLOWED = {
  'shared/ipc/tauri.ts': [
    '@tauri-apps/api/core',
    '@tauri-apps/api/event',
    '@tauri-apps/api/window',
  ],
  'shared/ipc/runtime.ts': ['@tauri-apps/api/core'],
}
const TAURI_SPECIFIER =
  /(?:\bfrom\s*|\bimport\s*\(\s*|\bimport\s+|\bvi\.(?:mock|doMock)\s*\(\s*)(['"])(@tauri-apps\/[^'"]+)\1/g

function checkTauriImports(files, problems) {
  for (const file of files) {
    const relSrc = file
      .slice(SRC.length + 1)
      .split('\\')
      .join('/')
    if (relSrc === 'shared/ipc/__tests__/tauri-imports.test.ts') continue
    const text = readFileSync(file, 'utf8')
    for (const m of text.matchAll(TAURI_SPECIFIER)) {
      const allowed = TAURI_ALLOWED[relSrc] ?? []
      if (!allowed.includes(m[2])) {
        problems.push({
          file: rel(file),
          line: lineOf(text, m.index),
          msg: `不得引用 ${m[2]}：@tauri-apps/* 只能出现在 shared/ipc/tauri.ts 与 runtime.ts（窗口能力一律走 Rust 命令）`,
        })
      }
    }
  }
}

// ---------------------------------------------------------------------------
// ④ 动效魔法数

/** 不检查的路径（相对 src）：令牌与预设本身、浏览器 mock（模拟的是原生窗口动画）、测试 */
function motionExempt(relSrc) {
  return (
    relSrc.startsWith('shared/motion/') ||
    relSrc === 'shared/styles/tokens.css' ||
    relSrc.startsWith('shared/ipc/mock/') ||
    relSrc.startsWith('test/') ||
    relSrc.includes('/__tests__/') ||
    /\.test\.tsx?$/.test(relSrc)
  )
}

const MOTION_IMPORT = /\bfrom\s*['"](?:motion(?:\/[a-z-]+)?|framer-motion)['"]/
/** 只在导入了 motion 的文件里检查（这些键名在别处可能是业务字段，例如 toast 的显示时长） */
const MOTION_RULES = [
  [
    /\b(duration|delay|repeatDelay|visualDuration)\s*:\s*(?!0(?![.\d]))\d*\.?\d+/g,
    '动画时长写成了数字',
  ],
  [/\b(stiffness|damping|mass|bounce)\s*:\s*\d/g, '弹簧参数写成了数字'],
  [/\bease\s*:\s*\[/g, '缓动曲线写成了数组'],
  [
    /\bease\s*:\s*['"](?:easeIn|easeOut|easeInOut|circIn|circOut|circInOut|backIn|backOut|backInOut|anticipate)['"]/g,
    '缓动用了 Motion 内置曲线',
  ],
]
/** Tailwind 内置的动画工具类：自带写死的时长与曲线（spin 1s linear、ping 1s、pulse 2s、bounce 1s 的 cubic-bezier） */
const BUILTIN_ANIMATION = /(?<![\w-])animate-(spin|ping|pulse|bounce)(?![\w-])/g
const ANY_RULES = [
  [/cubic-bezier\(/g, 'cubic-bezier 曲线（应使用 --ease-* 或 tokens.ease）'],
  // 用令牌写的任意值（`duration-[var(--dur-fast)]`、`ease-[var(--ease-standard)]`）与 Tailwind 4 的变量简写
  // （`duration-(--dur-fast)`）是正确写法，不算魔法数
  [
    /(?<![\w-])(?:duration|delay)-(?:\d+|\[(?!var\(--dur-[\w-]+\)\])[^\]]*\])(?![\w-])/g,
    'Tailwind 时长类（应使用 --dur-* 令牌）',
  ],
  [
    /(?<![\w-])ease-(?:in|out|in-out|\[(?!var\(--ease-[\w-]+\)\])[^\]]*\])(?![\w-])/g,
    'Tailwind 缓动类（应使用 --ease-* 令牌）',
  ],
  // 任意值动画 `animate-[spin_1s_linear_infinite]`：数字时长（下划线代替空格）
  [
    /(?<![\w-])animate-\[[^\]]*?(?<=[[_,(])\d*\.?\d+m?s(?=[\]_,)])[^\]]*\]/g,
    'Tailwind 任意值动画写了时长（应使用 --dur-* 令牌，或在 @theme 里用令牌定义 --animate-*）',
  ],
  [
    /\b(?:transition|animation)(?:Duration|Delay)?\s*[:=]\s*['"`][^'"`]*?\b(?!0m?s\b)\d*\.?\d+m?s\b/g,
    '内联样式里写了时长',
  ],
]
/** CSS 里定义动画 / 过渡的属性，含 Tailwind 4 `@theme` 里的 `--animate-<名>:`（animate-<名> 类的定义） */
const CSS_ANIMATION_PROP = String.raw`(?:\b(?:transition|animation)|--animate-[\w-]+)`
const CSS_RULES = [
  [
    new RegExp(
      String.raw`${CSS_ANIMATION_PROP}(?:-duration|-delay)?\s*:[^;{}]*?(?<![\w-])(?!0m?s\b)\d*\.?\d+m?s\b`,
      'g',
    ),
    'CSS 过渡 / 动画时长写成了数字（应使用 var(--dur-*)）',
  ],
  [
    new RegExp(
      String.raw`${CSS_ANIMATION_PROP}(?:-timing-function)?\s*:[^;{}]*?(?<![\w-])(?:ease|ease-in|ease-out|ease-in-out)(?![\w-])`,
      'g',
    ),
    'CSS 缓动用了内置关键字（应使用 var(--ease-*)）',
  ],
]

/** 项目 CSS 里用 `--animate-<名>:` 定义的动画名（Tailwind 4 `@theme`）：这些 animate-<名> 类引用的是项目自己的定义 */
export function themedAnimations(cssTexts) {
  const names = new Set()
  for (const text of cssTexts) {
    for (const m of stripComments(text, true).matchAll(/--animate-([\w-]+)\s*:/g)) names.add(m[1])
  }
  return names
}

/** 去掉注释，保留换行与长度不变（行号仍然准确）；JS / TS 用跳过字符串、模板与正则字面量的词法剥离 */
function stripComments(text, css) {
  if (!css) return stripJsComments(text)
  return text.replace(/\/\*[\s\S]*?\*\//g, (m) => m.replace(/[^\n]/g, ' '))
}

/** 从 `open`（左圆括号的位置）找到配对的右圆括号，跳过字符串与模板字面量；找不到时返回 -1 */
function closingParen(code, open) {
  let depth = 0
  for (let i = open; i < code.length; i++) {
    const c = code[i]
    if (c === '"' || c === "'" || c === '`') {
      for (i++; i < code.length && code[i] !== c; i++) if (code[i] === '\\') i++
    } else if (c === '(' || c === '[' || c === '{') depth++
    else if (c === ')' || c === ']' || c === '}') {
      if (--depth === 0) return c === ')' ? i : -1
    }
  }
  return -1
}

/** 按顶层逗号切分参数列表（`from`..`to` 之间），返回每个参数的 `[起点, 终点)` */
function splitArgs(code, from, to) {
  const out = []
  let depth = 0
  let start = from
  for (let i = from; i < to; i++) {
    const c = code[i]
    if (c === '"' || c === "'" || c === '`') {
      for (i++; i < to && code[i] !== c; i++) if (code[i] === '\\') i++
    } else if (c === '(' || c === '[' || c === '{') depth++
    else if (c === ')' || c === ']' || c === '}') depth--
    else if (c === ',' && depth === 0) {
      out.push([start, i])
      start = i + 1
    }
  }
  if (code.slice(start, to).trim()) out.push([start, to])
  return out
}

/**
 * Web Animations API：`el.animate(keyframes, options)` 的第二个参数（Overlay 的飞入动画用它）。数字形式的
 * 时长（`animate(kf, 180)`）、选项里数字的 `duration` / `delay` / `endDelay`、字面量的 `easing` 都是魔法数——
 * 时长用 `ms(duration.*)`，曲线用由 `tokens.ease` 生成的 cubic-bezier。返回 `[{ index, text, msg }]`
 */
export function waapiViolations(code) {
  const found = []
  for (const m of code.matchAll(/\.animate\s*\(/g)) {
    const open = m.index + m[0].length - 1
    const close = closingParen(code, open)
    if (close < 0) continue
    const opts = splitArgs(code, open + 1, close)[1]
    if (!opts) continue
    const [from, to] = opts
    const arg = code.slice(from, to)
    const lead = arg.length - arg.trimStart().length
    if (/^\s*\d*\.?\d+\s*$/.test(arg) && !/^\s*0+(?:\.0*)?\s*$/.test(arg)) {
      found.push({ index: from + lead, text: arg.trim(), msg: 'WAAPI 时长写成了数字' })
      continue
    }
    if (!arg.trimStart().startsWith('{')) continue
    for (const k of arg.matchAll(/\b(duration|delay|endDelay)\s*:\s*(?!0(?![.\d]))\d*\.?\d+/g)) {
      found.push({ index: from + k.index, text: k[0], msg: 'WAAPI 时长写成了数字' })
    }
    for (const k of arg.matchAll(/\beasing\s*:\s*['"`][^'"`]*['"`]/g)) {
      found.push({ index: from + k.index, text: k[0], msg: 'WAAPI 缓动写成了字面量' })
    }
  }
  return found
}

/**
 * 一个文件里的动效魔法数，返回 `[{ line, msg }]`。`themed` 是项目 CSS 里用 `--animate-<名>:` 重新定义过的动画名
 * （themedAnimations）：这些内置名的 animate-* 类不算魔法数
 */
export function motionViolations(text, relSrc, { themed = new Set() } = {}) {
  if (motionExempt(relSrc)) return []
  const css = relSrc.endsWith('.css')
  const lines = text.split('\n')
  const code = stripComments(text, css)
  const rules = css ? CSS_RULES : [...(MOTION_IMPORT.test(code) ? MOTION_RULES : []), ...ANY_RULES]
  const hits = []
  for (const [re, msg] of rules) {
    for (const m of code.matchAll(re)) hits.push({ index: m.index, text: m[0], msg })
  }
  if (!css) {
    hits.push(...waapiViolations(code))
    for (const m of code.matchAll(BUILTIN_ANIMATION)) {
      if (themed.has(m[1])) continue
      hits.push({
        index: m.index,
        text: m[0],
        msg: `Tailwind 内置动画（自带写死的时长与曲线；需在 @theme 里用令牌定义 --animate-${m[1]}）`,
      })
    }
  }
  const found = []
  let covered = -1
  for (const h of hits.toSorted((a, b) => a.index - b.index || b.text.length - a.text.length)) {
    // 同一处只报一次：导入了 motion 的文件里 WAAPI 的 duration 也会被 Motion 规则命中，
    // `easing: 'ease-out'` 里的 ease-out 也会被 Tailwind 规则命中
    // （CSS 规则都从属性名开始匹配，时长与缓动是两处不同的问题，不合并）
    if (!css && h.index < covered) continue
    covered = h.index + h.text.length
    const line = lineOf(code, h.index)
    // 豁免：同一行或上一行有 motion-ok 注释
    if (/motion-ok/.test(lines[line - 1] ?? '') || /motion-ok/.test(lines[line - 2] ?? '')) continue
    found.push({ line, msg: `${h.msg}：${h.text.trim().slice(0, 60)}` })
  }
  return found
}

function checkMotion(files, problems) {
  const texts = new Map(files.map((f) => [f, readFileSync(f, 'utf8')]))
  const themed = themedAnimations(files.filter((f) => f.endsWith('.css')).map((f) => texts.get(f)))
  for (const file of files) {
    const relSrc = file
      .slice(SRC.length + 1)
      .split('\\')
      .join('/')
    for (const v of motionViolations(texts.get(file), relSrc, { themed })) {
      problems.push({ file: rel(file), line: v.line, msg: `动效魔法数——${v.msg}` })
    }
  }
}

// ---------------------------------------------------------------------------
// ⑤ crate 依赖边界

const GUI_CRATES = new Set(['tauri', 'tao', 'wry'])

/** 从 cargo metadata 找出依赖了 GUI crate 的非 desktop 成员；返回 `[{ member, path }]` */
export function guiLeaks(meta) {
  const nodes = new Map(meta.resolve.nodes.map((n) => [n.id, n]))
  const pkgs = new Map(meta.packages.map((p) => [p.id, p]))
  const leaks = []
  for (const id of meta.workspace_members) {
    const member = pkgs.get(id)
    if (!member || member.name === 'wisp-desktop') continue
    // 广度优先，记录路径，找到第一个 GUI crate 即停
    const prev = new Map([[id, null]])
    const queue = [id]
    let hit = null
    while (queue.length && !hit) {
      const cur = queue.shift()
      for (const d of nodes.get(cur)?.deps ?? []) {
        if (prev.has(d.pkg)) continue
        prev.set(d.pkg, cur)
        if (GUI_CRATES.has(pkgs.get(d.pkg)?.name)) {
          hit = d.pkg
          break
        }
        queue.push(d.pkg)
      }
    }
    if (hit) {
      const path = []
      for (let p = hit; p; p = prev.get(p)) path.unshift(pkgs.get(p)?.name)
      leaks.push({ member: member.name, path })
    }
  }
  return leaks
}

function checkCrateBoundary(problems) {
  const r = spawnSync('cargo', ['metadata', '--format-version', '1', '--locked'], {
    cwd: ROOT,
    encoding: 'utf8',
    maxBuffer: 256 * 1024 * 1024,
  })
  if (r.error) {
    warn('找不到 cargo，跳过 crate 依赖边界检查（--no-cargo 可显式跳过）')
    return false
  }
  if (r.status !== 0) {
    problems.push({
      file: 'Cargo.lock',
      msg: `cargo metadata 失败：${r.stderr.trim().split('\n').slice(-3).join(' ')}`,
    })
    return false
  }
  for (const l of guiLeaks(JSON.parse(r.stdout))) {
    problems.push({
      file: `crates/${l.member}/Cargo.toml`,
      msg: `${l.member} 间接依赖了 ${l.path.at(-1)}（${l.path.join(' → ')}）：crates/* 必须无头可测，不能引入 tauri`,
    })
  }
  return true
}

// ---------------------------------------------------------------------------

function cargoTest(args) {
  console.log(`$ cargo ${args.join(' ')}`)
  const r = spawnSync('cargo', args, { cwd: ROOT, stdio: 'inherit' })
  if (r.error) throw new Error(`无法运行 cargo：${r.error.message}`)
  return r.status === 0
}

// ---------------------------------------------------------------------------
// 自检

function selfTest() {
  const lines = (text, relSrc = 'panel/X.tsx') => motionViolations(text, relSrc).map((v) => v.line)
  // Tailwind：令牌写法不算，数字与内置缓动算
  assert.deepEqual(
    lines(
      [
        '<div className="transition duration-[var(--dur-fast)] ease-[var(--ease-standard)]" />',
        '<div className="duration-(--dur-base) delay-(--dur-fast)" />',
        '<div className="duration-200 ease-out" />',
        '<div className="duration-[180ms] ease-[cubic-bezier(.2,0,0,1)]" />',
      ].join('\n'),
    ),
    [3, 3, 4, 4],
  )
  // WAAPI：不导入 motion 的文件里同样检查
  assert.deepEqual(
    lines(
      [
        "el.animate([{ opacity: 0 }, { opacity: 1 }], { duration: 180, easing: 'ease-out' })",
        'el.animate(kf, 240)',
        'el.animate(kf, { duration: ms(duration.base), easing: easeCss(ease.standard), fill: "both" })',
        'el.animate(kf, { duration: 0 })',
        'this.animate()',
        'el.animate(kf, {',
        '  endDelay: 20,',
        '  delay: 40, // motion-ok: 与原生窗口动画对齐',
        '})',
      ].join('\n'),
      'overlay/index.ts',
    ),
    [1, 1, 2, 7],
  )
  // 导入 motion 的文件：同一处只报一次
  assert.deepEqual(
    lines(["import { animate } from 'motion'", 'el.animate(kf, { duration: 0.2 })'].join('\n')),
    [2],
  )
  // 注释剥离不能吃掉字符串之后的代码：字符串里的 `//`、嵌套模板、含引号的正则
  assert.deepEqual(
    lines(
      [
        "const s = 'a // b'; el.animate(kf, 200)",
        'const t = `x ${a ? `y // z` : "w"}`; el.animate(kf, 210) // 注释里的 el.animate(kf, 1)',
        'const r = /[\'"`]/g; el.animate(kf, 220)',
        '/* el.animate(kf, 230) */ const ok = 1',
      ].join('\n'),
      'overlay/index.ts',
    ),
    [1, 2, 3],
  )
  // Tailwind 内置动画：没有在 @theme 里用令牌重新定义时算魔法数；任意值动画的数字时长
  const builtin = [
    '<i className="animate-spin" />',
    '<i className="animate-pulse opacity-50" />',
    '<i className="animate-ping" />',
    '<i className="animate-bounce" />',
    '<i className="animate-chunk-in animate-none" />',
    '<i className="animate-[wisp-spin_1s_linear_infinite]" />',
    '<i className="animate-[wisp-spin_var(--dur-slow)_linear_infinite]" />',
  ].join('\n')
  assert.deepEqual(lines(builtin), [1, 2, 3, 4, 6])
  const themed = themedAnimations([
    '@theme {\n  --animate-*: initial;\n  --animate-spin: wisp-spin var(--pulse-period) linear infinite;\n}',
    '/* --animate-pulse: x 2s; */',
  ])
  assert.deepEqual([...themed], ['spin'])
  assert.deepEqual(
    motionViolations(builtin, 'panel/X.tsx', { themed }).map((v) => v.line),
    [2, 3, 4, 6],
  )
  // CSS 与豁免路径；@theme 里 --animate-* 的定义按 CSS 动画规则检查
  assert.deepEqual(lines('.a { transition: opacity 120ms ease-out; }', 'panel/a.css'), [1, 1])
  assert.deepEqual(
    lines(
      [
        '@theme {',
        '  --animate-*: initial;',
        '  --animate-chunk-in: wisp-chunk-in var(--dur-chunk) var(--ease-standard) both;',
        '  --animate-pulse: wisp-pulse 2s ease-in-out infinite;',
        '  --animate-spin: wisp-spin var(--pulse-period) linear infinite;',
        '}',
      ].join('\n'),
      'shared/styles/tailwind.css',
    ),
    [4, 4],
  )
  assert.deepEqual(lines('el.animate(kf, 300)', 'shared/motion/presets.ts'), [])
  ok('check-contract 自检通过（Tailwind 令牌写法与内置动画、WAAPI、Motion、CSS 规则、注释剥离）')
}

function main() {
  const { values } = parseArgs({
    options: {
      rust: { type: 'boolean', default: false },
      'no-cargo': { type: 'boolean', default: false },
      'self-test': { type: 'boolean', default: false },
      help: { type: 'boolean', short: 'h', default: false },
    },
  })
  if (values.help) {
    console.log('用法：node scripts/check-contract.mjs [--rust] [--no-cargo] | --self-test')
    return
  }
  if (values['self-test']) return selfTest()
  const problems = []
  const nCommands = checkCommands(problems)
  const fe = walk(SRC, skipDirs).filter(
    (f) => /\.(?:ts|tsx|mts|css)$/.test(f) && !f.includes(`${join(SRC, 'generated')}`),
  )
  const feCode = fe.filter((f) => !f.endsWith('.css'))
  checkTauriImports(feCode, problems)
  checkMotion(fe, problems)
  const cargoChecked = values['no-cargo'] ? false : checkCrateBoundary(problems)

  for (const p of problems) error(p.msg, p.file, p.line)
  if (problems.length) throw new Error(`契约检查未通过：${problems.length} 处问题`)
  ok(
    `命令：${nCommands} 条，Rust / commands![…] / COMMAND_NAMES 一致，均带 rename_all = "snake_case"`,
  )
  ok(`前端 ${fe.length} 个文件：@tauri-apps/* 只在传输层；没有动效魔法数`)
  if (cargoChecked) ok('crate 依赖边界：只有 wisp-desktop 依赖 tauri')

  if (values.rust) {
    const passed =
      cargoTest(['test', '-p', 'wisp-desktop', '--lib', 'ipc::contract_tests']) &&
      cargoTest(['test', '-p', 'wisp-types'])
    if (!passed) throw new Error('Rust 契约测试未通过')
    ok('Rust 契约测试通过（ipc::contract_tests、wisp-types 黄金样例）')
  }
}

if (isMain(import.meta.url)) await run(main)
