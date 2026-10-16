#!/usr/bin/env node
/**
 * scripts/licenses.mjs：生成第三方许可清单 apps/desktop/src/generated/third-party-licenses.json。
 *
 * 用法：
 *   node scripts/licenses.mjs           重新生成（改动 Cargo.lock / pnpm-lock.yaml 后执行，与 lockfile 一起提交）
 *   node scripts/licenses.mjs --check   只比较，不一致时失败（CI lint）
 *   node scripts/licenses.mjs --self-test  许可表达式与仓库地址规范化的自检（不运行 cargo / pnpm）
 *
 * 来源：
 *   - Rust：`cargo metadata --format-version 1 --locked --filter-platform <目标>`，三个发布目标（aarch64-apple-darwin、
 *     x86_64-pc-windows-msvc、x86_64-unknown-linux-gnu）分别取 wisp-desktop 的**普通依赖**传递闭包后取并集
 *     （不含 dev / build 依赖与 workspace 自身的 crate）；
 *   - npm：`pnpm --filter @wisp/desktop licenses list --prod --json`（只含打进前端包的 dependencies），
 *     仓库地址取各包 package.json 的 repository；
 *   - 手写清单 scripts/licenses-extra.json：不经 lockfile、直接嵌进源码的第三方素材（如服务商标志的路径数据），
 *     每项同样是 `{ name, version, license, repository }`，另可写 `used_in` 注明用在哪里（不进生成的清单）。
 * 输出 `{ name, version, license, repository }[]`，按名称、版本排序，与运行平台无关（可复现）。设置 → 关于
 * 按许可证分组展示，所以 license 统一成规范的 SPDX 表达式（normalizeLicense：`MIT/Apache-2.0`、`Apache-2.0 OR MIT`、
 * `MIT OR Apache-2.0` 都写成 `Apache-2.0 OR MIT`），repository 统一成 https 地址。
 */
import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { parseArgs } from 'node:util'
import { APP_DIR, ROOT, error, info, isMain, ok, rel, run } from './lib/common.mjs'

export const OUT = join(APP_DIR, 'src', 'generated', 'third-party-licenses.json')
export const EXTRA = join(ROOT, 'scripts', 'licenses-extra.json')
export const TARGETS = [
  'aarch64-apple-darwin',
  'x86_64-pc-windows-msvc',
  'x86_64-unknown-linux-gnu',
]
const ROOT_CRATE = 'wisp-desktop'

function exec(cmd, args, cwd) {
  const r = spawnSync(cmd, args, {
    cwd,
    encoding: 'utf8',
    maxBuffer: 512 * 1024 * 1024,
    // Windows 上 pnpm 是 .cmd，需要经 shell 启动
    shell: process.platform === 'win32' && cmd === 'pnpm',
  })
  if (r.error) throw new Error(`无法运行 ${cmd}：${r.error.message}`)
  if (r.status !== 0) {
    throw new Error(
      `${cmd} ${args.join(' ')} 失败（退出码 ${r.status}）：\n${r.stderr.trim().split('\n').slice(-8).join('\n')}`,
    )
  }
  return r.stdout
}

/** 一个目标平台上 wisp-desktop 的普通依赖闭包（不含 workspace 成员）：Map<id, package> */
export function cargoClosure(meta) {
  const pkgs = new Map(meta.packages.map((p) => [p.id, p]))
  const nodes = new Map(meta.resolve.nodes.map((n) => [n.id, n]))
  const members = new Set(meta.workspace_members)
  const root = meta.workspace_members.find((id) => pkgs.get(id)?.name === ROOT_CRATE)
  if (!root) throw new Error(`cargo metadata 里没有 ${ROOT_CRATE}`)
  const out = new Map()
  const stack = [root]
  const seen = new Set(stack)
  while (stack.length) {
    const id = stack.pop()
    for (const d of nodes.get(id)?.deps ?? []) {
      // dep_kinds 中 kind 为 null 的是普通依赖（--filter-platform 已去掉不适用于该目标的边）
      if (!d.dep_kinds.some((k) => k.kind === null)) continue
      if (seen.has(d.pkg)) continue
      seen.add(d.pkg)
      if (!members.has(d.pkg)) out.set(d.pkg, pkgs.get(d.pkg))
      stack.push(d.pkg)
    }
  }
  return out
}

