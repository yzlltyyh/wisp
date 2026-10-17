#!/usr/bin/env node
/**
 * scripts/gen-fixtures.mjs：一次重写 / 核对全部由 Rust 生成的前端样例。
 *
 * 用法：
 *   node scripts/gen-fixtures.mjs              重写：逐个以 UPDATE_GOLDEN=1 跑生成用例，最后列出 fixtures 目录的改动
 *   node scripts/gen-fixtures.mjs --check      只核对：不设 UPDATE_GOLDEN，任何一份与 Rust 不一致即失败（CI 的
 *                                              `cargo test --workspace` 已经包含这些用例，这里是本地的快速入口）
 *   node scripts/gen-fixtures.mjs --list       列出生成器与各自写出的文件
 *   node scripts/gen-fixtures.mjs --self-test  不编译：核对生成器清单与仓库一致——fixtures 的每个子目录（consts/、desktop/……）
 *                                              下每个文件都有生成器认领、每个生成器的 Rust 用例文件存在且写的正是清单里的
 *                                              文件（CI 用）
 *
 * 样例都在 `apps/desktop/src/shared/ipc/fixtures/`，Rust 是唯一来源，前端只读不改：
 * - `<Type>[.<variant>].json` + `index.ts`：IPC 类型的黄金样例（wisp-types `tests/golden.rs`）；
 * - `consts/HotkeyDefaults.json`：三端默认快捷键（首选 / 备选，wisp-types `tests/consts.rs`）——设置页显示「Rust 尝试的组合」；
 * - `consts/EntityKinds.json` / `consts/TransformIds.json`：识别种类与「粘贴为…」变换的全部取值（wisp-types
 *   `tests/consts.rs`，取自 `EntityKind::ALL` / `TransformId::ALL`）——前端测试拿它比对类型联合与展示注册表；
 * - `consts/OcrLayouts.json`：OCR 结果每种排版能否作为默认、在哪些识别模式下可用（wisp-types `tests/consts.rs`）——
 *   前端测试拿它比对 `shared/ipc/layouts.ts`；
 * - `consts/BuiltinStyles.json`：6 个内置风格（wisp-prompts `tests/fixtures_consts.rs`）——浏览器 mock 的 `styles()`；
 * - `consts/SettingsSections.json` / `consts/SettingsAnchors.json`：设置分组与分组内锚点（wisp-desktop `app::sections`）；
 * - `consts/TransformCases.json`：若干代表性文字 × 适用的每种「粘贴为…」变换的结果（wisp-desktop `clipboard_svc::smart`，
 *   与 `text_inspect` / `text_transform` 同一套函数）——浏览器 mock 的 `text_transform` 只查这张表，不自己实现变换；
 * - `desktop/IpcError.json`：命令错误的形状（wisp-desktop `error::tests::golden_ipc_error`，前端 golden.test.ts 校验键集合）。
 */
import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { existsSync, readdirSync, readFileSync } from 'node:fs'
import { join } from 'node:path'
import { parseArgs } from 'node:util'
import { ROOT, error, info, isMain, ok, run } from './lib/common.mjs'

const FIXTURES = 'apps/desktop/src/shared/ipc/fixtures'

/** 生成器：Rust 用例（cargo test 参数）与它写出的文件（相对仓库根；`*.json` 表示该目录下顶层的全部样例） */
export const GENERATORS = [
  {
    name: 'IPC 黄金样例',
    source: 'crates/wisp-types/tests/golden.rs',
    cargo: ['-p', 'wisp-types', '--test', 'golden'],
    outputs: [`${FIXTURES}/*.json`, `${FIXTURES}/index.ts`],
  },
  {
    name: '默认快捷键、识别种类、变换清单与排版规则',
    source: 'crates/wisp-types/tests/consts.rs',
    cargo: ['-p', 'wisp-types', '--test', 'consts'],
    outputs: [
      `${FIXTURES}/consts/HotkeyDefaults.json`,
      `${FIXTURES}/consts/EntityKinds.json`,
      `${FIXTURES}/consts/TransformIds.json`,
      `${FIXTURES}/consts/OcrLayouts.json`,
    ],
  },
  {
    name: '内置风格',
    source: 'crates/wisp-prompts/tests/fixtures_consts.rs',
    cargo: ['-p', 'wisp-prompts', '--test', 'fixtures_consts'],
    outputs: [`${FIXTURES}/consts/BuiltinStyles.json`],
  },
  {
    name: '设置分组与锚点',
    source: 'apps/desktop/src-tauri/src/app/mod.rs',
    cargo: ['-p', 'wisp-desktop', '--lib', 'app::sections'],
    outputs: [
      `${FIXTURES}/consts/SettingsSections.json`,
      `${FIXTURES}/consts/SettingsAnchors.json`,
    ],
  },
  {
    name: '文本变换样例',
    source: 'apps/desktop/src-tauri/src/clipboard_svc/smart.rs',
    cargo: ['-p', 'wisp-desktop', '--lib', 'clipboard_svc::smart::'],
    outputs: [`${FIXTURES}/consts/TransformCases.json`],
  },
  {
    name: '命令错误（IpcError）',
    source: 'apps/desktop/src-tauri/src/error.rs',
    cargo: ['-p', 'wisp-desktop', '--lib', 'error::'],
    outputs: [`${FIXTURES}/desktop/IpcError.json`],
  },
]

