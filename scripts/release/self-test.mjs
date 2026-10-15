#!/usr/bin/env node
/**
 * scripts/release/self-test.mjs：发布脚本的离线自检（prepare 作业的第一步、CI 的 Linux test 作业都运行，
 * 不访问 GitHub）。覆盖：**仓库里的更新器配置是正式的那一份**（端点、公钥 key id、没有 dangerousInsecureTransportProtocol、
 * no-updater 配置只关更新器产物、release.yml 只用 no-updater 这一个 --config）；tag / 版本判定、Release 正文（下载表的
 * 实际文件名与链接、Linux 的 X11 会话说明、私有仓库与 macOS 签名的提示、校验和占位）与 latest.json 的 notes、
 * CHANGELOG.md 段落的提取与格式（应用内的更新说明不渲染链接）、变更日志范围（上一个正式版 tag）、草稿的复用 / 新建与
 * 并发去重、同一提交重跑时保留人工改过的说明（内存里的假 GitHub）、产物文件名与分类、缺失判定、每个平台要上传的文件
 * （upload.mjs，含 macOS 更新包改名）与上传的替换 / 重试（本机回环上的假 GitHub，走真实的 fetch）、latest.json 结构、
 * 校验和表与正文替换、minisign 校验（预哈希与旧格式、key id 不匹配、内容被篡改）。与 tauri-cli 2.12 `tauri signer sign`
 * 实际输出的兼容性已经验证过；产物文件名与签名已在真实 runner 上核对。
 */
import assert from 'node:assert/strict'
import { createHash, generateKeyPairSync, randomBytes, sign } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { createServer } from 'node:http'
import { join } from 'node:path'
import { ROOT, TAURI_DIR, ok, run } from '../lib/common.mjs'
import {
  NOTES_END,
  NOTES_START,
  changelogSection,
  mergeDraftBody,
  previousStableTag,
  releaseBody,
  resolveMeta,
  upsertDraft,
} from './prepare.mjs'
import {
  ARTIFACTS,
  BUILD_TARGETS,
  CHECKSUMS_END,
  CHECKSUMS_START,
  GitHub,
  OFFICIAL_UPDATER_ENDPOINT,
  OFFICIAL_UPDATER_KEY_ID,
  PRODUCT,
  artifactName,
  assertOfficialUpdaterConfig,
  buildLatestJson,
  buildMatrix,
  checkUpdaterConfig,
  checksumTable,
  classifyAssets,
  configuredPublicKey,
  extractNotes,
  missingArtifacts,
  parsePublicKey,
  parseSignature,
  planUploads,
  replaceBlock,
  resolvePlatforms,
  verifySignature,
  updateRelease,
} from './lib.mjs'
import { replaceAsset } from './upload.mjs'

const b64 = (s) => Buffer.from(s).toString('base64')

/** 生成 minisign 格式的密钥对（与 tauri signer 相同的「文本再 base64」形式） */
function keypair() {
  const { publicKey, privateKey } = generateKeyPairSync('ed25519')
  const raw = publicKey.export({ format: 'der', type: 'spki' }).subarray(-32)
  const keyNum = randomBytes(8)
  const id = Buffer.from(keyNum.toReversed()).toString('hex').toUpperCase()
  const pub = b64(
    `untrusted comment: minisign public key: ${id}\n${b64(Buffer.concat([Buffer.from('Ed'), keyNum, raw]))}\n`,
  )
  return { pub, privateKey, keyNum, id }
}

function signData(k, data, algorithm = 'ED') {
  const msg = algorithm === 'ED' ? createHash('blake2b512').update(data).digest() : data
  const sig = sign(null, msg, k.privateKey)
  const tc = 'timestamp:1791628604\tfile:Wisp_0.1.0_x64-setup.exe'
  const global = sign(null, Buffer.concat([sig, Buffer.from(tc)]), k.privateKey)
  return b64(
    `untrusted comment: signature from tauri secret key\n${b64(Buffer.concat([Buffer.from(algorithm), k.keyNum, sig]))}\ntrusted comment: ${tc}\n${b64(global)}\n`,
  )
}

function testMeta() {
  const base = { version: '0.1.0', runNumber: '42', hasKey: true }
  assert.deepEqual(resolveMeta({ ...base, ref: 'refs/tags/v0.1.0' }), {
    tag: 'v0.1.0',
    version: '0.1.0',
    prerelease: false,
    updater: true,
    test: false,
  })
  assert.throws(() => resolveMeta({ ...base, ref: 'refs/tags/v0.1.1' }), /不一致/)
  const draft = resolveMeta({ ...base, ref: 'refs/heads/ci/wp18' })
  assert.equal(draft.tag, 'v0.1.0-ci.42')
  assert.equal(draft.test, true)
  assert.equal(draft.prerelease, true)
  assert.equal(resolveMeta({ ...base, inputTag: 'v0.1.0' }).test, false)
  assert.throws(() => resolveMeta({ ...base, inputTag: 'v9.9.9' }), /不一致/)
  assert.equal(
    resolveMeta({ ...base, version: '0.2.0-beta.1', ref: 'refs/tags/v0.2.0-beta.1' }).prerelease,
    true,
  )
  assert.equal(resolveMeta({ ...base, withoutKey: true }).updater, false)
  assert.equal(resolveMeta({ ...base, hasKey: false }).updater, false)
  assert.throws(() => resolveMeta({ ...base, version: null }), /不一致/)
}

