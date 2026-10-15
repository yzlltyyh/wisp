/**
 * scripts/release/lib.mjs：发布脚本共用的 GitHub API、minisign 校验、
 * 产物清单与 latest.json 生成。只依赖 Node 内置模块（Node 24 自带 fetch）。
 */
/* oxlint-disable no-await-in-loop -- 分页与重试必须串行 */
import { createHash, createPublicKey, verify } from 'node:crypto'
import { existsSync, readFileSync } from 'node:fs'
import { basename, join } from 'node:path'
import { TAURI_DIR } from '../lib/common.mjs'

// ---------------------------------------------------------------------------
// GitHub REST（GITHUB_TOKEN 由 Actions 提供；本机可用 `gh auth token`）

export class GitHub {
  constructor(repo = process.env.GITHUB_REPOSITORY, token = process.env.GITHUB_TOKEN) {
    if (!repo) throw new Error('缺少 GITHUB_REPOSITORY（owner/repo）')
    if (!token) throw new Error('缺少 GITHUB_TOKEN')
    this.repo = repo
    this.token = token
    this.api = process.env.GITHUB_API_URL ?? 'https://api.github.com'
  }

  headers(extra = {}) {
    return {
      authorization: `Bearer ${this.token}`,
      accept: 'application/vnd.github+json',
      'x-github-api-version': '2022-11-28',
      'user-agent': 'wisp-release-scripts',
      ...extra,
    }
  }

  async request(method, path, body, { raw = false, accept } = {}) {
    const url = path.startsWith('https://') ? path : `${this.api}/repos/${this.repo}${path}`
    const init = { method, headers: this.headers(accept ? { accept } : {}) }
    if (body !== undefined) {
      init.headers['content-type'] = 'application/json'
      init.body = JSON.stringify(body)
    }
    for (let attempt = 1; ; attempt++) {
      const res = await fetch(url, init)
      // 5xx 与限流重试三次（指数退避）；其他错误直接抛出
      if ((res.status >= 500 || res.status === 429) && attempt < 4) {
        await new Promise((r) => setTimeout(r, 1000 * 2 ** attempt))
        continue
      }
      if (!res.ok) {
        const text = await res.text().catch(() => '')
        throw new Error(`GitHub API ${method} ${path} → ${res.status}：${text.slice(0, 300)}`)
      }
      if (raw) return new Uint8Array(await res.arrayBuffer())
      return res.status === 204 ? null : res.json()
    }
  }

  /**
   * 该 tag 的全部 Release（含草稿：/releases/tags/{tag} 不返回草稿，只能翻列表），按 id 升序（先建的在前）。
   * 正常情况下最多一个；并发的运行各自建草稿时会有多个同名草稿（GitHub 允许）
   */
  async releasesFor(tag) {
    const hits = []
    for (let page = 1; page <= 10; page++) {
      const list = await this.request('GET', `/releases?per_page=100&page=${page}`)
      hits.push(...list.filter((r) => r.tag_name === tag))
      if (list.length < 100) break
    }
    return hits.toSorted((a, b) => a.id - b.id)
  }

  /** 按 tag 找 Release：已发布的优先，否则取最早建的草稿；没有时为 null */
  async findRelease(tag) {
    const all = await this.releasesFor(tag)
    return all.find((r) => !r.draft) ?? all[0] ?? null
  }

  async assets(releaseId) {
    const out = []
    for (let page = 1; page <= 10; page++) {
      const list = await this.request(
        'GET',
        `/releases/${releaseId}/assets?per_page=100&page=${page}`,
      )
      out.push(...list)
      if (list.length < 100) break
    }
    return out
  }

  download(assetId) {
    return this.request('GET', `/releases/assets/${assetId}`, undefined, {
      raw: true,
      accept: 'application/octet-stream',
    })
  }

