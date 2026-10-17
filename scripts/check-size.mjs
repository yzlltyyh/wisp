#!/usr/bin/env node
/**
 * scripts/check-size.mjs：前端资源体积、入口依赖图与安装包体积检查。
 *
 * 用法：
 *   node scripts/check-size.mjs                  生产构建前端（输出到 target/size-check/dist，不影响 apps/desktop/dist），
 *                                                然后检查资源体积与入口依赖图
 *   node scripts/check-size.mjs --dist <目录>    只检查已有构建产物的体积（不检查依赖图）
 *   node scripts/check-size.mjs --bundles <目录> 递归查找安装包并检查体积（release 用，例如 --bundles target）
 *   node scripts/check-size.mjs --self-test      用构造的 chunk 图检查入口依赖图规则（CI 用）
 *
 * 预算（KB = 1000 B、MB = 1000² B，与 Vite 构建输出的单位一致；均为压缩前）：
 *   - 前端资源（dist 下全部文件）≤ 1.6 MB；
 *   - Overlay ≤ 15 KB。统计口径：overlay.html 的入口 JS **加上它全部的静态 import**
 *     （`<link rel="modulepreload">` 列出的共享 chunk）再加 overlay 的 CSS；动态 import 不计；
 *   - 安装包：dmg ≤ 12 MB、NSIS（*-setup.exe）≤ 10 MB、deb / rpm ≤ 15 MB、AppImage ≤ 90 MB。
 * 入口依赖图（读取 Rolldown 的 chunk 模块清单）：
 *   - panel 入口静态可达的模块里没有 src/settings、src/onboarding、src/overlay（连 lazy() 出去的 chunk 里也不能有）；
 *     五个视图（翻译 / OCR / 剪贴板 / 输入 / toast）都必须静态导入（首次唤出任何视图都没有懒加载请求）。
 *     要为压低热状态内存而放宽这条规则，前提是两件事一起到位：懒加载的实现与内存实测（能省多少），以及「懒加载视图的
 *     chunk 在首次唤出前已预取」的门禁（依赖图里核对预取清单，或冒烟每次启动后各唤出一次并按视图判 ≤ 100 ms）；
 *   - settings 入口静态可达的模块里没有 src/settings、src/onboarding（设置与引导互相 lazy()，各自只加载自己的代码）；
 *   - pinned（翻译 / OCR 的固定窗口）入口里没有 src/views/clipboard 下的任何模块（剪贴板视图只在面板与悬浮剪贴板的
 *     入口里；OCR 的芯片条与剪贴板共用的部件在 src/shared/smart）；clipfloat（悬浮剪贴板）入口静态导入剪贴板视图，
 *     不带翻译 / 输入视图与设置、引导、Overlay；
 *   - overlay 入口静态可达的模块里没有 React / Motion 运行时。
 * 另外打印 panel 入口首屏（静态可达）的 JS / CSS 体积与各视图的渲染后字节数（只报告，不设预算），供懒加载取舍参考。
 */