function testBody() {
  const ctx = { version: '0.1.0', tag: 'v0.1.0', repo: 'yzlltyyh/wisp' }
  const body = (o) => releaseBody({ ...ctx, ...o })
  const withKey = body({ changes: '### 新功能\n\n- x', updater: true, test: false })
  assert.ok(!withKey.includes('TAURI_SIGNING_PRIVATE_KEY'))
  assert.ok(
    withKey.includes('macOS · Apple Silicon') &&
      withKey.includes('Windows · x64') &&
      withKey.includes('不提供 Intel Mac、Windows ARM64 与 universal 包'),
    '写明平台范围',
  )
  assert.ok(withKey.includes('xattr -dr com.apple.quarantine'), '未公证的打开说明')
  // Linux：完整功能需要 X11 会话；写明哪些发行版还有 X11 会话、哪些默认只有 Wayland（Ubuntu 25.10+ / 26.04 LTS、
  // Fedora 43+ 的 GNOME 已经没有 Xorg 会话），rpm 的包名只对 Fedora
  for (const phrase of [
    '完整功能需要 X11 会话',
    'Ubuntu on Xorg',
    'Xfce、MATE、Cinnamon',
    '26.04 LTS',
    'Fedora 43',
    'EPEL',
    'openSUSE',
  ]) {
    assert.ok(withKey.includes(phrase), `Release 说明的 Linux 部分应写明「${phrase}」`)
  }
  assert.ok(
    !/请使用 X11 会话/.test(withKey),
    '不能只说「请使用 X11 会话」（新版 Ubuntu / Fedora 做不到）',
  )
  // 下载表写实际文件名（不是占位符），链接指向本 tag 的资源
  assert.ok(!withKey.includes('<版本>'), '下载表不应有占位符')
  for (const name of [
    'Wisp_0.1.0_aarch64.dmg',
    'Wisp_0.1.0_x64-setup.exe',
    'Wisp_0.1.0_amd64.deb',
    'Wisp-0.1.0-1.x86_64.rpm',
    'Wisp_0.1.0_amd64.AppImage',
  ]) {
    assert.ok(
      withKey.includes(
        `[${name}](https://github.com/yzlltyyh/wisp/releases/download/v0.1.0/${name})`,
      ),
      `下载表应包含 ${name}`,
    )
  }
  assert.ok(!withKey.includes('.app.tar.gz'), '更新器产物不进下载表')
  const noKey = body({ changes: '', updater: false, test: true })
  assert.ok(
    noKey.indexOf('TAURI_SIGNING_PRIVATE_KEY') < noKey.indexOf('### 下载'),
    '缺私钥的警告在顶部',
  )
  assert.ok(noKey.includes('测试草稿'))
  // latest.json 的 notes 只取 git-cliff 的分组变更：不含版本标题、测试草稿 / 缺私钥提示、下载表格与打开说明
  const changes =
    '## 0.2.0（2026-10-10）\n\n### 新功能\n\n- **ui**：新界面\n\n### 修复\n\n- 修一个 bug'
  const notes = extractNotes(body({ changes, updater: false, test: true }))
  assert.equal(notes, '### 新功能\n\n- **ui**：新界面\n\n### 修复\n\n- 修一个 bug')
  for (const leaked of ['CAUTION', 'WARNING', '### 下载', 'xattr', 'SmartScreen', '0.2.0']) {
    assert.ok(!notes.includes(leaked), `notes 不应包含 ${leaked}`)
  }
  // 没有面向用户的提交时 git-cliff 只给出版本标题
  assert.equal(
    extractNotes(body({ changes: '## 0.2.1（2026-10-11）', updater: true, test: false })),
    '本版本没有面向用户的变更。',
  )
  assert.equal(extractNotes('手工改过的正文'), null)
  // 私有仓库：顶部提示自动更新不可用；公开仓库不提示。提示不进 latest.json 的 notes
  const priv = body({ changes, updater: true, test: false, isPrivate: true })
  assert.ok(priv.indexOf('私有') > -1 && priv.indexOf('私有') < priv.indexOf('### 下载'))
  assert.ok(!withKey.includes('私有'))
  assert.ok(!extractNotes(priv).includes('私有'))
  // macOS 签名：ad-hoc 时提醒每次更新都要重新授权、钥匙串要输入登录密码（与 README 安装第 4 步一致）；有固定自签证书时
  // 说明 TCC 授权保持有效，钥匙串不承诺（没有实测）
  assert.ok(withKey.includes('重新授予') && !withKey.includes('固定的自签名证书'))
  assert.ok(withKey.includes('钥匙串会要求输入登录密码，然后选「始终允许」'))
  const cert = body({ changes, updater: true, test: false, macCert: true })
  assert.ok(cert.includes('固定的自签名证书') && !cert.includes('重新授予'))
  assert.ok(cert.includes('钥匙串仍可能要求输入登录密码'))
  // 文档链接：正式版指向 tag，测试草稿指向提交（测试草稿没有 git tag）
  assert.ok(withKey.includes('https://github.com/yzlltyyh/wisp/blob/v0.1.0/README.md#安装'))
  const sha = 'c'.repeat(40)
  assert.ok(
    body({ changes, updater: true, test: true, ref: sha }).includes(
      `/blob/${sha}/docs/user-guide.md`,
    ),
  )
  // 校验和占位：finalize 替换标记之间的内容，其余正文不动
  assert.ok(withKey.includes(CHECKSUMS_START) && withKey.includes(CHECKSUMS_END))
  const table = checksumTable([{ name: 'Wisp_0.1.0_aarch64.dmg', sha256: 'ab'.repeat(32) }])
  const filled = replaceBlock(withKey, CHECKSUMS_START, CHECKSUMS_END, table)
  assert.ok(filled.includes(`| Wisp_0.1.0_aarch64.dmg | \`${'ab'.repeat(32)}\` |`))
  assert.ok(!filled.includes('自动填入'))
  assert.equal(filled.replace(table, ''), withKey.replace('_各平台构建完成后自动填入。_', ''))
  assert.equal(replaceBlock(filled, CHECKSUMS_START, CHECKSUMS_END, table), filled, '重跑时幂等')
  assert.ok(!extractNotes(filled).includes('SHA-256'))
  // 正文被手工删掉了标记：在末尾追加一段，不丢原文
  const appended = replaceBlock('手工正文', CHECKSUMS_START, CHECKSUMS_END, table)
  assert.ok(appended.startsWith('手工正文\n\n') && appended.includes(CHECKSUMS_END))
  assert.equal(checksumTable([]), '_还没有可用的安装包。_')
}

