#!/usr/bin/env node
/**
 * scripts/release/finalize.mjs：release.yml 的 finalize 作业。
 *
 * 在全部平台的 build 作业结束后（某个平台失败也会运行，重跑失败的作业后它会跟着重跑）：
 *   1. 核对草稿里的产物恰好是约定的清单（lib.mjs 的 ARTIFACTS）：macOS arm64 dmg + app.tar.gz、Windows x64 NSIS、
 *      Linux deb / rpm / AppImage（有更新器私钥时每个更新器产物都带 .sig），没有多余文件（MSI、universal 等）；
 *   2. 有私钥时：下载每个更新器产物与 .sig，用 tauri.conf.json 的公钥逐个校验（签名必须能被公钥校验）。
 *      在此之前先核对更新器配置是正式的那一份（端点、公钥 key id、没有 dangerousInsecureTransportProtocol；
 *      lib.mjs 的 assertOfficialUpdaterConfig）——只用配置里的公钥校验，换掉的公钥也能「校验通过」；
 *   3. 生成 latest.json（darwin-aarch64、windows-x86_64 → NSIS、linux-x86_64 → AppImage，另带 -deb / -rpm 等
 *      安装器键），替换草稿里的旧文件。只收录校验通过的平台；缺失或校验失败的平台使作业失败。本次不生成
 *      latest.json 时（没有私钥，或没有一个平台通过校验）删除草稿里上一次运行留下的旧文件。notes 只取正文里
 *      `<!-- wisp:notes -->` 标记之间的分组变更（不含测试草稿 / 缺私钥提示、下载表格与打开说明）；
 *   4. 计算每个安装包的 SHA-256（GitHub 给资源记了 digest 时与之核对），写进 Release 正文 `<!-- wisp:checksums -->`
 *      标记之间（安装包没有代码签名，用户可以据此确认下载完整）。某个平台失败时只列已有的产物，重跑后补齐。
 *
 * 用法：
 *   node scripts/release/finalize.mjs --release-id <id> --tag <tag> [--no-updater]
 *   node scripts/release/finalize.mjs --local <目录> --tag <tag> [--no-updater] [--platforms macos]
 *     本机演练：把 <目录> 下的文件当作草稿资源，生成 latest.json 打印出来（不访问 GitHub）
 */
/* oxlint-disable no-await-in-loop -- 逐个下载、校验产物（并发下载上百 MB 没有必要） */
import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { basename } from 'node:path'
import { parseArgs } from 'node:util'
import { currentVersion } from '../bump.mjs'
import {
  appendSummary,
  error,
  formatBytes,
  info,
  isMain,
  ok,
  run,
  walk,
  warn,
} from '../lib/common.mjs'
import {
  ARTIFACTS,
  CHECKSUMS_END,
  CHECKSUMS_START,
  GitHub,
  assertOfficialUpdaterConfig,
  buildLatestJson,
  checksumTable,
  classifyAssets,
  configuredPublicKey,
  extractNotes,
  missingArtifacts,
  replaceBlock,
  verifySignature,
  updateRelease,
} from './lib.mjs'