  async upload(release, name, bytes, contentType) {
    const base = release.upload_url.replace(/\{.*$/, '')
    const url = `${base}?name=${encodeURIComponent(name)}`
    const res = await fetch(url, {
      method: 'POST',
      headers: this.headers({ 'content-type': contentType }),
      body: bytes,
    })
    if (!res.ok)
      throw new Error(`上传 ${name} 失败：${res.status} ${(await res.text()).slice(0, 300)}`)
    return res.json()
  }
}

/**
 * 更新一份 Release（PATCH）并**总是带上 tag_name**：GitHub 对草稿的 PATCH 不带 tag_name 时会把它重置成
 * `untagged-<hash>`（v0.1.0 的 finalize 只改正文，草稿随即丢了 tag；2026-10-13 用一次性草稿复现：建草稿时 v0.1.0 →
 * 只 PATCH body 之后 untagged-… → 带上 tag_name 的 PATCH 保持 v0.1.0）。丢了 tag 的草稿发布时会新建一个
 * untagged 的 tag，prepare 重跑时也按 tag 找不到它
 */
export function updateRelease(gh, id, tag, fields) {
  if (!tag)
    throw new Error(
      'updateRelease 需要 tag（GitHub 会把不带 tag_name 的草稿 PATCH 重置成 untagged）',
    )
  return gh.request('PATCH', `/releases/${id}`, { tag_name: tag, ...fields })
}

// ---------------------------------------------------------------------------
// minisign（Tauri 更新器签名）：公钥与 .sig 都是「minisign 文本」再整体 base64 一次

const ED25519_SPKI_PREFIX = Buffer.from('302a300506032b6570032100', 'hex')

function minisignLines(b64) {
  const text = Buffer.from(b64.trim(), 'base64').toString('utf8')
  if (!text.startsWith('untrusted comment:'))
    throw new Error('不是 minisign 格式（base64 解码后应以 untrusted comment: 开头）')
  return text.split('\n').map((l) => l.replace(/\r$/, ''))
}

/** 解析 tauri.conf.json 的 plugins.updater.pubkey → `{ keyId, key }` */
export function parsePublicKey(b64) {
  const lines = minisignLines(b64)
  const raw = Buffer.from(lines[1] ?? '', 'base64')
  if (raw.length !== 42 || raw.subarray(0, 2).toString() !== 'Ed')
    throw new Error('minisign 公钥格式不对')
  return {
    keyId: Buffer.from(raw.subarray(2, 10).toReversed()).toString('hex').toUpperCase(),
    key: createPublicKey({
      key: Buffer.concat([ED25519_SPKI_PREFIX, raw.subarray(10)]),
      format: 'der',
      type: 'spki',
    }),
  }
}

/** 解析 .sig 文件内容 → `{ algorithm, keyId, signature, trustedComment, globalSignature }` */
export function parseSignature(b64) {
  const lines = minisignLines(b64)
  const raw = Buffer.from(lines[1] ?? '', 'base64')
  const algorithm = raw.subarray(0, 2).toString()
  if (raw.length !== 74 || !['Ed', 'ED'].includes(algorithm))
    throw new Error('minisign 签名格式不对')
  const tc = lines[2] ?? ''
  if (!tc.startsWith('trusted comment: ')) throw new Error('minisign 签名缺少 trusted comment')
  return {
    algorithm,
    keyId: Buffer.from(raw.subarray(2, 10).toReversed()).toString('hex').toUpperCase(),
    signature: raw.subarray(10),
    trustedComment: tc.slice('trusted comment: '.length),
    globalSignature: Buffer.from(lines[3] ?? '', 'base64'),
  }
}

/** 用公钥校验数据与 .sig；失败时抛出原因 */
export function verifySignature(data, sigB64, pub) {
  const sig = parseSignature(sigB64)
  if (sig.keyId !== pub.keyId) {
    throw new Error(
      `签名的 key id ${sig.keyId} 与配置的公钥 ${pub.keyId} 不一致（GitHub Secrets 里的私钥与 tauri.conf.json 的公钥不是一对）`,
    )
  }
  // 'ED' 为预哈希模式（BLAKE2b-512），Tauri 的签名器默认使用；'Ed' 为旧版直接签原文
  const message = sig.algorithm === 'ED' ? createHash('blake2b512').update(data).digest() : data
  if (!verify(null, message, pub.key, sig.signature)) throw new Error('签名与文件内容不匹配')
  const global = Buffer.concat([sig.signature, Buffer.from(sig.trustedComment, 'utf8')])
  if (!verify(null, global, pub.key, sig.globalSignature))
    throw new Error('trusted comment 的全局签名无效')
  return sig
}

/** 读取 tauri.conf.json 中的更新器公钥 */
export function configuredPublicKey() {
  const conf = JSON.parse(readFileSync(join(TAURI_DIR, 'tauri.conf.json'), 'utf8'))
  const b64 = conf.plugins?.updater?.pubkey
  if (!b64) throw new Error('tauri.conf.json 没有 plugins.updater.pubkey')
  return parsePublicKey(b64)
}

// ---------------------------------------------------------------------------
// 正式更新器配置的守卫：已安装的客户端只认这一个端点和这一把公钥。端点或公钥一旦被改掉——
// 例如把本机演练的 --config 覆盖误写进 tauri.conf.json 并提交，或者同时换掉公钥与 Secrets 里的私钥——流水线照样
// 能签名、校验、全绿，发出去的版本却会指向错误的地址，或者再也验证不了之后的更新。self-test（prepare 之前运行，
// CI 的 Linux test 作业也跑）与 prepare、finalize 都会核对。

/** 正式的更新清单地址：tauri.conf.json 的 plugins.updater.endpoints 必须恰好是这一项 */
export const OFFICIAL_UPDATER_ENDPOINT =
  'https://github.com/yzlltyyh/wisp/releases/latest/download/latest.json'
/** 正式更新器公钥的 key id（对应的私钥只在维护者手里与仓库的 GitHub Secrets 中） */
export const OFFICIAL_UPDATER_KEY_ID = '890CA83224492BCC'
/** Tauri 会按平台自动合并进 tauri.conf.json 的配置文件：它们不能覆盖更新器配置 */
const PLATFORM_CONFS = ['tauri.macos.conf.json', 'tauri.windows.conf.json', 'tauri.linux.conf.json']

/**
 * 核对更新器配置（纯函数，便于自检）：`conf` 为 tauri.conf.json，`noUpdaterConf` 为 tauri.no-updater.conf.json，
 * `platformConfs` 为存在的平台配置 `{ 文件名: 内容 }`。返回问题列表（空数组表示通过）
 */
export function checkUpdaterConfig({ conf, noUpdaterConf, platformConfs = {} }) {
  const problems = []
  const updater = conf?.plugins?.updater ?? {}
  const endpoints = updater.endpoints
  if (
    !Array.isArray(endpoints) ||
    endpoints.length !== 1 ||
    endpoints[0] !== OFFICIAL_UPDATER_ENDPOINT
  ) {
    problems.push(
      `plugins.updater.endpoints 必须恰好是 ["${OFFICIAL_UPDATER_ENDPOINT}"]，现在是 ${JSON.stringify(endpoints)}`,
    )
  }
  if ('dangerousInsecureTransportProtocol' in updater) {
    problems.push(
      'plugins.updater 里有 dangerousInsecureTransportProtocol（只能出现在本机演练的 --config 临时覆盖里）',
    )
  }
  try {
    const { keyId } = parsePublicKey(updater.pubkey ?? '')
    if (keyId !== OFFICIAL_UPDATER_KEY_ID) {
      problems.push(
        `plugins.updater.pubkey 的 key id 是 ${keyId}，不是正式公钥 ${OFFICIAL_UPDATER_KEY_ID}`,
      )
    }
  } catch (e) {
    problems.push(`plugins.updater.pubkey 无法解析（${e instanceof Error ? e.message : e}）`)
  }
  if (conf?.bundle?.createUpdaterArtifacts !== true) {
    problems.push(
      'bundle.createUpdaterArtifacts 必须为 true（只在 tauri.no-updater.conf.json 里关闭）',
    )
  }
  if (
    JSON.stringify(noUpdaterConf) !== JSON.stringify({ bundle: { createUpdaterArtifacts: false } })
  ) {
    problems.push(
      `tauri.no-updater.conf.json 只能是 {"bundle":{"createUpdaterArtifacts":false}}，现在是 ${JSON.stringify(noUpdaterConf)}`,
    )
  }
  // 本机更新演练的钩子（scripts/release/updater-rehearsal.*、src/updater/rehearsal.rs）只能经 --config 叠加
  if (conf?.plugins?.wispUpdaterRehearsal !== undefined) {
    problems.push(
      'tauri.conf.json 里有 plugins.wispUpdaterRehearsal（演练钩子只能经 --config 叠加）',
    )
  }
  for (const [name, c] of Object.entries(platformConfs)) {
    if (c?.plugins?.updater !== undefined) problems.push(`${name} 覆盖了 plugins.updater`)
    if (c?.plugins?.wispUpdaterRehearsal !== undefined)
      problems.push(`${name} 带了 plugins.wispUpdaterRehearsal`)
    if (c?.bundle?.createUpdaterArtifacts !== undefined)
      problems.push(`${name} 覆盖了 bundle.createUpdaterArtifacts`)
  }
  return problems
}

/** 读取 `dir`（默认 src-tauri）里的配置并核对；不通过时抛出，并提示不要更换公钥与端点 */
export function assertOfficialUpdaterConfig(dir = TAURI_DIR) {
  const read = (name) => JSON.parse(readFileSync(join(dir, name), 'utf8'))
  const platformConfs = {}
  for (const name of PLATFORM_CONFS) {
    if (existsSync(join(dir, name))) platformConfs[name] = read(name)
  }
  const problems = checkUpdaterConfig({
    conf: read('tauri.conf.json'),
    noUpdaterConf: read('tauri.no-updater.conf.json'),
    platformConfs,
  })
  if (problems.length) {
    throw new Error(
      `更新器配置不是正式的那一份：${problems.join('；')}。不要更换公钥或端点——已安装的客户端只认 ${OFFICIAL_UPDATER_ENDPOINT} 与 key id ${OFFICIAL_UPDATER_KEY_ID}；本机演练只用 --config 临时覆盖，不要写进配置文件`,
    )
  }
}

// ---------------------------------------------------------------------------
// 产物清单（只出 macOS arm64、Windows x64、Linux x64；文件名以 tauri-action v1 的实际输出为准）

/**
 * 发布矩阵（release.yml 的 build 作业经 prepare 的 `matrix` 输出取用）：只出 Apple Silicon、Windows x64、Linux x64，
 * 不出 universal / Intel / ARM64。Linux 用 ubuntu-22.04 构建，产物只需要 glibc 2.35
 */
export const BUILD_TARGETS = [
  { name: 'macos', runner: 'macos-15', target: 'aarch64-apple-darwin' },
  { name: 'windows', runner: 'windows-latest', target: 'x86_64-pc-windows-msvc' },
  { name: 'linux', runner: 'ubuntu-22.04', target: 'x86_64-unknown-linux-gnu' },
]

/**
 * 本次构建哪些平台：`input` 为手动触发时选的 `all` / `macos` / `windows` / `linux`（推 tag 时为空 = all）。
 * 只构建部分平台只允许用于测试草稿（演练降级分支时省 macOS / Windows 的分钟数）；正式 tag 必须三端齐全，否则草稿
 * 会缺产物、latest.json 缺平台。返回平台名数组（按 BUILD_TARGETS 的顺序）
 */
export function resolvePlatforms(input, test) {
  const value = (input ?? '').trim() || 'all'
  const all = BUILD_TARGETS.map((t) => t.name)
  if (value === 'all') return all
  if (!all.includes(value)) throw new Error(`未知平台 ${value}（可选 all、${all.join('、')}）`)
  if (!test) {
    throw new Error(
      `正式 tag 必须构建全部平台（选择了 ${value}）：只构建部分平台只用于测试草稿（tag 留空）；单个平台失败时用 Re-run failed jobs`,
    )
  }
  return [value]
}

/** GitHub Actions 的矩阵（`strategy.matrix: ${{ fromJSON(…) }}`） */
export function buildMatrix(platforms) {
  return { include: BUILD_TARGETS.filter((t) => platforms.includes(t.name)) }
}

/** 产品名（tauri.conf.json 的 productName，产物文件名的前缀） */
export const PRODUCT = 'Wisp'

/**
 * 每种产物：平台、名称模式、实际文件名（`file(版本)`，Release 正文的下载表用它；与 pattern 的一致性由自检保证）、
 * 是否为更新器产物（带 .sig）、在 latest.json 里的键
 */
export const ARTIFACTS = [
  {
    id: 'dmg',
    platform: 'macos',
    pattern: /_aarch64\.dmg$/,
    file: (v) => `${PRODUCT}_${v}_aarch64.dmg`,
    updater: false,
  },
  {
    id: 'app',
    platform: 'macos',
    pattern: /_aarch64\.app\.tar\.gz$/,
    file: (v) => `${PRODUCT}_${v}_aarch64.app.tar.gz`,
    updater: true,
    updaterOnly: true,
    keys: ['darwin-aarch64', 'darwin-aarch64-app'],
  },
  {
    id: 'nsis',
    platform: 'windows',
    pattern: /_x64-setup\.exe$/,
    file: (v) => `${PRODUCT}_${v}_x64-setup.exe`,
    updater: true,
    keys: ['windows-x86_64', 'windows-x86_64-nsis'],
  },
  {
    id: 'deb',
    platform: 'linux',
    pattern: /_amd64\.deb$/,
    file: (v) => `${PRODUCT}_${v}_amd64.deb`,
    updater: true,
    keys: ['linux-x86_64-deb'],
  },
  {
    id: 'rpm',
    platform: 'linux',
    pattern: /\.x86_64\.rpm$/,
    file: (v) => `${PRODUCT}-${v}-1.x86_64.rpm`,
    updater: true,
    keys: ['linux-x86_64-rpm'],
  },
  {
    id: 'appimage',
    platform: 'linux',
    pattern: /_amd64\.AppImage$/,
    file: (v) => `${PRODUCT}_${v}_amd64.AppImage`,
    updater: true,
    keys: ['linux-x86_64', 'linux-x86_64-appimage'],
  },
]

/** 某种产物在该版本下的文件名 */
export function artifactName(id, version) {
  const a = ARTIFACTS.find((x) => x.id === id)
  if (!a) throw new Error(`未知产物 ${id}`)
  return a.file(version)
}

/** Release 资源的下载地址（草稿发布后生效） */
export function downloadUrl(repo, tag, name) {
  return `https://github.com/${repo}/releases/download/${encodeURIComponent(tag)}/${encodeURIComponent(name)}`
}

/**
 * tauri-cli 输出时不带版本号与架构的产物：macOS 更新包是 `bundle/macos/Wisp.app.tar.gz`（及 .sig），上传时改成
 * ARTIFACTS 里的名字（与 tauri-action v1 的命名一致，已在真实 runner 的产物上核对）。其余安装包的文件名
 * 由 bundler 直接给出，原样上传
 */
const RAW_NAMES = { app: [`${PRODUCT}.app.tar.gz`] }

/**
 * 某个平台要上传到草稿的文件（纯函数，便于自检；upload.mjs 用）：`files` 为 bundle 目录下全部文件的路径，
 * `platform` 为 macos / windows / linux，`updater` 为本次是否生成了更新器签名。
 * 返回 `{ uploads: [{ name, path }], missing: string[], problems: string[] }`：安装包缺失、有私钥时更新器产物
 * 缺少 .sig 都记为缺失；同一种产物匹配到多个文件记为问题。没有私钥时不上传 .sig（no-updater 配置本来也不会生成）
 */
export function planUploads(files, { platform, version, updater }) {
  const uploads = []
  const missing = []
  const problems = []
  const all = new Set(files)
  const byBase = new Map()
  for (const f of files) byBase.set(basename(f), [...(byBase.get(basename(f)) ?? []), f])
  for (const a of ARTIFACTS.filter((x) => x.platform === platform)) {
    // app.tar.gz 只是更新器产物：没有私钥（no-updater 配置）时不会生成
    if (a.updaterOnly && !updater) continue
    const name = a.file(version)
    const candidates = [name, ...(RAW_NAMES[a.id] ?? [])].flatMap((n) => byBase.get(n) ?? [])
    if (candidates.length === 0) {
      missing.push(name)
      continue
    }
    if (candidates.length > 1) {
      problems.push(`${name} 匹配到多个文件：${candidates.join('、')}`)
      continue
    }
    const [path] = candidates
    uploads.push({ name, path })
    if (updater && a.updater) {
      if (all.has(`${path}.sig`)) uploads.push({ name: `${name}.sig`, path: `${path}.sig` })
      else missing.push(`${name}.sig`)
    }
  }
  return { uploads, missing, problems }
}

/**
 * 把资源文件名分成：已知产物、签名、latest.json、意外文件。
 * 返回 `{ found: Map<id, name>, sigs: Map<id, name>, unexpected: string[], duplicates: string[] }`
 */
export function classifyAssets(names, version) {
  const found = new Map()
  const sigs = new Map()
  const unexpected = []
  const duplicates = []
  for (const name of names) {
    if (name === 'latest.json') continue
    const isSig = name.endsWith('.sig')
    const base = isSig ? name.slice(0, -4) : name
    const a = ARTIFACTS.find((x) => x.pattern.test(base))
    if (!a || !base.includes(version) || (isSig && !a.updater)) {
      unexpected.push(name)
      continue
    }
    const map = isSig ? sigs : found
    if (map.has(a.id)) duplicates.push(name)
    map.set(a.id, name)
  }
  return { found, sigs, unexpected, duplicates }
}

/** 期望但缺失的产物（`updater` 为真时还要求每个更新器产物都有 .sig） */
export function missingArtifacts(
  { found, sigs },
  updater,
  platforms = ['macos', 'windows', 'linux'],
) {
  const missing = []
  for (const a of ARTIFACTS) {
    if (!platforms.includes(a.platform)) continue
    // app.tar.gz 只是更新器产物：没有私钥（no-updater 配置）时不会生成
    if (a.updaterOnly && !updater) continue
    if (!found.has(a.id)) missing.push(`${a.platform} ${a.id}`)
    else if (updater && a.updater && !sigs.has(a.id)) missing.push(`${found.get(a.id)}.sig`)
  }
  return missing
}

/**
 * 从 Release 正文里取出更新说明（latest.json 的 notes，客户端的更新提示会显示它）：只要 prepare 写在
 * `<!-- wisp:notes:start -->` 与 `<!-- wisp:notes:end -->` 之间的 git-cliff 分组变更，去掉第一行的版本标题；
 * 测试草稿 / 缺私钥的提示、下载表格与首次打开说明都不进 notes。没有标记时返回 null（正文被手工改过）
 */
export function extractNotes(
  body,
  start = '<!-- wisp:notes:start -->',
  end = '<!-- wisp:notes:end -->',
) {
  const from = body.indexOf(start)
  const to = body.indexOf(end, from + start.length)
  if (from < 0 || to < 0) return null
  return body
    .slice(from + start.length, to)
    .trim()
    .replace(/^##\s[^\n]*\n*/, '')
    .trim()
}

/** 生成 latest.json（Tauri v2 更新器格式；同时写 `<os>-<arch>` 与 `<os>-<arch>-<installer>` 两种键） */
export function buildLatestJson({ version, notes, pubDate, repo, tag, entries }) {
  const platforms = {}
  for (const e of entries) {
    const a = ARTIFACTS.find((x) => x.id === e.id)
    const url = downloadUrl(repo, tag, e.name)
    for (const key of a.keys) platforms[key] = { signature: e.signature, url }
  }
  const sorted = Object.fromEntries(
    Object.entries(platforms).toSorted(([a], [b]) => (a < b ? -1 : 1)),
  )
  return { version, notes, pub_date: pubDate, platforms: sorted }
}

// ---------------------------------------------------------------------------
// 校验和（finalize 在核对产物后写进 Release 正文：安装包没有代码签名，用户可以对照 SHA-256 确认下载完整、未被替换）

/** Release 正文里包住校验和表的标记（prepare 写入占位，finalize 替换中间的内容） */
export const CHECKSUMS_START = '<!-- wisp:checksums:start -->'
export const CHECKSUMS_END = '<!-- wisp:checksums:end -->'

/**
 * 把 `body` 里 `start` 与 `end` 两个标记之间的内容换成 `content`（标记保留）；没有标记时在正文末尾追加一段
 * 带标记的 `content`（正文被手工改过）。返回新的正文
 */
export function replaceBlock(body, start, end, content) {
  const from = body.indexOf(start)
  const to = from < 0 ? -1 : body.indexOf(end, from + start.length)
  const block = `${start}\n${content}\n${end}`
  if (from < 0 || to < 0) return `${body.trimEnd()}\n\n${block}\n`
  return body.slice(0, from) + block + body.slice(to + end.length)
}

/** 校验和表（Markdown）：`rows` 为 `{ name, sha256 }`，按 ARTIFACTS 的顺序给出（安装包与更新包，不列 .sig 与 latest.json） */
export function checksumTable(rows) {
  if (rows.length === 0) return '_还没有可用的安装包。_'
  return [
    '| 文件 | SHA-256 |',
    '| --- | --- |',
    ...rows.map((r) => `| ${r.name} | \`${r.sha256}\` |`),
    '',
    'macOS / Linux：`shasum -a 256 <文件>`；Windows：`Get-FileHash <文件>`（PowerShell）。',
  ].join('\n')
}