function testChangelogSection() {
  const md = [
    '# 更新日志',
    '',
    '## 0.2.0-beta.1（2026-11-01）',
    '',
    '- 预发布',
    '',
    '## 0.1.10（2026-10-30）',
    '',
    '- 十',
    '',
    '## 0.1.0（2026-10-12）',
    '',
    '### 新功能',
    '',
    '- **翻译**：流式',
    '',
    '#### 细节',
    '',
    '- 不是下一个版本',
    '',
    '## 0.0.9',
    '',
    '- 旧',
  ].join('\n')
  const s = changelogSection(md, '0.1.0')
  assert.ok(s.startsWith('## 0.1.0（2026-10-12）'))
  assert.ok(s.includes('#### 细节') && s.endsWith('- 不是下一个版本'), '### / #### 不算下一个版本')
  assert.ok(!s.includes('0.0.9'))
  assert.equal(changelogSection(md, '0.1.1'), null)
  assert.equal(changelogSection(md, '0.2.0'), null, '0.2.0 不匹配 0.2.0-beta.1')
  assert.ok(changelogSection(md, '0.2.0-beta.1').endsWith('- 预发布'))
  assert.equal(changelogSection(md, '0.0.9'), '## 0.0.9\n\n- 旧', '最后一段到文件末尾')
  // 取出的段落经 releaseBody / extractNotes 后，latest.json 的 notes 是去掉版本标题的正文
  const ctx = { version: '0.1.0', tag: 'v0.1.0', repo: 'yzlltyyh/wisp', updater: true, test: false }
  assert.ok(extractNotes(releaseBody({ ...ctx, changes: s })).startsWith('### 新功能'))

  // 仓库里的 CHANGELOG.md：每个版本段落都有列表项；不写 Markdown 链接与 HTML（应用「关于 → 更新内容」用
  // mdlite 渲染，链接会原样显示）；当前版本若是正式版，必须有对应段落（Release 说明与更新提示取自它）
  const real = readFileSync(join(ROOT, 'CHANGELOG.md'), 'utf8')
  const versions = [...real.matchAll(/^## (\d+\.\d+\.\d+\S*)/gm)].map((m) => m[1])
  assert.ok(versions.length > 0, 'CHANGELOG.md 至少有一个版本段落')
  for (const v of versions) {
    const sec = changelogSection(real, v)
    assert.ok(/^- /m.test(sec), `CHANGELOG ${v} 没有列表项`)
    assert.ok(!/\]\(/.test(sec), `CHANGELOG ${v} 不应包含 Markdown 链接（更新提示里会原样显示）`)
    assert.ok(
      !/<!--|<\/?(?:a|br|img|details|summary|div|span|p|sup|sub)\b/i.test(sec),
      `CHANGELOG ${v} 不应包含 HTML`,
    )
  }
  const pkg = JSON.parse(readFileSync(join(ROOT, 'package.json'), 'utf8'))
  if (!pkg.version.includes('-')) {
    assert.ok(
      changelogSection(real, pkg.version),
      `CHANGELOG.md 缺少当前版本 ${pkg.version} 的段落（发版前先写好，见 docs/development.md「发布流程」）`,
    )
  }
}

function testPlatforms() {
  // 推 tag（输入为空）与 all：三端齐全，顺序固定；矩阵的每一项都有 runner 与 Rust 目标
  assert.deepEqual(resolvePlatforms('', false), ['macos', 'windows', 'linux'])
  assert.deepEqual(resolvePlatforms(undefined, false), ['macos', 'windows', 'linux'])
  assert.deepEqual(resolvePlatforms('all', true), ['macos', 'windows', 'linux'])
  // 测试草稿可以只构建一个平台；正式 tag 不行；未知平台报错
  assert.deepEqual(resolvePlatforms('linux', true), ['linux'])
  assert.throws(() => resolvePlatforms('linux', false), /正式 tag 必须构建全部平台/)
  assert.throws(() => resolvePlatforms('freebsd', true), /未知平台/)
  assert.deepEqual(
    buildMatrix(['linux']).include.map((t) => t.target),
    ['x86_64-unknown-linux-gnu'],
  )
  assert.deepEqual(
    buildMatrix(resolvePlatforms('', false)).include.map((t) => `${t.runner}/${t.target}`),
    [
      'macos-15/aarch64-apple-darwin',
      'windows-latest/x86_64-pc-windows-msvc',
      'ubuntu-22.04/x86_64-unknown-linux-gnu',
    ],
    '只出 Apple Silicon、Windows x64、Linux x64，Linux 在 22.04 上构建（glibc 2.35）',
  )
  // 矩阵的平台名与产物清单一致（finalize 的 --platforms 按它核对缺失）
  assert.deepEqual(
    BUILD_TARGETS.map((t) => t.name).toSorted(),
    [...new Set(ARTIFACTS.map((a) => a.platform))].toSorted(),
  )
}

function testChangelogRange() {
  // 上一个正式版 tag：预发布 tag 不算（并入下一个正式版），本次的 tag 自身不算，按版本号而不是字典序
  const tags = ['v0.1.0', 'v0.2.0-beta.1', 'v0.10.0', 'v0.9.1', 'v0.11.0-rc.1']
  assert.equal(previousStableTag(tags, 'v0.11.0-rc.1'), 'v0.10.0')
  assert.equal(previousStableTag([...tags, 'v0.11.0'], 'v0.11.0'), 'v0.10.0')
  assert.equal(previousStableTag(['v0.1.0-beta.1'], 'v0.1.0'), null)
  assert.equal(previousStableTag([], 'v0.1.0-ci.3'), null)
}

/** 内存里的 GitHub Releases：只实现 upsertDraft 用到的接口；`onPost` 模拟另一个运行在本次新建前后抢先建了草稿 */
class FakeGitHub {
  constructor(releases = []) {
    this.releases = releases
    this.nextId = 100
    this.calls = []
    this.onPost = null
  }
  async releasesFor(tag) {
    return this.releases.filter((r) => r.tag_name === tag).toSorted((a, b) => a.id - b.id)
  }
  async assets(id) {
    return this.releases.find((r) => r.id === id)?.assets ?? []
  }
  async request(method, path, body) {
    this.calls.push(`${method} ${path}`)
    const id = Number(path.split('/').at(-1))
    if (method === 'POST') {
      const created = { id: this.nextId++, draft: true, assets: [], ...body }
      this.releases.push(created)
      this.onPost?.()
      return created
    }
    if (method === 'DELETE' && path.startsWith('/releases/assets/')) {
      for (const r of this.releases) r.assets = r.assets.filter((a) => a.id !== id)
      return null
    }
    if (method === 'DELETE') {
      this.releases = this.releases.filter((r) => r.id !== id)
      return null
    }
    const r = this.releases.find((x) => x.id === id)
    // GitHub 的行为（2026-10-13 实测）：草稿的 PATCH 不带 tag_name 时 tag 被重置成 untagged-<hash>
    if (method === 'PATCH' && r.draft && !('tag_name' in body)) r.tag_name = `untagged-${id}`
    Object.assign(r, body)
    return r
  }
}

async function testDraft() {
  const quiet = { ok() {}, warn() {}, info() {} }
  const args = {
    tag: 'v0.1.0',
    sha: 'b'.repeat(40),
    name: 'Wisp v0.1.0',
    body: 'x',
    prerelease: false,
  }
  const draft = (id, extra = {}) => ({
    id,
    tag_name: 'v0.1.0',
    draft: true,
    target_commitish: args.sha,
    assets: [],
    ...extra,
  })
  // 没有草稿：新建
  const empty = new FakeGitHub()
  assert.equal((await upsertDraft(empty, args, quiet)).id, 100)
  assert.equal(empty.releases.length, 1)
  // 已有草稿：复用；草稿原先指向别的提交时先删旧资源
  const stale = new FakeGitHub([
    draft(7, { target_commitish: 'a'.repeat(40), assets: [{ id: 1 }] }),
  ])
  const reused = await upsertDraft(stale, args, quiet)
  assert.equal(reused.id, 7)
  assert.equal(reused.target_commitish, args.sha)
  assert.equal(
    reused.tag_name,
    'v0.1.0',
    '复用草稿的 PATCH 必须带 tag_name，否则 GitHub 把它重置成 untagged',
  )
  // updateRelease 总是带 tag_name（finalize 写校验和也走它）；FakeGitHub 模拟了不带时被重置
  const patched = new FakeGitHub([draft(9)])
  await patched.request('PATCH', '/releases/9', { body: 'y' })
  assert.match(patched.releases[0].tag_name, /^untagged-/, 'FakeGitHub 模拟 GitHub 的重置')
  patched.releases[0].tag_name = 'v0.1.0'
  assert.equal((await updateRelease(patched, 9, 'v0.1.0', { body: 'z' })).tag_name, 'v0.1.0')
  assert.throws(() => updateRelease(patched, 9, '', { body: 'z' }), /tag/)
  assert.deepEqual(reused.assets, [])
  // 并发：本次新建的同时另一个运行建了更早的草稿 → 删掉自己建的，改用更早的那份，最终只剩一份
  const race = new FakeGitHub()
  race.onPost = () => race.releases.push(draft(50))
  const winner = await upsertDraft(race, args, quiet)
  assert.equal(winner.id, 50)
  assert.deepEqual(
    race.releases.map((r) => r.id),
    [50],
  )
  assert.ok(race.calls.includes('DELETE /releases/100'))
  // 历史遗留的重复草稿：用最早的一份，告警请人工处理，不删别的
  const warned = []
  const dup = new FakeGitHub([draft(9), draft(4)])
  const kept = await upsertDraft(dup, args, { ...quiet, warn: (m) => warned.push(m) })
  assert.equal(kept.id, 4)
  assert.equal(dup.releases.length, 2)
  assert.equal(warned.length, 1)
  // 同一提交重跑（Re-run all jobs）：说明重新生成，但 notes 标记之间人工改过的内容与已写入的校验和保留
  const ctx = { version: '0.1.0', tag: 'v0.1.0', repo: 'yzlltyyh/wisp', updater: true, test: false }
  const generated = releaseBody({ ...ctx, changes: '## 0.1.0\n\n- 原文' })
  const filled = replaceBlock(generated, CHECKSUMS_START, CHECKSUMS_END, '| a | `x` |')
  const edited = filled.replace('- 原文', '- 人工改过的措辞').replaceAll('\n', '\r\n')
  const fresh = releaseBody({ ...ctx, changes: '## 0.1.0\n\n- 原文', isPrivate: true })
  const merged = mergeDraftBody(edited, fresh)
  assert.ok(merged.keptNotes && merged.keptChecksums)
  assert.ok(merged.body.includes('- 人工改过的措辞') && !merged.body.includes('- 原文'))
  assert.ok(merged.body.includes('| a | `x` |') && !merged.body.includes('自动填入'))
  assert.ok(merged.body.includes('私有'), '标记之外的部分按本次的输入重新生成')
  assert.ok(!merged.body.includes('\r'), '统一成 LF')
  assert.equal(extractNotes(merged.body), '- 人工改过的措辞')
  // 没改过（只是 GitHub 网页把换行存成 CRLF）：不算改过；删掉了标记：按新生成的正文
  const untouched = mergeDraftBody(generated.replaceAll('\n', '\r\n'), fresh)
  assert.ok(!untouched.keptNotes && !untouched.keptChecksums)
  assert.equal(untouched.body, fresh)
  const stripped = mergeDraftBody(edited.replace(NOTES_START, '').replace(NOTES_END, ''), fresh)
  assert.ok(!stripped.keptNotes)
  assert.ok(stripped.body.includes('- 原文'))
  // upsertDraft：同一提交保留并告警；换了提交（删 tag 重打到新提交）整体换成新正文
  const warns = []
  const same = new FakeGitHub([draft(11, { body: edited })])
  const r1 = await upsertDraft(
    same,
    { ...args, body: fresh },
    { ...quiet, warn: (m) => warns.push(m) },
  )
  assert.ok(r1.body.includes('- 人工改过的措辞'))
  assert.equal(warns.length, 1)
  const moved = new FakeGitHub([
    draft(12, { body: edited, target_commitish: 'a'.repeat(40), assets: [{ id: 5 }] }),
  ])
  const r2 = await upsertDraft(moved, { ...args, body: fresh }, quiet)
  assert.equal(r2.body, fresh)
  assert.deepEqual(r2.assets, [])
  // 已经发布：拒绝
  await assert.rejects(
    upsertDraft(new FakeGitHub([draft(3, { draft: false })]), args, quiet),
    /已经发布/,
  )
}

const V = '0.1.0'
const ALL = [
  `Wisp_${V}_aarch64.dmg`,
  `Wisp_${V}_aarch64.app.tar.gz`,
  `Wisp_${V}_aarch64.app.tar.gz.sig`,
  `Wisp_${V}_x64-setup.exe`,
  `Wisp_${V}_x64-setup.exe.sig`,
  `Wisp_${V}_amd64.deb`,
  `Wisp_${V}_amd64.deb.sig`,
  `Wisp-${V}-1.x86_64.rpm`,
  `Wisp-${V}-1.x86_64.rpm.sig`,
  `Wisp_${V}_amd64.AppImage`,
  `Wisp_${V}_amd64.AppImage.sig`,
  'latest.json',
]

function testAssets() {
  // 文件名生成与名称模式、产品名一致（下载表与 finalize 的核对用的是同一份清单）
  const conf = JSON.parse(readFileSync(join(TAURI_DIR, 'tauri.conf.json'), 'utf8'))
  assert.equal(PRODUCT, conf.productName, 'PRODUCT 应与 tauri.conf.json 的 productName 一致')
  assert.deepEqual(
    ARTIFACTS.map((a) => artifactName(a.id, V)),
    ALL.filter((n) => n !== 'latest.json' && !n.endsWith('.sig')),
  )
  for (const a of ARTIFACTS) assert.ok(a.pattern.test(a.file(V)), `${a.id} 的文件名应匹配其模式`)
  const full = classifyAssets(ALL, V)
  assert.deepEqual(full.unexpected, [])
  assert.deepEqual(missingArtifacts(full, true), [])
  // Linux 作业失败：缺三个产物
  const noLinux = classifyAssets(
    ALL.filter((n) => !/deb|rpm|AppImage/.test(n)),
    V,
  )
  assert.equal(missingArtifacts(noLinux, true).length, 3)
  assert.deepEqual(missingArtifacts(noLinux, true, ['macos', 'windows']), [])
  // 缺签名
  const noSig = classifyAssets(
    ALL.filter((n) => !n.endsWith('.sig')),
    V,
  )
  assert.equal(missingArtifacts(noSig, true).length, 5)
  assert.deepEqual(missingArtifacts(noSig, false), [])
  // 没有私钥时 macOS 只有 dmg（app.tar.gz 是更新器产物）
  const installersOnly = classifyAssets(
    ALL.filter((n) => !n.endsWith('.sig') && !n.includes('.app.tar.gz')),
    V,
  )
  assert.deepEqual(missingArtifacts(installersOnly, false), [])
  assert.ok(missingArtifacts(installersOnly, true).includes('macos app'))
  // 多余的 MSI、universal、旧版本
  const extra = classifyAssets(
    [...ALL, `Wisp_${V}_x64_en-US.msi`, `Wisp_${V}_universal.dmg`, 'Wisp_0.0.9_aarch64.dmg'],
    V,
  )
  assert.equal(extra.unexpected.length, 3)
  // dmg 不应该有 .sig
  assert.deepEqual(classifyAssets([`Wisp_${V}_aarch64.dmg.sig`], V).unexpected, [
    `Wisp_${V}_aarch64.dmg.sig`,
  ])
}

function testUpdaterConfig() {
  // 仓库里的配置必须是正式的那一份（prepare 之前、CI 的 test 作业里都会跑到这里）
  assertOfficialUpdaterConfig()
  assert.equal(configuredPublicKey().keyId, OFFICIAL_UPDATER_KEY_ID)
  const read = (name) => JSON.parse(readFileSync(join(TAURI_DIR, name), 'utf8'))
  const base = { conf: read('tauri.conf.json'), noUpdaterConf: read('tauri.no-updater.conf.json') }
  assert.deepEqual(checkUpdaterConfig(base), [])
  const updater = base.conf.plugins.updater
  const variant = (patch, extra = {}) =>
    checkUpdaterConfig({
      ...base,
      conf: {
        ...base.conf,
        bundle: { ...base.conf.bundle, ...extra.bundle },
        plugins: { ...base.conf.plugins, updater: { ...updater, ...patch } },
      },
      ...extra.top,
    })
  // 本机演练的覆盖误写进配置：端点指向本机、带 dangerousInsecureTransportProtocol
  assert.equal(variant({ endpoints: ['http://127.0.0.1:8765/latest.json'] }).length, 1)
  assert.equal(
    variant({ endpoints: [OFFICIAL_UPDATER_ENDPOINT, 'https://example.com/latest.json'] }).length,
    1,
  )
  assert.match(
    variant({ dangerousInsecureTransportProtocol: true })[0],
    /dangerousInsecureTransportProtocol/,
  )
  // 换了公钥（例如同时换掉 Secrets 里的私钥，流水线照样能校验通过）
  assert.match(
    variant({ pubkey: keypair().pub })[0],
    new RegExp(`不是正式公钥 ${OFFICIAL_UPDATER_KEY_ID}`),
  )
  assert.match(variant({ pubkey: 'bm90IGEga2V5' })[0], /无法解析/)
  assert.match(
    variant({}, { bundle: { createUpdaterArtifacts: false } })[0],
    /createUpdaterArtifacts/,
  )
  // no-updater 配置只能关更新器产物；平台配置不能覆盖更新器
  assert.equal(
    variant(
      {},
      { top: { noUpdaterConf: { bundle: { createUpdaterArtifacts: false }, plugins: {} } } },
    ).length,
    1,
  )
  assert.equal(
    variant(
      {},
      { top: { platformConfs: { 'tauri.linux.conf.json': { plugins: { updater: {} } } } } },
    ).length,
    1,
  )
  // 本机更新演练的钩子只能经 --config 叠加，不能写进正式配置
  assert.match(
    checkUpdaterConfig({
      ...base,
      conf: {
        ...base.conf,
        plugins: { ...base.conf.plugins, wispUpdaterRehearsal: { enabled: true } },
      },
    })[0],
    /wispUpdaterRehearsal/,
  )
  // release.yml 里传给 tauri 的 --config 只有 no-updater 这一个（不能在工作流里覆盖端点或公钥）
  const yml = readFileSync(join(ROOT, '.github', 'workflows', 'release.yml'), 'utf8')
  // 注释里路径后面紧跟中文括号；行内 JSON（`--config '{"plugins":…}'`）会取到 `{`，同样判失败
  const configs = [...yml.matchAll(/--config\s+["']?([^\s"'（]+)/g)].map((m) => m[1])
  assert.ok(configs.length > 0)
  for (const c of configs)
    assert.equal(c, 'src-tauri/tauri.no-updater.conf.json', `release.yml 的 --config ${c}`)
}

function testUploads() {
  const dir = '/r/target/aarch64-apple-darwin/release/bundle'
  const mac = [
    `${dir}/dmg/Wisp_${V}_aarch64.dmg`,
    `${dir}/dmg/bundle_dmg.sh`,
    `${dir}/macos/Wisp.app.tar.gz`,
    `${dir}/macos/Wisp.app.tar.gz.sig`,
  ]
  // 有私钥：dmg + 改名后的更新包与签名（名字与 ARTIFACTS、finalize 的核对一致）
  const full = planUploads(mac, { platform: 'macos', version: V, updater: true })
  assert.deepEqual(full.missing, [])
  assert.deepEqual(full.problems, [])
  assert.deepEqual(
    full.uploads.map((u) => `${u.name} ← ${u.path.slice(dir.length + 1)}`),
    [
      `Wisp_${V}_aarch64.dmg ← dmg/Wisp_${V}_aarch64.dmg`,
      `Wisp_${V}_aarch64.app.tar.gz ← macos/Wisp.app.tar.gz`,
      `Wisp_${V}_aarch64.app.tar.gz.sig ← macos/Wisp.app.tar.gz.sig`,
    ],
  )
  for (const u of full.uploads) assert.deepEqual(classifyAssets([u.name], V).unexpected, [])
  // 没有私钥：只有 dmg，不传 .sig；有私钥却缺 .sig / 更新包：记为缺失、一个都不传
  assert.deepEqual(
    planUploads([mac[0]], { platform: 'macos', version: V, updater: false }).uploads.map(
      (u) => u.name,
    ),
    [`Wisp_${V}_aarch64.dmg`],
  )
  assert.deepEqual(
    planUploads(mac.slice(0, 3), { platform: 'macos', version: V, updater: true }).missing,
    [`Wisp_${V}_aarch64.app.tar.gz.sig`],
  )
  assert.deepEqual(
    planUploads([mac[0]], { platform: 'macos', version: V, updater: true }).missing,
    [`Wisp_${V}_aarch64.app.tar.gz`],
  )
  // Linux：三个安装包各带 .sig；MSI 等清单之外的文件、别的版本、别的平台都不上传
  const lx = '/r/target/x86_64-unknown-linux-gnu/release/bundle'
  const linux = [
    `${lx}/deb/Wisp_${V}_amd64.deb`,
    `${lx}/deb/Wisp_${V}_amd64.deb.sig`,
    `${lx}/rpm/Wisp-${V}-1.x86_64.rpm`,
    `${lx}/rpm/Wisp-${V}-1.x86_64.rpm.sig`,
    `${lx}/appimage/Wisp_${V}_amd64.AppImage`,
    `${lx}/appimage/Wisp_${V}_amd64.AppImage.sig`,
    `${lx}/appimage/Wisp_0.0.9_amd64.AppImage`,
    `${lx}/msi/Wisp_${V}_x64_en-US.msi`,
  ]
  const lp = planUploads(linux, { platform: 'linux', version: V, updater: true })
  assert.deepEqual(lp.missing, [])
  assert.equal(lp.uploads.length, 6)
  assert.ok(lp.uploads.every((u) => !u.name.includes('0.0.9') && !u.name.endsWith('.msi')))
  assert.deepEqual(planUploads(linux, { platform: 'windows', version: V, updater: true }).missing, [
    `Wisp_${V}_x64-setup.exe`,
  ])
  // 同一种产物匹配到两个文件：有歧义，不上传
  assert.equal(
    planUploads([...linux, `${lx}/other/Wisp_${V}_amd64.deb`], {
      platform: 'linux',
      version: V,
      updater: true,
    }).problems.length,
    1,
  )
}

/**
 * upload.mjs 的替换与重试，经 lib.mjs 的 GitHub 走真实的 fetch：本机回环上的假 GitHub 记录请求，草稿里已有一个
 * 同名旧资源，第一次上传返回 500
 */
async function testUploadHttp() {
  const calls = []
  let assets = [
    { id: 7, name: 'Wisp_0.1.0_amd64.deb' },
    { id: 8, name: 'keep.txt' },
  ]
  let failNext = true
  let received = null
  const server = createServer((req, res) => {
    const chunks = []
    req.on('data', (c) => chunks.push(c))
    req.on('end', () => {
      const url = new URL(req.url, 'http://x')
      calls.push(`${req.method} ${url.pathname}`)
      assert.equal(req.headers.authorization, 'Bearer t0ken')
      const json = (status, body) => {
        res.writeHead(status, { 'content-type': 'application/json' })
        res.end(body === undefined ? '' : JSON.stringify(body))
      }
      if (req.method === 'GET' && url.pathname === '/repos/o/r/releases/1/assets')
        return json(200, assets)
      if (req.method === 'DELETE' && url.pathname.startsWith('/repos/o/r/releases/assets/')) {
        const id = Number(url.pathname.split('/').at(-1))
        assets = assets.filter((a) => a.id !== id)
        res.writeHead(204)
        return res.end()
      }
      if (req.method === 'POST' && url.pathname === '/uploads/1') {
        if (failNext) {
          failNext = false
          // 模拟 GitHub 那边留下了一个半成品：重试前必须先删掉它
          assets.push({ id: 9, name: url.searchParams.get('name') })
          return json(500, { message: 'boom' })
        }
        received = { name: url.searchParams.get('name'), body: Buffer.concat(chunks) }
        const created = { id: 10, name: received.name, size: received.body.length }
        assets.push(created)
        return json(201, created)
      }
      return json(404, { message: 'not found' })
    })
  })
  await new Promise((r) => server.listen(0, '127.0.0.1', r))
  try {
    const base = `http://127.0.0.1:${server.address().port}`
    const gh = new GitHub('o/r', 't0ken')
    gh.api = base
    const release = { id: 1, upload_url: `${base}/uploads/1{?name,label}` }
    const bytes = randomBytes(64 * 1024)
    const logs = []
    const out = await replaceAsset(gh, release, 'Wisp_0.1.0_amd64.deb', bytes, {
      delay: 1,
      log: (m) => logs.push(m),
    })
    assert.equal(out.id, 10)
    assert.equal(received.name, 'Wisp_0.1.0_amd64.deb')
    assert.ok(received.body.equals(bytes), '上传的内容与文件一致')
    assert.deepEqual(
      assets.map((a) => a.id).toSorted((a, b) => a - b),
      [8, 10],
      '旧资源与失败留下的半成品都被删掉，别的资源不动',
    )
    assert.equal(logs.length, 1)
    assert.deepEqual(calls, [
      'GET /repos/o/r/releases/1/assets',
      'DELETE /repos/o/r/releases/assets/7',
      'POST /uploads/1',
      'GET /repos/o/r/releases/1/assets',
      'DELETE /repos/o/r/releases/assets/9',
      'POST /uploads/1',
    ])
  } finally {
    await new Promise((r) => server.close(r))
  }
}

function testLatestJson() {
  const entries = [
    { id: 'app', name: `Wisp_${V}_aarch64.app.tar.gz`, signature: 'sig-mac' },
    { id: 'nsis', name: `Wisp_${V}_x64-setup.exe`, signature: 'sig-win' },
    { id: 'appimage', name: `Wisp_${V}_amd64.AppImage`, signature: 'sig-ai' },
    { id: 'deb', name: `Wisp_${V}_amd64.deb`, signature: 'sig-deb' },
  ]
  const j = buildLatestJson({
    version: V,
    notes: 'n',
    pubDate: '2026-10-10T00:00:00.000Z',
    repo: 'yzlltyyh/wisp',
    tag: 'v0.1.0',
    entries,
  })
  assert.equal(j.version, V)
  assert.deepEqual(Object.keys(j.platforms), [
    'darwin-aarch64',
    'darwin-aarch64-app',
    'linux-x86_64',
    'linux-x86_64-appimage',
    'linux-x86_64-deb',
    'windows-x86_64',
    'windows-x86_64-nsis',
  ])
  assert.equal(j.platforms['windows-x86_64'].signature, 'sig-win')
  assert.equal(
    j.platforms['linux-x86_64'].url,
    `https://github.com/yzlltyyh/wisp/releases/download/v0.1.0/Wisp_${V}_amd64.AppImage`,
  )
}

function testMinisign() {
  const k = keypair()
  const pub = parsePublicKey(k.pub)
  assert.equal(pub.keyId, k.id)
  const data = randomBytes(256 * 1024)
  for (const alg of ['ED', 'Ed']) {
    const sig = signData(k, data, alg)
    assert.equal(parseSignature(sig).algorithm, alg)
    verifySignature(data, sig, pub)
    const bad = Buffer.from(data)
    bad[1000] ^= 1
    assert.throws(() => verifySignature(bad, sig, pub), /不匹配/)
  }
  const other = keypair()
  assert.throws(() => verifySignature(data, signData(other, data), pub), /key id/)
  assert.throws(() => parsePublicKey(b64('not minisign')), /minisign/)
}

await run(async () => {
  testUpdaterConfig()
  testMeta()
  testBody()
  testChangelogSection()
  testPlatforms()
  testChangelogRange()
  await testDraft()
  testAssets()
  testUploads()
  await testUploadHttp()
  testLatestJson()
  testMinisign()
  ok(
    'release 脚本自检通过（正式更新器配置、tag 判定、正文与更新说明、CHANGELOG 段落、构建平台、变更日志范围、草稿复用 / 并发去重 / 保留人工改过的说明、产物清单、上传清单与替换重试、latest.json、校验和、minisign）',
  )
})