function cargoEntries() {
  const all = new Map()
  for (const target of TARGETS) {
    const meta = JSON.parse(
      exec(
        'cargo',
        ['metadata', '--format-version', '1', '--locked', '--filter-platform', target],
        ROOT,
      ),
    )
    for (const [id, p] of cargoClosure(meta)) all.set(id, p)
  }
  return [...all.values()].map((p) => ({
    name: p.name,
    version: p.version,
    license: p.license
      ? normalizeLicense(p.license)
      : p.license_file
        ? `SEE LICENSE IN ${p.license_file}`
        : 'UNKNOWN',
    repository:
      normalizeRepo(p.repository) ??
      normalizeRepo(p.homepage) ??
      `https://crates.io/crates/${p.name}`,
  }))
}

const SPDX_OPERATORS = new Set(['AND', 'OR', 'WITH'])
const SPDX_ID = /^[A-Za-z0-9][A-Za-z0-9.+:-]*$/
/** 不区分大小写的固定顺序（与运行环境的 locale 无关） */
const spdxOrder = (a, b) => {
  const x = a.replace(/^\(/, '').toLowerCase()
  const y = b.replace(/^\(/, '').toLowerCase()
  return x < y ? -1 : x > y ? 1 : a < b ? -1 : a > b ? 1 : 0
}

/**
 * 把许可证字符串规范成 SPDX 表达式：Cargo 旧写法的 `/` 换成 OR，运算符大写，同一运算符的嵌套展开，OR / AND 的
 * 各项按固定顺序排列并去重，多余的括号去掉（组合项一律加括号，读起来不必记优先级）。WITH 跟在许可证后面整体
 * 当作一项。不是合法表达式的（`SEE LICENSE IN …`、自定义文字）原样返回（只去掉首尾空白）。
 */
export function normalizeLicense(text) {
  const raw = String(text).trim()
  const tokens = raw.replace(/\s*\/\s*/g, ' OR ').match(/[()]|[^\s()]+/g) ?? []
  let pos = 0
  const op = (t) =>
    t !== undefined && SPDX_OPERATORS.has(t.toUpperCase()) ? t.toUpperCase() : null
  const bad = () => {
    throw new Error('not spdx')
  }
  const atom = () => {
    const t = tokens[pos++]
    if (t === '(') {
      const inner = expr()
      if (tokens[pos++] !== ')') bad()
      return inner
    }
    if (t === undefined || t === ')' || op(t) || !SPDX_ID.test(t)) bad()
    if (op(tokens[pos]) === 'WITH') {
      const exception = tokens[pos + 1]
      if (exception === undefined || op(exception) || !SPDX_ID.test(exception)) bad()
      pos += 2
      return `${t} WITH ${exception}`
    }
    return t
  }
  const chain = (kind, next) => {
    const items = [next()]
    while (op(tokens[pos]) === kind) {
      pos++
      items.push(next())
    }
    if (items.length === 1) return items[0]
    // 同一运算符的嵌套展开：(A OR B) OR C → A OR B OR C
    const flat = items.flatMap((x) => (typeof x === 'object' && x.kind === kind ? x.items : [x]))
    return { kind, items: flat }
  }
  const and = () => chain('AND', atom)
  const expr = () => chain('OR', and)
  const show = (node, nested) => {
    if (typeof node === 'string') return node
    const parts = [...new Set(node.items.map((x) => show(x, true)))].toSorted(spdxOrder)
    if (parts.length === 1) return parts[0]
    const s = parts.join(` ${node.kind} `)
    return nested ? `(${s})` : s
  }
  try {
    const ast = expr()
    if (pos !== tokens.length) bad()
    return show(ast, false)
  } catch {
    return raw
  }
}

/** package.json / Cargo.toml 的 repository（或 homepage）→ https 地址 */
export function normalizeRepo(repo) {
  let url = typeof repo === 'string' ? repo : repo?.url
  if (!url) return null
  url = url.trim()
  const short = /^(github|gitlab|bitbucket):(.+)$/.exec(url)
  if (short) return `https://${short[1]}.${short[1] === 'bitbucket' ? 'org' : 'com'}/${short[2]}`
  if (/^[\w.-]+\/[\w.-]+$/.test(url)) return `https://github.com/${url}`
  url = url
    .replace(/^git\+/, '')
    .replace(/^(?:git|http):\/\//, 'https://')
    .replace(/^ssh:\/\/git@/, 'https://')
    .replace(/^git@([^:]+):/, 'https://$1/')
    .replace(/#.*$/, '')
    .replace(/\.git$/, '')
  return url
}

function npmEntries() {
  const json = JSON.parse(
    exec('pnpm', ['--filter', '@wisp/desktop', 'licenses', 'list', '--prod', '--json'], ROOT) ||
      '{}',
  )
  const out = []
  for (const [license, pkgs] of Object.entries(json)) {
    for (const p of pkgs) {
      p.versions.forEach((version, i) => {
        let repository = null
        const dir = p.paths?.[i] ?? p.paths?.[0]
        if (dir) {
          try {
            repository = normalizeRepo(
              JSON.parse(readFileSync(join(dir, 'package.json'), 'utf8')).repository,
            )
          } catch {
            repository = null
          }
        }
        out.push({
          name: p.name,
          version,
          license: normalizeLicense(p.license ?? license),
          repository:
            repository ?? normalizeRepo(p.homepage) ?? `https://www.npmjs.com/package/${p.name}`,
        })
      })
    }
  }
  return out
}

/** 手写清单（scripts/licenses-extra.json）：校验必填字段，许可证与仓库地址同样规范化 */
export function extraEntries(text) {
  const list = JSON.parse(text)
  if (!Array.isArray(list)) throw new Error(`${rel(EXTRA)} 应当是数组`)
  return list.map((e, i) => {
    for (const key of ['name', 'version', 'license', 'repository']) {
      if (typeof e?.[key] !== 'string' || !e[key].trim())
        throw new Error(`${rel(EXTRA)} 第 ${i + 1} 项缺少 ${key}`)
    }
    return {
      name: e.name,
      version: e.version,
      license: normalizeLicense(e.license),
      repository: normalizeRepo(e.repository),
    }
  })
}

const cmp = (a, b) => (a < b ? -1 : a > b ? 1 : 0)

/** 去重并排序，返回最终的 JSON 文本 */
export function render(entries) {
  const uniq = new Map()
  for (const e of entries) uniq.set(`${e.name}@${e.version}@${e.license}`, e)
  const sorted = [...uniq.values()].toSorted(
    (a, b) => cmp(a.name, b.name) || cmp(a.version, b.version) || cmp(a.license, b.license),
  )
  return `${JSON.stringify(
    sorted.map(({ name, version, license, repository }) => ({
      name,
      version,
      license,
      repository,
    })),
    null,
    2,
  )}\n`
}

function selfTest() {
  const cases = [
    ['MIT OR Apache-2.0', 'Apache-2.0 OR MIT'],
    ['Apache-2.0 OR MIT', 'Apache-2.0 OR MIT'],
    ['MIT/Apache-2.0', 'Apache-2.0 OR MIT'],
    ['Apache-2.0 / MIT', 'Apache-2.0 OR MIT'],
    ['(MIT or Apache-2.0)', 'Apache-2.0 OR MIT'],
    ['Unlicense/MIT', 'MIT OR Unlicense'],
    ['Zlib OR Apache-2.0 OR MIT', 'Apache-2.0 OR MIT OR Zlib'],
    ['MIT OR (Apache-2.0 OR Zlib)', 'Apache-2.0 OR MIT OR Zlib'],
    ['MIT OR Apache-2.0 OR MIT', 'Apache-2.0 OR MIT'],
    [
      'Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT',
      'Apache-2.0 OR Apache-2.0 WITH LLVM-exception OR MIT',
    ],
    ['(MIT OR Apache-2.0) AND Unicode-3.0', '(Apache-2.0 OR MIT) AND Unicode-3.0'],
    ['Unicode-3.0 AND (Apache-2.0 OR MIT)', '(Apache-2.0 OR MIT) AND Unicode-3.0'],
    ['MIT AND BSD-3-Clause', 'BSD-3-Clause AND MIT'],
    ['MIT OR Apache-2.0 AND ISC', '(Apache-2.0 AND ISC) OR MIT'],
    ['0BSD OR MIT OR Apache-2.0', '0BSD OR Apache-2.0 OR MIT'],
    ['GPL-2.0+', 'GPL-2.0+'],
    // 不是 SPDX 表达式：原样保留
    ['SEE LICENSE IN LICENSE.md', 'SEE LICENSE IN LICENSE.md'],
    ['UNKNOWN', 'UNKNOWN'],
    ['MIT OR', 'MIT OR'],
    ['(MIT', '(MIT'],
    ['Custom: https://example.com/license', 'Custom: https://example.com/license'],
  ]
  for (const [input, want] of cases) assert.equal(normalizeLicense(input), want, input)
  // 同一组合的各种写法规范化后完全相同（关于页按它分组）
  assert.equal(new Set(cases.slice(0, 5).map(([x]) => normalizeLicense(x))).size, 1)
  const repos = [
    ['http://github.com/tailhook/quick-error', 'https://github.com/tailhook/quick-error'],
    ['git+https://github.com/a/b.git', 'https://github.com/a/b'],
    ['git://github.com/a/b.git#main', 'https://github.com/a/b'],
    ['git@github.com:a/b.git', 'https://github.com/a/b'],
    ['github:a/b', 'https://github.com/a/b'],
    ['a/b', 'https://github.com/a/b'],
    [{ url: 'ssh://git@github.com/a/b.git' }, 'https://github.com/a/b'],
    [undefined, null],
  ]
  for (const [input, want] of repos) assert.equal(normalizeRepo(input), want, String(input))
  assert.deepEqual(
    extraEntries(
      '[{"name":"x","version":"1.0.0","license":"MIT","repository":"git+https://github.com/a/b.git","used_in":"src"}]',
    ),
    [{ name: 'x', version: '1.0.0', license: 'MIT', repository: 'https://github.com/a/b' }],
  )
  assert.throws(
    () => extraEntries('[{"name":"x","version":"1.0.0","license":"MIT"}]'),
    /repository/,
  )
  assert.throws(() => extraEntries('{}'), /数组/)
  // 手写清单本身也要能通过校验
  extraEntries(readFileSync(EXTRA, 'utf8'))
  ok(`licenses 自检通过（${cases.length} 个许可表达式、${repos.length} 个仓库地址、手写清单）`)
}

function main() {
  const { values } = parseArgs({
    options: {
      check: { type: 'boolean', default: false },
      'self-test': { type: 'boolean', default: false },
      help: { type: 'boolean', short: 'h', default: false },
    },
  })
  if (values.help) {
    console.log('用法：node scripts/licenses.mjs [--check | --self-test]')
    return
  }
  if (values['self-test']) return selfTest()
  const rust = cargoEntries()
  const npm = npmEntries()
  const extra = extraEntries(readFileSync(EXTRA, 'utf8'))
  const text = render([...rust, ...npm, ...extra])
  const count = JSON.parse(text).length
  const parts = `Rust ${rust.length}、npm ${npm.length}、手写 ${extra.length}`
  let current = ''
  try {
    current = readFileSync(OUT, 'utf8')
  } catch {
    current = ''
  }
  if (values.check) {
    if (current === text) {
      ok(`第三方许可清单与 lockfile 同步（${count} 项：${parts}）`)
      return
    }
    const key = (e) => `${e.name}@${e.version}（${e.license}）`
    let old = []
    try {
      old = JSON.parse(current || '[]')
    } catch {
      old = []
    }
    const before = new Set(old.map(key))
    const after = new Set(JSON.parse(text).map(key))
    const added = [...after].filter((k) => !before.has(k))
    const removed = [...before].filter((k) => !after.has(k))
    if (added.length)
      info(
        `新增：${added.slice(0, 30).join('、')}${added.length > 30 ? ` 等 ${added.length} 项` : ''}`,
      )
    if (removed.length)
      info(
        `移除：${removed.slice(0, 30).join('、')}${removed.length > 30 ? ` 等 ${removed.length} 项` : ''}`,
      )
    error(
      `${rel(OUT)} 与 lockfile（或 ${rel(EXTRA)}）不同步：执行 pnpm licenses 重新生成并提交`,
      rel(OUT),
    )
    throw new Error('第三方许可清单需要重新生成')
  }
  if (current === text) {
    ok(`${rel(OUT)} 已是最新（${count} 项）`)
    return
  }
  writeFileSync(OUT, text)
  ok(`已生成 ${rel(OUT)}（${count} 项：${parts}）`)
}

if (isMain(import.meta.url)) await run(main)
