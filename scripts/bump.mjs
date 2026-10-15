#!/usr/bin/env node
/**
 * scripts/bump.mjs：同步版本号。
 *
 * 版本号出现在四处，必须一致：
 *   - package.json（根）与 apps/desktop/package.json 的 `version`
 *   - Cargo.toml 的 `[workspace.package] version`（各 crate 用 `version.workspace = true` 继承）
 *   - apps/desktop/src-tauri/tauri.conf.json 的 `version`（安装包文件名与 latest.json 的版本来源）
 * Cargo.lock 记录了工作区各 crate 的版本，一并改掉，`cargo … --locked` 才不会因为版本号变化而失败。工作区成员按 Cargo.toml 的
 * `[workspace] members` 认定——`[patch.crates-io]` 引入的 vendor/ 源码（例如 vendor/wry）在 Cargo.lock 里同样没有 `source`，
 * 但不是工作区成员，版本号不归本仓库管，既不检查也不改写。
 *
 * 用法：
 *   node scripts/bump.mjs 0.2.0              设为指定版本（SemVer，可带预发布后缀，如 0.2.0-beta.1；前缀 v 可省略）
 *   node scripts/bump.mjs patch|minor|major  在当前版本上递增（预发布版本的 patch 递增即对应的正式版）
 *   node scripts/bump.mjs --check            只检查各处是否一致（CI lint 与 release 用）
 *   node scripts/bump.mjs --print            打印当前版本
 *   node scripts/bump.mjs --self-test        核对工作区成员的认定，并在临时目录里演练一次改版本号与改回（CI 用）
 *   加 --dry-run 只显示将要做的修改。
 *
 * 只做最小的文本替换，保留各文件原有的格式与注释（改过去再改回来逐字节不变，自检覆盖）。
 * 发版流程见 docs/development.md「发布流程」：改版本号之后、提交之前，先用 git-cliff 把新版本的段落
 * 插进 CHANGELOG.md 并人工审定（release 工作流的 Release 说明取自它）。
 */
import assert from 'node:assert/strict'
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join, relative } from 'node:path'
import { parseArgs } from 'node:util'
import { ROOT, error, isMain, ok, run } from './lib/common.mjs'

/** SemVer 2.0（不含构建元数据：tauri 的 version 不接受 `+`） */
export const SEMVER =
  /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*))?$/

/** 某个仓库根目录下需要同步版本号的文件（自检在临时目录里演练时传入别的根目录） */
function files(root) {
  return {
    json: [
      join(root, 'package.json'),
      join(root, 'apps', 'desktop', 'package.json'),
      join(root, 'apps', 'desktop', 'src-tauri', 'tauri.conf.json'),
    ],
    toml: join(root, 'Cargo.toml'),
    lock: join(root, 'Cargo.lock'),
    rel: (path) => relative(root, path).replaceAll('\\', '/'),
  }
}

/** `[workspace.package]` 段内的 version 行（段内不跨越下一个 `[` 开头的段） */
const WS_VERSION = /^(\[workspace\.package\]\n(?:(?!\[)[^\n]*\n)*?version\s*=\s*")([^"]*)(")/m

/**
 * 工作区成员的 crate 名：根 Cargo.toml `[workspace] members` 列出的目录（支持 `dir/*`）下各 Cargo.toml 的 `[package] name`
 */
export function workspaceCrates(root = ROOT) {
  const toml = readFileSync(join(root, 'Cargo.toml'), 'utf8')
  const section = /^\[workspace\]\n([\s\S]*?)(?=^\[(?!\s*")|(?![\s\S]))/m.exec(toml)?.[1] ?? ''
  const list = /^members\s*=\s*\[([^\]]*)\]/m.exec(section)?.[1]
  if (list === undefined) throw new Error('Cargo.toml 中没有 [workspace] members')
  const dirs = [...list.matchAll(/"([^"]+)"/g)].flatMap(([, pattern]) => {
    if (!pattern.endsWith('/*')) return [pattern]
    const parent = pattern.slice(0, -2)
    return readdirSync(join(root, parent), { withFileTypes: true })
      .filter((d) => d.isDirectory())
      .map((d) => `${parent}/${d.name}`)
  })
  const names = new Set()
  for (const dir of dirs) {
    const manifest = join(root, dir, 'Cargo.toml')
    if (!existsSync(manifest)) continue
    const pkg = /^\[package\]\n([\s\S]*?)(?=^\[|(?![\s\S]))/m.exec(
      readFileSync(manifest, 'utf8'),
    )?.[1]
    const name = pkg && /^name\s*=\s*"([^"]+)"/m.exec(pkg)?.[1]
    if (name) names.add(name)
  }
  if (names.size === 0) throw new Error('Cargo.toml 的 [workspace] members 下没有找到任何 crate')
  return names
}