import assert from 'node:assert/strict'
import { gzipSync } from 'node:zlib'
import { readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { basename, join, relative, resolve, sep } from 'node:path'
import { pathToFileURL } from 'node:url'
import { parseArgs } from 'node:util'
import {
  APP_DIR,
  ROOT,
  appendSummary,
  error,
  isMain,
  ok,
  rel,
  run,
  sizeOf,
  walk,
} from './lib/common.mjs'

const KB = 1000
const MB = 1000 * 1000

export const BUDGET = {
  frontend: 1.6 * MB,
  overlay: 15 * KB,
  bundles: [
    { kind: 'dmg', test: (f) => f.endsWith('.dmg'), max: 12 * MB },
    { kind: 'NSIS', test: (f) => /-setup\.exe$/i.test(f), max: 10 * MB },
    { kind: 'deb', test: (f) => f.endsWith('.deb'), max: 15 * MB },
    { kind: 'rpm', test: (f) => f.endsWith('.rpm'), max: 15 * MB },
    { kind: 'AppImage', test: (f) => f.endsWith('.AppImage'), max: 90 * MB },
    // 不在发布矩阵里（Windows 只出 NSIS），出现时按 NSIS 的预算检查
    { kind: 'MSI', test: (f) => f.endsWith('.msi'), max: 10 * MB },
  ],
}

/**
 * 依赖图规则：入口名 → 模块前缀（相对 apps/desktop 的路径前缀，或 `pkg:<包名>`）。
 * - `forbid`：静态可达（首屏）的模块里不能出现；
 * - `forbidLazy`：静态 + 动态可达（含 lazy() 出去的 chunk）的模块里都不能出现；
 * - `require`：必须静态可达（首屏就要用，不能有懒加载请求）。
 */
export const GRAPH_RULES = {
  panel: {
    forbid: ['src/settings/', 'src/onboarding/', 'src/overlay/'],
    forbidLazy: ['src/settings/', 'src/onboarding/', 'src/overlay/'],
    require: [
      'src/views/translate/',
      'src/views/ocr/',
      'src/views/clipboard/',
      'src/views/input/',
      'src/views/toast/',
    ],
  },
  settings: { forbid: ['src/settings/', 'src/onboarding/'], require: [] },
  pinned: {
    forbid: [
      'src/settings/',
      'src/onboarding/',
      'src/overlay/',
      'src/views/clipboard/',
      'src/floating/',
    ],
    require: [],
  },
  clipfloat: {
    forbid: [
      'src/settings/',
      'src/onboarding/',
      'src/overlay/',
      'src/views/translate/',
      'src/views/input/',
      'src/pinned/',
    ],
    forbidLazy: ['src/settings/', 'src/onboarding/', 'src/overlay/'],
    require: ['src/views/clipboard/', 'src/floating/'],
  },
  overlay: {
    forbid: [
      'pkg:react',
      'pkg:react-dom',
      'pkg:scheduler',
      'pkg:motion',
      'pkg:framer-motion',
      'pkg:motion-dom',
      'pkg:motion-utils',
      'src/settings/',
      'src/onboarding/',
      'src/panel/',
      'src/views/',
    ],
    require: [],
  },
}

const fmt = (n) => (n >= MB ? `${(n / MB).toFixed(2)} MB` : `${(n / KB).toFixed(2)} KB`)

// ---------------------------------------------------------------------------
// 前端构建（带 chunk 模块清单）

/** 以 apps/desktop/vite.config.ts 做一次生产构建，返回 `{ outDir, chunks }`（chunks 为 Rolldown 的 chunk 信息） */
async function buildWithGraph(outDir) {
  const require = createRequire(join(APP_DIR, 'package.json'))
  const vite = await import(pathToFileURL(require.resolve('vite')).href)
  const chunks = []
  const recorder = {
    name: 'wisp:size-graph',
    apply: 'build',
    generateBundle(_options, bundle) {
      for (const item of Object.values(bundle)) {
        if (item.type !== 'chunk') continue
        chunks.push({
          fileName: item.fileName,
          isEntry: item.isEntry,
          facadeModuleId: item.facadeModuleId ?? null,
          imports: [...item.imports],
          dynamicImports: [...item.dynamicImports],
          moduleIds: [...(item.moduleIds ?? Object.keys(item.modules ?? {}))],
          // 各模块压缩后的渲染字节数（只用于报告各视图的体积）
          moduleSizes: Object.fromEntries(
            Object.entries(item.modules ?? {}).map(([id, m]) => [id, m.renderedLength ?? 0]),
          ),
          size: Buffer.byteLength(item.code ?? ''),
          css: [...(item.viteMetadata?.importedCss ?? [])],
        })
      }
    },
  }
  const cwd = process.cwd()
  // vite.config.ts 用 process.cwd() 读取 .env，与 `pnpm -C apps/desktop build` 保持一致
  process.chdir(APP_DIR)
  try {
    await vite.build({
      configFile: join(APP_DIR, 'vite.config.ts'),
      root: APP_DIR,
      mode: 'production',
      logLevel: 'warn',
      plugins: [recorder],
      build: { outDir, emptyOutDir: true },
    })
  } finally {
    process.chdir(cwd)
  }
  if (chunks.length === 0) throw new Error('构建没有产出任何 chunk')
  return chunks
}

/** 模块 id → 规则用的规范名：apps/desktop 下的源码为相对路径，node_modules 里的为 `pkg:<包名>` */
export function moduleKey(id) {
  const clean = id.replace(/\?.*$/, '').replaceAll('\\', '/')
  const nm = clean.lastIndexOf('/node_modules/')
  if (nm >= 0) {
    const rest = clean.slice(nm + '/node_modules/'.length).split('/')
    return `pkg:${rest[0].startsWith('@') ? `${rest[0]}/${rest[1]}` : rest[0]}`
  }
  // 不直接比较 APP_DIR：仓库路径里有符号链接时，Rolldown 给出的是真实路径
  const at = clean.lastIndexOf('/apps/desktop/')
  return at >= 0 ? clean.slice(at + '/apps/desktop/'.length) : clean
}

/** 从入口 chunk 出发沿静态 import（`dynamic` 为真时连同动态 import）收集 chunk 与模块 */
export function staticClosure(chunks, entryFile, dynamic = false) {
  const byFile = new Map(chunks.map((c) => [c.fileName, c]))
  const seen = new Set()
  const stack = [entryFile]
  while (stack.length) {
    const f = stack.pop()
    if (seen.has(f)) continue
    seen.add(f)
    const c = byFile.get(f)
    for (const i of c?.imports ?? []) stack.push(i)
    if (dynamic) for (const i of c?.dynamicImports ?? []) stack.push(i)
  }
  const modules = new Set()
  for (const f of seen) for (const m of byFile.get(f)?.moduleIds ?? []) modules.add(moduleKey(m))
  return { chunks: seen, modules }
}

/** 入口名 → 入口 chunk */
function entryChunk(chunks, entry) {
  return chunks.find(
    (c) => c.isEntry && c.facadeModuleId?.replaceAll('\\', '/').endsWith(`/${entry}.html`),
  )
}

const matches = (m, prefix) => (prefix.startsWith('pkg:') ? m === prefix : m.startsWith(prefix))

/** 按 GRAPH_RULES 检查依赖图，返回错误列表 */
export function checkGraph(chunks, rules = GRAPH_RULES) {
  const problems = []
  const shown = (hit) => hit.slice(0, 5).join('、') + (hit.length > 5 ? ` 等 ${hit.length} 个` : '')
  for (const [entry, rule] of Object.entries(rules)) {
    const chunk = entryChunk(chunks, entry)
    if (!chunk) {
      problems.push(`找不到 ${entry}.html 的入口 chunk`)
      continue
    }
    const list = [...staticClosure(chunks, chunk.fileName).modules]
    const all = [...staticClosure(chunks, chunk.fileName, true).modules]
    for (const f of rule.forbid) {
      const hit = list.filter((m) => matches(m, f))
      if (hit.length) {
        problems.push(
          `${entry} 入口静态依赖了 ${f.replace(/^pkg:/, '')}：${shown(hit)}（应改为 lazy() / 动态 import）`,
        )
      }
    }
    for (const f of rule.forbidLazy ?? []) {
      // 静态依赖已经由 forbid 报过，这里只报 lazy() 出去才到达的
      const hit = all.filter((m) => matches(m, f) && !list.includes(m))
      if (hit.length) {
        problems.push(
          `${entry} 入口经 lazy() / 动态 import 依赖了 ${f.replace(/^pkg:/, '')}：${shown(hit)}（不属于这个窗口的代码）`,
        )
      }
    }
    for (const r of rule.require) {
      if (!list.some((m) => m.startsWith(r))) {
        problems.push(`${entry} 入口没有静态导入 ${r}（首次唤出会产生懒加载请求）`)
      }
    }
  }
  return problems
}

/**
 * panel 入口的首屏体积与各视图的去向（只报告）：静态可达 chunk 的 JS / CSS 字节数，每个视图目录的渲染后字节数及它是
 * 静态导入还是 lazy()。懒加载取舍用这组数字估算首屏 JS 的变化
 */
export function panelReport(chunks, readCss = () => 0) {
  const entry = entryChunk(chunks, 'panel')
  if (!entry) return null
  const byFile = new Map(chunks.map((c) => [c.fileName, c]))
  const stat = staticClosure(chunks, entry.fileName)
  let js = 0
  const css = new Set()
  for (const f of stat.chunks) {
    const c = byFile.get(f)
    js += c?.size ?? 0
    for (const x of c?.css ?? []) css.add(x)
  }
  const cssBytes = [...css].reduce((sum, f) => sum + readCss(f), 0)
  const views = {}
  for (const view of ['translate', 'ocr', 'clipboard', 'input', 'toast']) {
    const prefix = `src/views/${view}/`
    let bytes = 0
    let lazy = 0
    for (const c of chunks) {
      const inStatic = stat.chunks.has(c.fileName)
      for (const [id, n] of Object.entries(c.moduleSizes ?? {})) {
        if (!moduleKey(id).startsWith(prefix)) continue
        bytes += n
        if (!inStatic) lazy += n
      }
    }
    views[view] = {
      bytes,
      mode: bytes === 0 ? 'missing' : lazy === 0 ? 'static' : lazy === bytes ? 'lazy' : 'mixed',
    }
  }
  return { js, css: cssBytes, chunks: stat.chunks.size, views }
}

// ---------------------------------------------------------------------------
// 体积

/** overlay.html 引用的入口 JS、modulepreload 与样式表（相对 dist 的路径） */
export function overlayFiles(html) {
  const files = new Set()
  for (const m of html.matchAll(/<script\b[^>]*\btype="module"[^>]*\bsrc="([^"]+)"/g))
    files.add(m[1])
  for (const m of html.matchAll(
    /<link\b[^>]*\brel="(?:modulepreload|stylesheet)"[^>]*\bhref="([^"]+)"/g,
  )) {
    files.add(m[1])
  }
  for (const m of html.matchAll(
    /<link\b[^>]*\bhref="([^"]+)"[^>]*\brel="(?:modulepreload|stylesheet)"/g,
  )) {
    files.add(m[1])
  }
  return [...files].filter((f) => !/^(?:https?:|data:)/.test(f)).map((f) => f.replace(/^\.?\//, ''))
}

function checkFrontend(dist) {
  const problems = []
  const files = walk(dist).filter((f) => !f.endsWith('.map'))
  if (files.length === 0) throw new Error(`${rel(dist)} 为空：先构建前端`)
  const total = files.reduce((s, f) => s + (sizeOf(f) ?? 0), 0)
  const rows = [['前端资源合计', total, BUDGET.frontend]]

  const overlayHtml = join(dist, 'overlay.html')
  const html = readFileSync(overlayHtml, 'utf8')
  const parts = overlayFiles(html).map((f) => ({ f, size: sizeOf(join(dist, f)) }))
  const missing = parts.filter((p) => p.size === null)
  if (missing.length)
    problems.push(`overlay.html 引用的文件不存在：${missing.map((p) => p.f).join('、')}`)
  const overlay = parts.reduce((s, p) => s + (p.size ?? 0), 0)
  rows.push(['Overlay（入口 + 静态 import + CSS）', overlay, BUDGET.overlay])

  console.log('前端资源（压缩前）：')
  for (const [name, size, max] of rows) {
    const line = `  ${name.padEnd(28)} ${fmt(size).padStart(10)} / ${fmt(max)}（${((size / max) * 100).toFixed(0)}%）`
    console.log(line)
    if (size > max) problems.push(`${name} ${fmt(size)} 超出预算 ${fmt(max)}`)
  }
  console.log('  Overlay 构成：')
  for (const p of parts) {
    const gz = p.size === null ? 0 : gzipSync(readFileSync(join(dist, p.f))).length
    console.log(`    ${p.f.padEnd(44)} ${fmt(p.size ?? 0).padStart(10)}（gzip ${fmt(gz)}）`)
  }
  appendSummary(
    [
      '### 前端体积',
      '',
      '| 项 | 大小 | 预算 |',
      '| --- | ---: | ---: |',
      ...rows.map(([n, s, m]) => `| ${n} | ${fmt(s)} | ${fmt(m)} |`),
    ].join('\n'),
  )
  return problems
}

/** 递归查找安装包并检查体积；返回 `{ rows, problems }` */
export function checkBundles(dir) {
  const files = walk(
    dir,
    (name) =>
      name === 'node_modules' || name === '.git' || name === 'deps' || name === 'incremental',
  )
  const rows = []
  for (const f of files) {
    const name = basename(f)
    const b = BUDGET.bundles.find((x) => x.test(name))
    if (b)
      rows.push({
        file: relative(ROOT, f).split(sep).join('/'),
        kind: b.kind,
        size: sizeOf(f) ?? 0,
        max: b.max,
      })
  }
  const problems = rows
    .filter((r) => r.size > r.max)
    .map((r) => `${r.file}（${r.kind}）${fmt(r.size)} 超出预算 ${fmt(r.max)}`)
  if (rows.length === 0)
    problems.push(
      `${rel(dir) || dir} 下没有找到任何安装包（dmg / setup.exe / deb / rpm / AppImage）`,
    )
  return { rows, problems }
}

function selfTest() {
  const chunk = (fileName, moduleIds, extra = {}) => ({
    fileName,
    isEntry: false,
    facadeModuleId: null,
    imports: [],
    dynamicImports: [],
    moduleIds,
    moduleSizes: Object.fromEntries(moduleIds.map((m) => [m, 100])),
    size: 100 * moduleIds.length,
    css: [],
    ...extra,
  })
  const root = '/r/apps/desktop/'
  const entry = (name, mods, extra) =>
    chunk(`${name}.js`, mods, { isEntry: true, facadeModuleId: `${root}${name}.html`, ...extra })
  const base = () => [
    entry('settings', [`${root}src/window/settings.tsx`]),
    entry('overlay', [`${root}src/overlay/index.ts`]),
    // 固定窗口里 OCR 的芯片条用的是共享部件（shared/smart），不经过剪贴板视图的目录
    entry('pinned', [`${root}src/pinned/PinnedApp.tsx`, `${root}src/shared/smart/EntityChip.tsx`]),
    entry('clipfloat', [
      `${root}src/floating/FloatingApp.tsx`,
      `${root}src/views/clipboard/ClipboardView.tsx`,
    ]),
  ]
  // 五个视图都静态导入（toast 在共享 chunk 里）：符合规则
  const staticViews = [
    ...base(),
    entry(
      'panel',
      [
        `${root}src/panel/main.tsx`,
        `${root}src/views/translate/T.tsx`,
        `${root}src/views/ocr/O.tsx`,
        `${root}src/views/clipboard/C.tsx`,
        `${root}src/views/input/I.tsx`,
      ],
      { imports: ['shared.js'], dynamicImports: ['preview.js'] },
    ),
    chunk('shared.js', [`${root}src/views/toast/Toast.tsx`, '/r/node_modules/react/index.js']),
    // 视图内部的非首屏部分可以 lazy()（剪贴板预览）
    chunk('preview.js', [`${root}src/views/clipboard/Preview.tsx`]),
  ]
  assert.deepEqual(checkGraph(staticViews), [])
  const report = panelReport(staticViews)
  assert.equal(report.views.translate.mode, 'static')
  assert.equal(report.views.clipboard.mode, 'mixed')
  assert.equal(report.js, 700) // panel.js（5 个模块）+ shared.js（2 个），lazy() 出去的预览不算首屏
  // 任何一个视图整体 lazy()：首次唤出有懒加载请求，一律失败
  for (const [view, file] of [
    ['translate', 'T.tsx'],
    ['ocr', 'O.tsx'],
    ['input', 'I.tsx'],
  ]) {
    const lazyView = staticViews.map((c) =>
      c.fileName === 'panel.js'
        ? {
            ...c,
            moduleIds: c.moduleIds.filter((m) => !m.endsWith(file)),
            dynamicImports: [...c.dynamicImports, 'lazy.js'],
          }
        : c,
    )
    lazyView.push(chunk('lazy.js', [`${root}src/views/${view}/${file}`]))
    assert.ok(
      checkGraph(lazyView).some((p) => p.includes(`src/views/${view}/`) && p.includes('懒加载')),
      `${view} 视图 lazy() 应失败`,
    )
  }
  // lazy() 出去的 chunk 带进了设置页的代码
  const leaky = staticViews.map((c) =>
    c.fileName === 'preview.js'
      ? { ...c, moduleIds: [...c.moduleIds, `${root}src/settings/pages/ClipboardPage.tsx`] }
      : c,
  )
  assert.ok(checkGraph(leaky).some((p) => p.includes('lazy()') && p.includes('src/settings/')))
  // overlay 静态依赖 React
  const reactOverlay = staticViews.map((c) =>
    c.fileName === 'overlay.js' ? { ...c, imports: ['shared.js'] } : c,
  )
  assert.ok(checkGraph(reactOverlay).some((p) => p.startsWith('overlay') && p.includes('react')))
  // 固定窗口静态带上了剪贴板视图（应只在悬浮剪贴板的入口里）
  const heavyPinned = staticViews.map((c) =>
    c.fileName === 'pinned.js'
      ? { ...c, moduleIds: [...c.moduleIds, `${root}src/views/clipboard/ClipboardView.tsx`] }
      : c,
  )
  assert.ok(
    checkGraph(heavyPinned).some((p) => p.startsWith('pinned') && p.includes('ClipboardView')),
  )
  // 剪贴板目录里的小模块同样不行：共用的部件应放进 shared，固定窗口不反向依赖剪贴板视图
  const clipPinned = staticViews.map((c) =>
    c.fileName === 'pinned.js'
      ? { ...c, moduleIds: [...c.moduleIds, `${root}src/views/clipboard/specs.ts`] }
      : c,
  )
  assert.ok(
    checkGraph(clipPinned).some(
      (p) => p.startsWith('pinned') && p.includes('src/views/clipboard/specs.ts'),
    ),
  )
  // 悬浮剪贴板把剪贴板视图 lazy() 出去：首帧交接前多一次请求
  const lazyFloat = staticViews.map((c) =>
    c.fileName === 'clipfloat.js'
      ? { ...c, moduleIds: [`${root}src/floating/FloatingApp.tsx`], dynamicImports: ['cv.js'] }
      : c,
  )
  lazyFloat.push(chunk('cv.js', [`${root}src/views/clipboard/ClipboardView.tsx`]))
  assert.ok(checkGraph(lazyFloat).some((p) => p.startsWith('clipfloat') && p.includes('懒加载')))
  ok('check-size 自检通过（入口依赖图规则）')
}

async function main() {
  const { values } = parseArgs({
    options: {
      dist: { type: 'string' },
      bundles: { type: 'string' },
      'self-test': { type: 'boolean', default: false },
      help: { type: 'boolean', short: 'h', default: false },
    },
  })
  if (values['self-test']) return selfTest()
  if (values.help) {
    console.log('用法：node scripts/check-size.mjs [--dist <目录>] | --bundles <目录>')
    return
  }
  let problems = []
  if (values.bundles) {
    const { rows, problems: p } = checkBundles(resolve(process.cwd(), values.bundles))
    console.log('安装包体积：')
    for (const r of rows) {
      console.log(`  ${r.file.padEnd(72)} ${fmt(r.size).padStart(10)} / ${fmt(r.max)}`)
    }
    appendSummary(
      [
        '### 安装包体积',
        '',
        '| 文件 | 类型 | 大小 | 预算 |',
        '| --- | --- | ---: | ---: |',
        ...rows.map((r) => `| ${basename(r.file)} | ${r.kind} | ${fmt(r.size)} | ${fmt(r.max)} |`),
      ].join('\n'),
    )
    problems = p
  } else if (values.dist) {
    problems = checkFrontend(resolve(process.cwd(), values.dist))
  } else {
    const outDir = join(ROOT, 'target', 'size-check', 'dist')
    const started = Date.now()
    const chunks = await buildWithGraph(outDir)
    console.log(
      `生产构建完成（${((Date.now() - started) / 1000).toFixed(1)} s，${chunks.length} 个 chunk）→ ${rel(outDir)}`,
    )
    problems = [...checkFrontend(outDir), ...checkGraph(chunks)]
    const pr = panelReport(chunks, (f) => sizeOf(join(outDir, f)) ?? 0)
    if (pr) {
      const mode = { static: '静态', lazy: 'lazy()', mixed: '部分 lazy()', missing: '—' }
      console.log(
        `panel 首屏（静态可达 ${pr.chunks} 个 chunk）：JS ${fmt(pr.js)}、CSS ${fmt(pr.css)}；各视图（渲染后）：${Object.entries(
          pr.views,
        )
          .map(([v, x]) => `${v} ${fmt(x.bytes)} ${mode[x.mode]}`)
          .join('、')}`,
      )
      appendSummary(
        [
          '### panel 首屏（只报告）',
          '',
          `JS ${fmt(pr.js)}、CSS ${fmt(pr.css)}（静态可达 ${pr.chunks} 个 chunk）`,
          '',
          '| 视图 | 渲染后 | 加载方式 |',
          '| --- | ---: | --- |',
          ...Object.entries(pr.views).map(
            ([v, x]) => `| ${v} | ${fmt(x.bytes)} | ${mode[x.mode]} |`,
          ),
        ].join('\n'),
      )
    }
    if (!problems.some((p) => p.includes('入口'))) ok('入口依赖图符合规则')
  }
  for (const p of problems) error(p)
  if (problems.length) throw new Error(`体积 / 依赖图检查未通过（${problems.length} 项）`)
  ok('体积检查通过')
}

if (isMain(import.meta.url)) await run(main)