/** 生成器清单与仓库是否一致；返回问题列表（不编译、不运行 cargo） */
export function checkManifest(root = ROOT, generators = GENERATORS) {
  const problems = []
  const claimed = new Set(generators.flatMap((g) => g.outputs))
  // 顶层的 `<Type>.json` 由黄金样例的通配认领；每个子目录（consts/、desktop/……）下的文件都要逐个认领
  const dir = join(root, FIXTURES)
  const subdirs = existsSync(dir)
    ? readdirSync(dir, { withFileTypes: true })
        .filter((e) => e.isDirectory())
        .map((e) => e.name)
    : []
  for (const sub of subdirs) {
    for (const f of readdirSync(join(dir, sub))) {
      if (f.startsWith('.')) continue // .DS_Store 之类
      if (!claimed.has(`${FIXTURES}/${sub}/${f}`)) {
        problems.push(
          `${FIXTURES}/${sub}/${f} 没有生成器认领（fixtures 的子目录下只放 Rust 生成的样例；新增时在 GENERATORS 里登记）`,
        )
      }
    }
  }
  for (const g of generators) {
    const src = join(root, g.source)
    if (!existsSync(src)) {
      problems.push(`${g.name}：生成用例 ${g.source} 不存在`)
      continue
    }
    const text = readFileSync(src, 'utf8')
    for (const out of g.outputs) {
      const name = out.split('/').at(-1)
      if (name === '*.json') continue
      if (!existsSync(join(root, out)))
        problems.push(`${g.name}：${out} 不存在（运行 node scripts/gen-fixtures.mjs）`)
      if (!text.includes(name)) problems.push(`${g.name}：${g.source} 里没有写 ${name} 的代码`)
    }
  }
  return problems
}

function cargoTest(g, update) {
  const args = ['test', ...g.cargo, '--locked']
  info(`${update ? '重写' : '核对'} ${g.name}：cargo ${args.join(' ')}`)
  const r = spawnSync('cargo', args, {
    cwd: ROOT,
    stdio: 'inherit',
    env: { ...process.env, UPDATE_GOLDEN: update ? '1' : '0' },
  })
  if (r.error) throw new Error(`无法运行 cargo：${r.error.message}`)
  return r.status === 0
}

function selfTest() {
  assert.deepEqual(checkManifest(), [], '生成器清单与仓库应一致')
  // 未登记的 consts 文件与缺失的生成用例都要报出来
  const fake = [
    {
      name: 'x',
      source: 'crates/wisp-types/tests/consts.rs',
      cargo: [],
      outputs: [`${FIXTURES}/consts/Nope.json`],
    },
    { name: 'y', source: 'crates/none/tests/none.rs', cargo: [], outputs: [] },
  ]
  const problems = checkManifest(ROOT, fake)
  assert.ok(problems.some((p) => p.includes('HotkeyDefaults.json') && p.includes('没有生成器认领')))
  assert.ok(
    problems.some((p) => p.includes('desktop/IpcError.json') && p.includes('没有生成器认领')),
    'desktop/ 子目录同样要逐个认领',
  )
  assert.ok(problems.some((p) => p.includes('Nope.json') && p.includes('不存在')))
  assert.ok(problems.some((p) => p.includes('crates/none/tests/none.rs')))
  ok(`gen-fixtures 自检通过（${GENERATORS.length} 个生成器，fixtures 子目录下的文件都有认领）`)
}

function main() {
  const { values } = parseArgs({
    options: {
      check: { type: 'boolean', default: false },
      list: { type: 'boolean', default: false },
      'self-test': { type: 'boolean', default: false },
      help: { type: 'boolean', short: 'h', default: false },
    },
  })
  if (values.help) {
    console.log('用法：node scripts/gen-fixtures.mjs [--check | --list | --self-test]')
    return
  }
  if (values['self-test']) return selfTest()
  if (values.list) {
    for (const g of GENERATORS) {
      console.log(`${g.name}（cargo test ${g.cargo.join(' ')}）`)
      for (const o of g.outputs) console.log(`  ${o}`)
    }
    return
  }
  const manifest = checkManifest()
  if (manifest.length && values.check) {
    for (const p of manifest) error(p)
    throw new Error('生成器清单与仓库不一致')
  }
  const failed = GENERATORS.filter((g) => !cargoTest(g, !values.check))
  if (failed.length) {
    throw new Error(
      `${failed.map((g) => g.name).join('、')} ${values.check ? '与 Rust 不一致（运行 node scripts/gen-fixtures.mjs 重写，确认改动符合契约后提交）' : '生成失败'}`,
    )
  }
  if (values.check) {
    ok('全部样例与 Rust 一致')
    return
  }
  const status = spawnSync('git', ['status', '--short', '--', FIXTURES], {
    cwd: ROOT,
    encoding: 'utf8',
  })
  const changed = (status.stdout ?? '').trim()
  info(changed ? `fixtures 的改动：\n${changed}` : 'fixtures 没有改动')
  const after = checkManifest()
  for (const p of after) error(p)
  if (after.length) throw new Error('生成器清单与仓库不一致')
  ok('样例已重写')
}

if (isMain(import.meta.url)) await run(main)