/** Cargo.lock 的一个 [[package]] 块是不是工作区成员（没有 `source`、且名字在工作区成员里） */
function isMemberBlock(block, members) {
  const name = /^name = "([^"]+)"/m.exec(block)?.[1]
  return Boolean(name) && members.has(name) && !/^source = /m.test(block)
}

/** Cargo.lock 中的工作区 crate（`[patch]` 引入的 vendor/ 路径依赖同样没有 `source`，按成员名排除） */
export function lockMembers(lock, members = workspaceCrates()) {
  const out = []
  for (const block of lock.split(/^(?=\[\[package\]\]$)/m)) {
    if (!block.startsWith('[[package]]') || !isMemberBlock(block, members)) continue
    out.push({
      name: /^name = "([^"]+)"/m.exec(block)?.[1] ?? '',
      version: /^version = "([^"]+)"/m.exec(block)?.[1] ?? '',
    })
  }
  return out
}

/** 读取各处的版本号 */
export function readVersions(root = ROOT) {
  const f = files(root)
  const found = []
  for (const file of f.json) {
    const v = JSON.parse(readFileSync(file, 'utf8')).version
    found.push({ file: f.rel(file), version: typeof v === 'string' ? v : String(v) })
  }
  const toml = readFileSync(f.toml, 'utf8')
  const wsVersion = WS_VERSION.exec(toml)?.[2]
  if (wsVersion === undefined) throw new Error('Cargo.toml 中没有 [workspace.package] version')
  found.push({ file: 'Cargo.toml [workspace.package]', version: wsVersion })
  for (const m of lockMembers(readFileSync(f.lock, 'utf8'), workspaceCrates(root))) {
    found.push({ file: `Cargo.lock (${m.name})`, version: m.version })
  }
  return found
}

/** 各处一致时返回该版本，否则返回 null */
export function currentVersion(root = ROOT) {
  const all = readVersions(root)
  return new Set(all.map((v) => v.version)).size === 1 ? (all[0]?.version ?? null) : null
}

/** 在 current 上做 patch / minor / major 递增 */
export function nextVersion(current, kind) {
  const m = SEMVER.exec(current)
  if (!m) throw new Error(`当前版本 ${current} 不是合法的 SemVer`)
  let [major, minor, patch] = [Number(m[1]), Number(m[2]), Number(m[3])]
  const pre = m[4] !== undefined
  if (kind === 'major') [major, minor, patch] = [major + 1, 0, 0]
  else if (kind === 'minor') [minor, patch] = [minor + 1, 0]
  else if (!pre) patch += 1
  return `${major}.${minor}.${patch}`
}

function replaceJsonVersion(text, to, name) {
  const m = /("version"\s*:\s*")([^"]*)(")/.exec(text)
  if (!m) throw new Error(`${name}：没有 "version" 字段`)
  const next = text.slice(0, m.index) + m[1] + to + m[3] + text.slice(m.index + m[0].length)
  // 第一个 "version" 必须就是顶层字段，否则宁可报错也不改错位置
  if (JSON.parse(next).version !== to) throw new Error(`${name}：第一个 "version" 不是顶层字段`)
  return next
}