async function main() {
  const { values } = parseArgs({
    options: {
      'release-id': { type: 'string' },
      tag: { type: 'string' },
      'no-updater': { type: 'boolean', default: false },
      local: { type: 'string' },
      platforms: { type: 'string', default: 'macos,windows,linux' },
      help: { type: 'boolean', short: 'h', default: false },
    },
  })
  if (values.help || !values.tag || (!values['release-id'] && !values.local)) {
    console.log(
      '用法：node scripts/release/finalize.mjs --release-id <id> --tag <tag> [--no-updater] | --local <目录> --tag <tag>',
    )
    if (!values.help) throw new Error('缺少 --tag 与 --release-id / --local')
    return
  }
  const version = currentVersion()
  if (!version) throw new Error('各处版本号不一致')
  // 构建用的就是这份配置：端点或公钥被换掉时，签名照样能「校验通过」，已安装的客户端却收不到这个版本
  assertOfficialUpdaterConfig()
  const updater = !values['no-updater']
  const platforms = values.platforms.split(',').map((s) => s.trim())
  const tag = values.tag

  // 资源来源：GitHub 草稿或本机目录
  let gh = null
  let release = null
  let assets
  let read
  if (values.local) {
    const files = walk(values.local)
    assets = files.map((f) => ({ name: basename(f), size: readFileSync(f).length, path: f }))
    read = async (a) => readFileSync(a.path)
  } else {
    gh = new GitHub()
    release = await gh.request('GET', `/releases/${values['release-id']}`)
    if (release.tag_name !== tag)
      throw new Error(`Release ${release.id} 的 tag 是 ${release.tag_name}，不是 ${tag}`)
    if (!release.draft) throw new Error('Release 已经发布：finalize 只处理草稿')
    // prepare 把草稿指向本次构建的提交（指向别的提交时已清掉旧资源）；这里再确认没有被其他运行改过
    const sha = process.env.GITHUB_SHA
    if (
      sha &&
      /^[0-9a-f]{40}$/.test(release.target_commitish) &&
      release.target_commitish !== sha
    ) {
      throw new Error(
        `草稿指向 ${release.target_commitish.slice(0, 12)}，本次构建的是 ${sha.slice(0, 12)}：草稿里的产物可能来自别的提交，请重新运行整个 release 工作流`,
      )
    }
    assets = await gh.assets(release.id)
    read = (a) => gh.download(a.id)
  }

  const problems = []
  const cls = classifyAssets(
    assets.map((a) => a.name),
    version,
  )
  for (const n of cls.unexpected)
    problems.push(`多余的文件：${n}（只出 dmg / app.tar.gz / NSIS / deb / rpm / AppImage）`)
  for (const n of cls.duplicates) problems.push(`重复的产物：${n}`)
  if (!updater) {
    for (const n of cls.sigs.values()) problems.push(`没有更新器私钥的构建却出现了签名：${n}`)
  }
  const missing = missingArtifacts(cls, updater, platforms)
  for (const m of missing) problems.push(`缺少产物：${m}`)

  // 逐个下载产物：计算 SHA-256（与 GitHub 记录的 digest 核对），更新器产物再用公钥校验 .sig
  const entries = []
  const rows = []
  const sums = []
  const byName = new Map(assets.map((a) => [a.name, a]))
  const pub = updater ? configuredPublicKey() : null
  for (const a of ARTIFACTS) {
    const name = cls.found.get(a.id)
    if (!name) continue
    const asset = byName.get(name)
    const data = Buffer.from(await read(asset))
    const sha256 = createHash('sha256').update(data).digest('hex')
    // GitHub 为新上传的资源记录 `digest`（sha256:<hex>）；对不上说明下载或上传过程出了问题
    if (typeof asset.digest === 'string' && asset.digest.startsWith('sha256:')) {
      if (asset.digest.slice('sha256:'.length) !== sha256)
        problems.push(`${name}：下载内容的 SHA-256 与 GitHub 记录的 digest 不一致`)
    }
    sums.push({ name, sha256 })
    let state = '—'
    if (pub && a.updater && cls.sigs.has(a.id)) {
      const sigText = Buffer.from(await read(byName.get(cls.sigs.get(a.id))))
        .toString('utf8')
        .trim()
      try {
        verifySignature(data, sigText, pub)
        entries.push({ id: a.id, name, signature: sigText })
        state = '签名有效'
      } catch (e) {
        state = '签名无效'
        problems.push(`${name}：${e instanceof Error ? e.message : e}`)
      }
    }
    rows.push({ platform: a.platform, name, size: asset.size, state, sha256 })
  }

  console.log('草稿中的产物：')
  for (const r of rows)
    console.log(
      `  ${r.platform.padEnd(8)} ${r.name.padEnd(36)} ${formatBytes(r.size).padStart(10)}  ${r.state.padEnd(4)}  ${r.sha256.slice(0, 16)}…`,
    )
  appendSummary(
    [
      `## 发布产物（${tag}）`,
      '',
      '| 平台 | 文件 | 大小 | 更新器签名 | SHA-256 |',
      '| --- | --- | ---: | --- | --- |',
      ...rows.map(
        (r) =>
          `| ${r.platform} | ${r.name} | ${formatBytes(r.size)} | ${r.state} | \`${r.sha256}\` |`,
      ),
    ].join('\n'),
  )

  // latest.json：有私钥且至少一个平台校验通过时生成；否则草稿里不能留着上一次运行的旧文件——它的签名与本次的产物
  // 对不上，草稿一旦发布，更新器会下载到签名不匹配的包
  let latestText = null
  let latestKeys = []
  if (updater) {
    if (entries.length === 0) {
      problems.push('没有任何通过校验的更新器产物，不生成 latest.json')
    } else {
      let notes = ''
      if (release) {
        notes = extractNotes(release.body ?? '') ?? ''
        if (!release.body?.includes('wisp:notes')) {
          warn(
            'Release 正文里没有 <!-- wisp:notes --> 标记（被手工改过？）：latest.json 的 notes 留空',
          )
        }
      }
      const latest = buildLatestJson({
        version,
        notes,
        pubDate: new Date().toISOString(),
        repo: process.env.GITHUB_REPOSITORY ?? 'yzlltyyh/wisp',
        tag,
        entries,
      })
      latestText = `${JSON.stringify(latest, null, 2)}\n`
      latestKeys = Object.keys(latest.platforms)
      info(`latest.json：${latestKeys.join('、')}`)
    }
  } else {
    info('没有更新器私钥：不生成 latest.json')
  }
  if (gh) {
    const stale = assets.filter((x) => x.name === 'latest.json')
    for (const old of stale) await gh.request('DELETE', `/releases/assets/${old.id}`)
    if (latestText) {
      await gh.upload(
        release,
        'latest.json',
        new TextEncoder().encode(latestText),
        'application/json',
      )
      ok('已上传 latest.json')
    } else if (stale.length) {
      warn(
        '已删除草稿里上一次运行留下的 latest.json（本次没有可用的更新器签名，旧文件与本次的产物对不上）',
      )
    }
  } else if (latestText) {
    console.log(latestText)
  }
  if (latestText) {
    appendSummary(`\nlatest.json 平台：${latestKeys.map((k) => `\`${k}\``).join('、')}`)
  }

  // 校验和写进 Release 正文（保留正文其余部分，包括人工编辑过的内容）
  const table = checksumTable(sums)
  if (release) {
    const body = replaceBlock(release.body ?? '', CHECKSUMS_START, CHECKSUMS_END, table)
    if (body !== release.body) {
      await updateRelease(gh, release.id, tag, { body })
      ok(`已把 ${sums.length} 个安装包的 SHA-256 写进 Release 说明`)
    }
  } else {
    console.log(`\n${table}`)
  }

  for (const p of problems) error(p)
  if (problems.length) throw new Error(`发布产物核对未通过（${problems.length} 项）`)
  ok(
    `产物核对通过：${rows.length} 个安装包${updater ? `，${entries.length} 个更新器签名有效` : ''}`,
  )
}

if (isMain(import.meta.url)) await run(main)
