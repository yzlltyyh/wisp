#!/usr/bin/env node
/**
 * scripts/release/upload.mjs：release.yml 的 build 作业在构建与核对都通过之后，把本平台的
 * 安装包与更新器签名上传到 prepare 建好的草稿（按 release id，不按 tag 查找，三个平台不会各自建出同名 Release）。
 *
 * 为什么单独一步：构建（`pnpm tauri build`）会执行 vite 及其插件、几百个 crate 的 build.rs 与过程宏，这些第三方代码
 * 和那一步的环境变量在同一个进程树里。上传需要 `contents: write` 的 GITHUB_TOKEN——把它从构建步骤里拿掉，只交给这一步，
 * 而这一步只运行本仓库的这个脚本（Node 内置模块，不加载任何依赖包），构建期的代码就碰不到可写令牌。
 *
 * 规则（与 lib.mjs 的 ARTIFACTS 同一份清单）：
 *   - 只上传本平台的产物：macOS dmg + app.tar.gz、Windows NSIS、Linux deb / rpm / AppImage；有私钥时每个更新器产物带 .sig；
 *   - tauri-cli 输出的 `bundle/macos/Wisp.app.tar.gz(.sig)` 改名为 `Wisp_<版本>_aarch64.app.tar.gz(.sig)`（与 tauri-action v1
 *     的命名一致），其余文件名原样；
 *   - 先核对清单：缺任何一个就失败，一个都不上传（草稿里不会只有半套产物）；
 *   - 草稿里已有同名资源（重跑）时先删除再上传；上传失败重试 3 次。MSI、universal 等清单之外的文件不上传。
 *
 * 用法：
 *   node scripts/release/upload.mjs --release-id <id> --platform macos|windows|linux --bundle-dir <目录> [--no-updater]
 *   node scripts/release/upload.mjs --dry-run --platform macos --bundle-dir target/release/bundle [--no-updater]
 *     只打印将要上传的文件（不访问 GitHub）
 * 环境变量：GITHUB_REPOSITORY、GITHUB_TOKEN（--dry-run 时不需要）、GITHUB_SHA（核对草稿指向本次构建的提交）。
 */
/* oxlint-disable no-await-in-loop -- 逐个上传（并发上传上百 MB 没有必要，失败重试也更好判断） */
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { parseArgs } from 'node:util'
import { currentVersion } from '../bump.mjs'
import {
  appendSummary,
  error,
  formatBytes,
  info,
  isMain,
  ok,
  rel,
  run,
  walk,
  warn,
} from '../lib/common.mjs'
import { BUILD_TARGETS, GitHub, planUploads } from './lib.mjs'

/**
 * 上传一个资源并替换草稿里的同名旧资源。`gh` 需要 assets / request / upload（lib.mjs 的 GitHub；自检传入内存实现）。
 * 每次尝试前都重新列一次资源：上一次尝试可能在 GitHub 那边留下了同名的半成品
 */
export async function replaceAsset(
  gh,
  release,
  name,
  bytes,
  { attempts = 3, delay = 1000, log = warn } = {},
) {
  for (let attempt = 1; ; attempt++) {
    const old = (await gh.assets(release.id)).filter((a) => a.name === name)
    for (const a of old) await gh.request('DELETE', `/releases/assets/${a.id}`)
    try {
      return await gh.upload(release, name, bytes, 'application/octet-stream')
    } catch (e) {
      if (attempt >= attempts) throw e
      const wait = delay * 2 ** attempt
      log(
        `上传 ${name} 失败（第 ${attempt} 次：${e instanceof Error ? e.message : e}），${wait / 1000} s 后重试`,
      )
      await new Promise((r) => setTimeout(r, wait))
    }
  }
}

async function main() {
  const { values } = parseArgs({
    options: {
      'release-id': { type: 'string' },
      platform: { type: 'string' },
      'bundle-dir': { type: 'string' },
      'no-updater': { type: 'boolean', default: false },
      'dry-run': { type: 'boolean', default: false },
      help: { type: 'boolean', short: 'h', default: false },
    },
  })
  const usage =
    '用法：node scripts/release/upload.mjs --release-id <id> --platform macos|windows|linux --bundle-dir <目录> [--no-updater] [--dry-run]'
  if (values.help) {
    console.log(usage)
    return
  }
  const dryRun = values['dry-run']
  const platforms = BUILD_TARGETS.map((t) => t.name)
  if (!platforms.includes(values.platform ?? ''))
    throw new Error(`--platform 必须是 ${platforms.join(' / ')}。${usage}`)
  if (!values['bundle-dir']) throw new Error(`缺少 --bundle-dir。${usage}`)
  if (!dryRun && !values['release-id']) throw new Error(`缺少 --release-id。${usage}`)
  const version = currentVersion()
  if (!version) throw new Error('各处版本号不一致')
  const updater = !values['no-updater']
  const dir = resolve(process.cwd(), values['bundle-dir'])
  // .app 是目录（几千个文件），AppImage 的 AppDir 同理：都不是要上传的东西，不进入
  const files = walk(dir, (name) => name.endsWith('.app') || name.endsWith('.AppDir'))
  const plan = planUploads(files, { platform: values.platform, version, updater })
  for (const m of plan.missing) error(`缺少产物：${m}（${rel(dir)}）`)
  for (const p of plan.problems) error(p)
  if (plan.missing.length || plan.problems.length) {
    throw new Error(
      `本平台的产物不全或有歧义，一个都没有上传（${updater ? '有私钥：每个更新器产物都应带 .sig' : '没有私钥：只核对安装包'}）`,
    )
  }
  info(
    `${values.platform}：${plan.uploads.length} 个文件，版本 ${version}${updater ? '' : '，没有更新器签名'}`,
  )

  if (dryRun) {
    for (const u of plan.uploads) console.log(`  ${u.name.padEnd(40)} ← ${rel(u.path)}`)
    return
  }

  const gh = new GitHub()
  const release = await gh.request('GET', `/releases/${values['release-id']}`)
  if (!release.draft) throw new Error(`Release ${release.id} 已经发布：只往草稿里上传`)
  const sha = process.env.GITHUB_SHA
  if (sha && /^[0-9a-f]{40}$/.test(release.target_commitish) && release.target_commitish !== sha) {
    throw new Error(
      `草稿指向 ${release.target_commitish.slice(0, 12)}，本次构建的是 ${sha.slice(0, 12)}：可能有另一个运行改了这份草稿，请重新运行整个 release 工作流`,
    )
  }
  const rows = []
  for (const u of plan.uploads) {
    const bytes = readFileSync(u.path)
    await replaceAsset(gh, release, u.name, bytes)
    ok(`已上传 ${u.name}（${formatBytes(bytes.length)}）`)
    rows.push(`| ${u.name} | ${formatBytes(bytes.length)} |`)
  }
  appendSummary(
    [`## 上传到草稿（${values.platform}）`, '', '| 文件 | 大小 |', '| --- | ---: |', ...rows].join(
      '\n',
    ),
  )
}

if (isMain(import.meta.url)) await run(main)