/** 把 `root` 下各处的版本号改为 `to`；`log` 默认打印到控制台（自检传入空实现） */
export function bump(to, dryRun, root = ROOT, log = { info: console.log, ok }) {
  const f = files(root)
  const changes = []
  for (const file of f.json) {
    const text = readFileSync(file, 'utf8')
    const from = JSON.parse(text).version
    if (from !== to) changes.push({ file, from, text: replaceJsonVersion(text, to, f.rel(file)) })
  }
  const toml = readFileSync(f.toml, 'utf8')
  const tomlFrom = WS_VERSION.exec(toml)?.[2]
  if (tomlFrom === undefined) throw new Error('Cargo.toml 中没有 [workspace.package] version')
  if (tomlFrom !== to) {
    changes.push({ file: f.toml, from: tomlFrom, text: toml.replace(WS_VERSION, `$1${to}$3`) })
  }
  const lock = readFileSync(f.lock, 'utf8')
  const crates = workspaceCrates(root)
  const members = lockMembers(lock, crates)
  if (members.some((m) => m.version !== to)) {
    const next = lock
      .split(/^(?=\[\[package\]\]$)/m)
      .map((block) =>
        block.startsWith('[[package]]') && isMemberBlock(block, crates)
          ? block.replace(/^(version = ")[^"]*(")/m, `$1${to}$2`)
          : block,
      )
      .join('')
    const from = [...new Set(members.map((m) => m.version))].join(' / ')
    changes.push({
      file: f.lock,
      from: `${from}（${members.length} 个工作区 crate）`,
      text: next,
    })
  }

  if (changes.length === 0) {
    log.ok(`版本号已经是 ${to}`)
    return
  }
  for (const c of changes) {
    log.info(`${dryRun ? '将修改' : '修改'} ${f.rel(c.file)}：${c.from} → ${to}`)
    if (!dryRun) writeFileSync(c.file, c.text)
  }
  if (dryRun) return
  if (currentVersion(root) !== to) throw new Error('修改后各处版本仍不一致，请检查上面列出的文件')
  log.ok(
    `版本号已同步为 ${to}。下一步：npx --yes git-cliff@2.14.2 --unreleased --tag v${to} --prepend CHANGELOG.md，` +
      `审定新段落后 git commit -am "chore(release): v${to}"，再打 tag v${to}（见 docs/development.md「发布流程」）`,
  )
}

function check(print) {
  const all = readVersions()
  const set = new Set(all.map((v) => v.version))
  if (set.size !== 1) {
    for (const v of all) error(`${v.file}: ${v.version}`)
    throw new Error('版本号不一致，请执行 node scripts/bump.mjs <版本> 同步')
  }
  const [v] = set
  if (!SEMVER.test(v)) throw new Error(`版本号 ${v} 不是合法的 SemVer`)
  if (print) console.log(v)
  else ok(`版本号一致：${v}（${all.length} 处）`)
}

function selfTest() {
  // [patch] 引入的 vendor 源码（没有 source）不是工作区成员
  const lock = [
    '# This file is automatically @generated by Cargo.',
    'version = 4',
    '',
    '[[package]]',
    'name = "serde"',
    'version = "1.0.0"',
    'source = "registry+https://github.com/rust-lang/crates.io-index"',
    '',
    '[[package]]',
    'name = "wisp-types"',
    'version = "0.1.0"',
    '',
    '[[package]]',
    'name = "wry"',
    'version = "0.57.0"',
    '',
  ].join('\n')
  assert.deepEqual(lockMembers(lock, new Set(['wisp-types', 'wisp-desktop'])), [
    { name: 'wisp-types', version: '0.1.0' },
  ])
  // 本仓库：两类成员（crates/* 与 apps/desktop/src-tauri）都认出来，vendor/ 不算
  const crates = workspaceCrates()
  for (const name of ['wisp-types', 'wisp-desktop'])
    assert.ok(crates.has(name), `工作区成员缺 ${name}`)
  assert.ok(!crates.has('wry'), 'vendor/wry 不是工作区成员')
  const inLock = lockMembers(readFileSync(files(ROOT).lock, 'utf8'), crates).map((m) => m.name)
  assert.deepEqual(
    inLock.toSorted(),
    [...crates].toSorted(),
    'Cargo.lock 里的工作区成员与 Cargo.toml 一致',
  )
  roundTrip()
  ok(
    `bump 自检通过（工作区成员 ${crates.size} 个，vendor/ 的 [patch] 源码不计入；改版本号再改回逐字节不变）`,
  )
}

/**
 * 在临时目录里搭一个最小的仓库（两个工作区成员 + 一个 [patch] 引入的 vendor crate、带注释与非标准缩进的 JSON / TOML），
 * 改到预发布版本、递增到正式版、再改回原版本：每一步各处一致，最后与原文件逐字节相同，vendor 的版本与依赖的
 * version 字段不受影响
 */
function roundTrip() {
  const root = mkdtempSync(join(tmpdir(), 'wisp-bump-'))
  const write = (path, text) => {
    mkdirSync(join(root, path, '..'), { recursive: true })
    writeFileSync(join(root, path), text)
  }
  try {
    const original = {
      'Cargo.toml': [
        '[workspace]',
        'members = ["crates/*", "apps/desktop/src-tauri"]',
        'exclude = ["vendor"]',
        '',
        '[workspace.package]',
        'edition = "2024"',
        'version = "0.1.0" # 由 scripts/bump.mjs 维护',
        '',
        '[workspace.dependencies]',
        'serde = { version = "1.0.0" }',
        '',
        '[patch.crates-io]',
        'wry = { path = "vendor/wry" }',
        '',
      ].join('\n'),
      'crates/wisp-a/Cargo.toml': '[package]\nname = "wisp-a"\nversion.workspace = true\n',
      'apps/desktop/src-tauri/Cargo.toml':
        '[package]\nname = "wisp-desktop"\nversion.workspace = true\n',
      'Cargo.lock': [
        'version = 4',
        '',
        '[[package]]',
        'name = "serde"',
        'version = "1.0.0"',
        'source = "registry+https://github.com/rust-lang/crates.io-index"',
        '',
        '[[package]]',
        'name = "wisp-a"',
        'version = "0.1.0"',
        'dependencies = [',
        ' "serde",',
        ']',
        '',
        '[[package]]',
        'name = "wisp-desktop"',
        'version = "0.1.0"',
        '',
        '[[package]]',
        'name = "wry"',
        'version = "0.57.0"',
        '',
      ].join('\n'),
      'package.json': '{\n  "name": "wisp",\n  "version": "0.1.0",\n  "private": true\n}\n',
      'apps/desktop/package.json':
        '{\n    "name": "desktop",\n    "version": "0.1.0",\n    "dependencies": { "x": "1.0.0" }\n}\n',
      'apps/desktop/src-tauri/tauri.conf.json':
        '{\n  "productName": "Wisp",\n  "version": "0.1.0",\n  "plugins": { "updater": { "version": "keep" } }\n}\n',
    }
    for (const [path, text] of Object.entries(original)) write(path, text)
    const quiet = { info() {}, ok() {} }
    assert.equal(currentVersion(root), '0.1.0')
    bump('0.2.0-beta.1', false, root, quiet)
    assert.equal(currentVersion(root), '0.2.0-beta.1')
    const lock = readFileSync(join(root, 'Cargo.lock'), 'utf8')
    assert.ok(lock.includes('name = "wry"\nversion = "0.57.0"'), 'vendor crate 的版本不改')
    assert.ok(lock.includes('name = "serde"\nversion = "1.0.0"'), '外部依赖的版本不改')
    const conf = JSON.parse(
      readFileSync(join(root, 'apps/desktop/src-tauri/tauri.conf.json'), 'utf8'),
    )
    assert.equal(conf.plugins.updater.version, 'keep', '只改顶层 version')
    bump(nextVersion('0.2.0-beta.1', 'patch'), false, root, quiet)
    assert.equal(currentVersion(root), '0.2.0')
    // dry-run 不写文件
    bump('9.9.9', true, root, quiet)
    assert.equal(currentVersion(root), '0.2.0')
    bump('0.1.0', false, root, quiet)
    for (const [path, text] of Object.entries(original)) {
      assert.equal(readFileSync(join(root, path), 'utf8'), text, `${path} 改回后应逐字节不变`)
    }
  } finally {
    rmSync(root, { recursive: true, force: true })
  }
}

function main() {
  const { values, positionals } = parseArgs({
    allowPositionals: true,
    options: {
      'self-test': { type: 'boolean', default: false },
      check: { type: 'boolean', default: false },
      print: { type: 'boolean', default: false },
      'dry-run': { type: 'boolean', default: false },
      help: { type: 'boolean', short: 'h', default: false },
    },
  })
  if (values.help) {
    console.log(
      '用法：node scripts/bump.mjs <版本|patch|minor|major> [--dry-run]  |  --check  |  --print  |  --self-test',
    )
    return
  }
  if (values['self-test']) return selfTest()
  if (values.check || values.print) return check(values.print)
  const arg = positionals[0]
  if (!arg) throw new Error('缺少版本号；用法：node scripts/bump.mjs <版本|patch|minor|major>')
  let to = arg.replace(/^v/, '')
  if (['patch', 'minor', 'major'].includes(to)) {
    const cur = currentVersion()
    if (!cur) throw new Error('各处版本当前不一致，请指定明确的版本号')
    to = nextVersion(cur, to)
  }
  if (!SEMVER.test(to)) throw new Error(`${arg} 不是合法的 SemVer（例：0.2.0、0.2.0-beta.1）`)
  bump(to, values['dry-run'])
}

if (isMain(import.meta.url)) await run(main)
