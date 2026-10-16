#!/usr/bin/env node
/**
 * scripts/release/prepare.mjs：release.yml 的 prepare 作业。
 *
 * 1. 版本与 tag：四处版本号必须一致（bump.mjs）；推 tag 触发时 tag 必须等于 `v<版本>`；手动触发时可指定 tag
 *    （同样必须等于 `v<版本>`），留空则为 CI 测试草稿 `v<版本>-ci.<运行号>`（不会创建 git tag，验证后删除）。
 *    tag 已经存在时必须指向本次构建的提交（GITHUB_SHA），否则产物与说明会和 tag 对不上；
 * 2. Release 说明：缺少更新器私钥时顶部加警告，仓库为私有时加提示（已安装的客户端读不到
 *    `releases/latest/download/latest.json`，自动更新不可用）；然后是本版本的变更——CHANGELOG.md 里有该版本的段落
 *    （`## <版本>`，发版前人工审定）时取它，否则由 git-cliff 生成（上一个正式版 tag 到 HEAD，预发布并入）；变更包在
 *    `<!-- wisp:notes -->` 标记里，finalize 只把这一段写进 latest.json 的 notes；再是下载表与系统要求、
 *    首次打开说明（未签名 / 未公证、macOS 权限、SmartScreen、Linux 会话）与文档链接；最后留出校验和的位置
 *    （`<!-- wisp:checksums -->`，finalize 核对产物后填入每个安装包的 SHA-256）；
 * 3. 草稿 Release：该 tag 已有草稿时复用（更新名称、正文与目标提交），已发布则拒绝；草稿原先指向别的提交时
 *    （删掉 tag 后重打同名 tag 等）先删除它的全部旧资源、正文整体换成新生成的。草稿就是本次提交的（Re-run all jobs、
 *    同一提交上重推 tag）时正文同样重新生成，但 `<!-- wisp:notes -->` 之间人工改过的内容与已经写入的校验和保留
 *    （mergeDraftBody；保留时在日志里告警）。否则新建草稿。各平台的 build 作业都上传到这一份草稿，单独重跑某个作业
 *    不会产生重复的 Release。同一 tag 的运行由 release.yml 的并发组排队；万一仍有两个运行同时新建（并发组
 *    之外的竞争），建完后再列一次，有更早的同名草稿就删掉自己建的这份、改用那份。
 *
 * 4. 构建矩阵：推 tag 时三端齐全；手动触发生成测试草稿时可以只选一个平台（演练降级分支时节省 runner 时间），正式 tag 不允许。
 * 开始之前先核对更新器配置是正式的那一份（lib.mjs 的 assertOfficialUpdaterConfig：端点、公钥 key id、没有
 * dangerousInsecureTransportProtocol、no-updater 配置只关更新器产物）。
 *
 * 输入（环境变量）：GITHUB_REPOSITORY、GITHUB_TOKEN、GITHUB_REF、GITHUB_SHA、GITHUB_RUN_NUMBER、INPUT_TAG、
 *   INPUT_PLATFORMS（all / macos / windows / linux）、HAS_UPDATER_KEY（"true" / "false"）、WITHOUT_UPDATER_KEY（"true" 时
 *   模拟缺少私钥）、HAS_MAC_P12（"true" 时 macOS 用固定自签证书签名）。
 * 输出（step outputs）：tag、version、prerelease、updater、test、private、platforms（逗号分隔）、matrix（JSON）、release-id。
 * 本机预览：node scripts/release/prepare.mjs --dry-run [--tag v0.1.0] [--private] [--platforms linux]（只打印，不访问 GitHub）。
 */
import { spawnSync } from 'node:child_process'
import { existsSync, readFileSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { tmpdir } from 'node:os'
import { parseArgs } from 'node:util'
import { SEMVER, currentVersion } from '../bump.mjs'
import { ROOT, appendSummary, info, isMain, ok, run, setOutput, warn } from '../lib/common.mjs'
import {
  CHECKSUMS_END,
  CHECKSUMS_START,
  GitHub,
  PRODUCT,
  artifactName,
  assertOfficialUpdaterConfig,
  buildMatrix,
  downloadUrl,
  replaceBlock,
  resolvePlatforms,
  updateRelease,
} from './lib.mjs'

export const GIT_CLIFF = 'git-cliff@2.14.2'

/** 计算 tag 等元信息（纯函数，便于自检） */
export function resolveMeta({
  version,
  ref = '',
  inputTag = '',
  runNumber = '0',
  hasKey,
  withoutKey,
}) {
  if (!version) throw new Error('各处版本号不一致：先执行 node scripts/bump.mjs <版本> 同步')
  if (!SEMVER.test(version)) throw new Error(`版本号 ${version} 不是合法的 SemVer`)
  let tag
  let test = false
  if (ref.startsWith('refs/tags/')) {
    tag = ref.slice('refs/tags/'.length)
    if (tag !== `v${version}`) {
      throw new Error(
        `tag ${tag} 与版本号 ${version} 不一致：先 node scripts/bump.mjs ${tag.replace(/^v/, '')} 并提交，再打 tag`,
      )
    }
  } else if (inputTag) {
    tag = inputTag.trim()
    if (tag !== `v${version}`)
      throw new Error(`指定的 tag ${tag} 与版本号 ${version} 不一致（应为 v${version}）`)
  } else {
    tag = `v${version}-ci.${runNumber}`
    test = true
  }
  return {
    tag,
    version,
    prerelease: tag.includes('-'),
    updater: hasKey === true && withoutKey !== true,
    test,
  }
}

/** 运行 git，返回去掉首尾空白的标准输出；失败时返回 null */
function git(args) {
  const r = spawnSync('git', args, { cwd: ROOT, encoding: 'utf8' })
  return r.status === 0 ? r.stdout.trim() : null
}

/** vX.Y.Z 的数值比较（只用于正式版 tag） */
function compareStable(a, b) {
  const pa = a.slice(1).split('.').map(Number)
  const pb = b.slice(1).split('.').map(Number)
  for (let i = 0; i < 3; i++) if (pa[i] !== pb[i]) return pa[i] - pb[i]
  return 0
}

/**
 * 上一个正式版 tag：`tags`（HEAD 的祖先上的 tag）中形如 vX.Y.Z、且不是本次 tag 的最大版本；没有时为 null。
 * 预发布 tag 不算——它们的变更并入下一个正式版（cliff.toml 的 ignore_tags）
 */
export function previousStableTag(tags, tag) {
  const stable = tags.filter((t) => /^v\d+\.\d+\.\d+$/.test(t) && t !== tag)
  return stable.toSorted(compareStable).at(-1) ?? null
}

/**
 * CHANGELOG.md 中某个版本的段落：从 `## <版本>`（后面可以跟日期）到下一个 `## ` 标题之前；没有时返回 null。
 * 版本号按原样匹配，`0.1.0` 不会匹配 `0.1.0-beta.1` 或 `0.1.01`
 */
export function changelogSection(markdown, version) {
  const escaped = version.replaceAll(/[.*+?^${}()|[\]\\]/g, '\\$&')
  const head = new RegExp(`^## v?${escaped}(?=[\\s（(]|$)`, 'm').exec(markdown)
  if (!head) return null
  const rest = markdown.slice(head.index)
  const next = rest.slice(3).search(/^## /m)
  return (next < 0 ? rest : rest.slice(0, next + 3)).trim()
}

/**
 * 本版本的变更（git-cliff）：上一个正式版 tag 到 HEAD 的提交，标题用本次的 tag。
 * 不用 --current / --latest：它们以最近的任意 tag（含被忽略的预发布 tag）为起点，正式版会丢掉预发布期间的变更，
 * 预发布 tag 本身又会被当成「未发布」
 */
function gitCliff(tag) {
  const tags = (git(['tag', '--merged', 'HEAD', '--list', 'v*']) ?? '').split('\n').filter(Boolean)
  const prev = previousStableTag(tags, tag)
  const args = ['--yes', GIT_CLIFF, '--config', join(ROOT, 'cliff.toml'), '--strip', 'header']
  args.push('--tag', tag)
  if (prev) args.push(`${prev}..HEAD`)
  info(`变更日志范围：${prev ? `${prev}..HEAD` : '全部历史（还没有正式版 tag）'}`)
  const r = spawnSync('npx', args, {
    cwd: ROOT,
    encoding: 'utf8',
    shell: process.platform === 'win32',
    env: { ...process.env, NO_COLOR: '1' },
  })
  if (r.error || r.status !== 0) {
    throw new Error(
      `git-cliff 失败：${r.error?.message ?? r.stderr.trim().split('\n').slice(-3).join(' ')}`,
    )
  }
  return r.stdout.trim()
}

/**
 * 本版本的变更：CHANGELOG.md 里有 `## <版本>` 段落（发版前由人审定、按功能概括）时用它，否则由 git-cliff 从提交
 * 生成。测试草稿（v0.1.0-ci.N）按版本号取同一段，正文与正式发布时一致；预发布（v0.2.0-beta.1）的版本号带后缀，
 * CHANGELOG 里通常没有对应段落，走 git-cliff
 */
function changelog(tag, version) {
  const file = join(ROOT, 'CHANGELOG.md')
  const section = existsSync(file) ? changelogSection(readFileSync(file, 'utf8'), version) : null
  if (section) {
    info(`变更说明取自 CHANGELOG.md 的「${section.split('\n')[0].replace(/^## /, '')}」`)
    return section
  }
  return gitCliff(tag)
}

/** Release 正文里包住变更日志的标记（finalize 只取这一段作为 latest.json 的 notes） */
export const NOTES_START = '<!-- wisp:notes:start -->'
export const NOTES_END = '<!-- wisp:notes:end -->'

/**
 * 组装 Release 正文。`version` 决定下载表里的实际文件名（与 lib.mjs 的 ARTIFACTS 同一来源），`repo` / `tag`
 * 决定下载链接（草稿发布后生效），`ref` 决定文档链接指向的版本（测试草稿没有 tag，用提交）。
 * `isPrivate`：仓库是私有的（已安装的客户端读不到 latest.json）；`macCert`：macOS 用固定的自签证书签名
 * （更新后 TCC 授权保持有效），否则是 ad-hoc（每个版本的 cdhash 都不同，更新后需要重新授权）
 */
export function releaseBody({
  changes,
  updater,
  test,
  version,
  tag,
  repo,
  ref = tag,
  isPrivate = false,
  macCert = false,
}) {
  const parts = []
  if (test) {
    parts.push(
      '> [!CAUTION]\n> 这是 CI 手动触发生成的**测试草稿**（不对应任何 git tag），只用于验证发布流水线，验证后请删除，不要发布。',
    )
  }
  if (!updater) {
    parts.push(
      '> [!WARNING]\n> 本次构建未配置 `TAURI_SIGNING_PRIVATE_KEY`：没有更新器签名与 `latest.json`，已安装的版本**不会**自动更新到此版本，请手动下载安装。',
    )
  }
  if (isPrivate) {
    parts.push(
      '> [!NOTE]\n> 仓库目前是私有的：下载链接需要用有访问权限的 GitHub 账号登录后才能打开；已安装的 Wisp 也读不到更新清单，**应用内的自动更新不可用**，新版本请回到 Release 页面手动下载安装（可以在「设置 → 通用 → 检查更新」中选「关闭」）。',
    )
  }
  // git-cliff 在没有面向用户的提交时只输出版本标题
  const notes = /^- /m.test(changes)
    ? changes
    : `${changes ? `${changes}\n\n` : ''}本版本没有面向用户的变更。`
  parts.push(`${NOTES_START}\n${notes}\n${NOTES_END}`)
  const link = (id) => {
    const name = artifactName(id, version)
    return `[${name}](${downloadUrl(repo, tag, name)})`
  }
  parts.push(
    [
      '### 下载',
      '',
      '| 平台 | 安装包 | 系统要求 |',
      '| --- | --- | --- |',
      `| macOS · Apple Silicon | ${link('dmg')} | macOS 14 Sonoma 或更新 |`,
      `| Windows · x64 | ${link('nsis')} | Windows 10 / 11；缺少 WebView2 运行时时安装程序会自动下载 |`,
      `| Linux x64 · Debian / Ubuntu | ${link('deb')} | Ubuntu 22.04 / 24.04（选 Ubuntu on Xorg）、Debian 12+，或其他提供 X11 会话的发行版 |`,
      `| Linux x64 · Fedora | ${link('rpm')} | Fedora；完整功能需要 X11 桌面（Xfce、MATE、Cinnamon 等）；RHEL 系需先启用 EPEL（未验证） |`,
      `| Linux x64 · 其他发行版 | ${link('appimage')} | glibc 2.35+、FUSE 2，免安装，\`chmod +x\` 后运行；完整功能同样需要 X11 会话 |`,
      '',
      '只提供以上三种平台架构：不提供 Intel Mac、Windows ARM64 与 universal 包。rpm 的依赖用的是 Fedora 的包名，openSUSE 等包名不同的发行版请用 AppImage。',
    ].join('\n'),
  )
  // 钥匙串：ad-hoc 时更新后第一次读 Key 要输入登录密码（本机更新演练 updater-rehearsal.sh 实测）；
  // 自签证书下是否还会询问没有实测，所以只承诺 TCC 授权保持有效
  const mac = macCert
    ? '使用固定的自签名证书签名，更新版本后已授予的这两项权限保持有效；从 ad-hoc 签名的旧版本（0.1.1 及更早）更新过来时需要最后一次重新授予（托盘会提示）。读取 API Key 时钥匙串仍可能要求输入登录密码，输入后选「始终允许」。'
    : '没有固定的签名身份，**每次更新版本后 macOS 都会把它当作新应用**：需要重新授予这两项权限（托盘会提示）；读取 API Key 时钥匙串会要求输入登录密码，然后选「始终允许」。'
  const docs = `https://github.com/${repo}/blob/${ref}`
  parts.push(
    [
      '### 首次打开',
      '',
      `- **macOS**：应用未经 Apple 公证。先把 ${PRODUCT} 拖进「应用程序」文件夹；第一次打开被拦下后，到「系统设置 → 隐私与安全性」底部点「仍要打开」，或在终端执行 \`xattr -dr com.apple.quarantine /Applications/${PRODUCT}.app\`。划词与自动粘贴需要「辅助功能」权限，截图 OCR 需要「屏幕录制」权限（授权后重新启动 ${PRODUCT}），首次启动的引导会带你完成。${mac}`,
      '- **Windows**：安装包未签名，SmartScreen 提示「已保护你的电脑」时点「更多信息 → 仍要运行」。安装到当前用户，不需要管理员权限。',
      '- **Linux**：**完整功能需要 X11 会话**——Ubuntu 22.04 / 24.04 在登录界面选「Ubuntu on Xorg」，或者使用 Xfce、MATE、Cinnamon 等仍以 X11 为默认会话的桌面（Xubuntu、Ubuntu MATE、Linux Mint 等）。Ubuntu 25.10 起（包括 26.04 LTS）与 Fedora 43 起的默认 GNOME 已经没有 Xorg 会话，Kubuntu 26.04 与 Fedora KDE 默认也只装 Wayland 会话：在 Wayland 下全局快捷键、取词、截图与剪贴板记录都不可用，主要只剩输入翻译（经托盘或 `--trigger` 打开）。GNOME 需要 AppIndicator 扩展才能显示托盘图标；保存 API Key 需要 Secret Service（GNOME Keyring 或 KWallet）。',
      '',
      `安装、权限与接入模型的完整说明见 [README](${docs}/README.md#安装)，日常使用见[使用说明](${docs}/docs/user-guide.md)。`,
    ].join('\n'),
  )
  parts.push(
    ['### SHA-256', '', CHECKSUMS_START, '_各平台构建完成后自动填入。_', CHECKSUMS_END].join('\n'),
  )
  return `${parts.join('\n\n')}\n`
}

/** `body` 里 `start` 与 `end` 之间的内容（去掉 replaceBlock 加的首尾换行、统一成 LF）；没有标记时返回 null */
export function blockContent(body, start, end) {
  const text = (body ?? '').replaceAll('\r\n', '\n')
  const from = text.indexOf(start)
  const to = from < 0 ? -1 : text.indexOf(end, from + start.length)
  if (from < 0 || to < 0) return null
  return text
    .slice(from + start.length, to)
    .replace(/^\n/, '')
    .replace(/\n$/, '')
}

/**
 * 复用同一提交的草稿时合并正文：`fresh` 是本次新生成的正文（警告、私有提示、下载表、首次打开说明都按本次的
 * 输入重新生成），`existing` 是草稿现在的正文。同一提交的 CHANGELOG 与 git-cliff 输出都不变，所以 notes 标记之间
 * 的内容与新生成的不同 = 有人在草稿页改过，保留改过的版本（`keptNotes`）；finalize 已经写入的校验和也保留
 * （`keptChecksums`；同一提交的资源没有被删除，finalize 重跑时会重写）。草稿里删掉了标记时按新生成的正文。
 * 返回 `{ body, keptNotes, keptChecksums }`
 */
export function mergeDraftBody(existing, fresh) {
  let body = fresh
  const oldNotes = blockContent(existing, NOTES_START, NOTES_END)
  const newNotes = blockContent(fresh, NOTES_START, NOTES_END)
  const keptNotes = oldNotes !== null && newNotes !== null && oldNotes !== newNotes
  if (keptNotes) body = replaceBlock(body, NOTES_START, NOTES_END, oldNotes)
  const oldSums = blockContent(existing, CHECKSUMS_START, CHECKSUMS_END)
  const newSums = blockContent(fresh, CHECKSUMS_START, CHECKSUMS_END)
  const keptChecksums = oldSums !== null && newSums !== null && oldSums !== newSums
  if (keptChecksums) body = replaceBlock(body, CHECKSUMS_START, CHECKSUMS_END, oldSums)
  return { body, keptNotes, keptChecksums }
}

/**
 * 复用或新建该 tag 的草稿，返回最终使用的 Release。已发布则拒绝；草稿原先指向别的提交时先删除它的全部旧资源、
 * 正文整体换成新生成的；草稿就是本次提交的时按 mergeDraftBody 保留人工改过的变更说明与已写入的校验和。
 * 新建之后再列一次：并发组之外的竞争让同一 tag 出现了更早的草稿时，删掉本次新建的这份、改用那份。
 * `gh` 需要 releasesFor / assets / request（lib.mjs 的 GitHub；自检传入内存实现），`log` 默认打印到控制台
 */
export async function upsertDraft(
  gh,
  { tag, sha, name, body, prerelease },
  log = { ok, warn, info },
) {
  const all = await gh.releasesFor(tag)
  const published = all.find((r) => !r.draft)
  if (published) throw new Error(`${tag} 的 Release 已经发布，不能覆盖；请递增版本号`)
  let existing = all[0] ?? null
  if (all.length > 1) {
    log.warn(
      `tag ${tag} 有 ${all.length} 份草稿（id ${all.map((r) => r.id).join('、')}）：本次使用最早的 ${existing.id}，其余请人工检查后删除`,
    )
  }
  if (!existing) {
    const created = await gh.request('POST', '/releases', {
      tag_name: tag,
      target_commitish: sha,
      name,
      body,
      draft: true,
      prerelease,
    })
    const first = (await gh.releasesFor(tag)).find((r) => r.draft)
    if (!first || first.id === created.id) {
      log.ok(`已创建草稿 ${name}（id ${created.id}）`)
      return created
    }
    await gh.request('DELETE', `/releases/${created.id}`)
    log.warn(
      `另一个运行同时为 ${tag} 建了更早的草稿（id ${first.id}）：已删除本次新建的草稿 ${created.id}，改用它`,
    )
    existing = first
  }
  // 草稿原先对应别的提交：旧资源来自那个提交，留着会被 finalize 当成本次的产物写进 latest.json
  let finalBody = body
  if (existing.target_commitish !== sha) {
    const stale = await gh.assets(existing.id)
    await Promise.all(stale.map((a) => gh.request('DELETE', `/releases/assets/${a.id}`)))
    log.info(
      `草稿原先指向 ${existing.target_commitish}，本次是 ${sha.slice(0, 12)}：已删除 ${stale.length} 个旧资源，说明按 CHANGELOG 重新生成`,
    )
  } else {
    const merged = mergeDraftBody(existing.body ?? '', body)
    finalBody = merged.body
    if (merged.keptNotes) {
      log.warn(
        '草稿说明里 <!-- wisp:notes --> 之间的内容被人工改过：保留改过的版本（其余部分重新生成）。要改回 CHANGELOG.md 的内容，先在草稿页删掉这一对标记再重跑',
      )
    }
  }
  const release = await updateRelease(gh, existing.id, tag, {
    name,
    body: finalBody,
    prerelease,
    target_commitish: sha,
  })
  log.ok(`复用已有草稿 ${name}（id ${release.id}）`)
  return release
}

async function main() {
  const { values } = parseArgs({
    options: {
      'dry-run': { type: 'boolean', default: false },
      tag: { type: 'string' },
      private: { type: 'boolean', default: false },
      platforms: { type: 'string' },
      help: { type: 'boolean', short: 'h', default: false },
    },
  })
  if (values.help) {
    console.log(
      '用法：node scripts/release/prepare.mjs [--dry-run] [--tag v<版本>] [--private] [--platforms all|macos|windows|linux]（CI 中由环境变量驱动）',
    )
    return
  }
  const dryRun = values['dry-run']
  const env = process.env
  // 端点、公钥不是正式的那一份时，在花 macOS / Windows 分钟数之前失败（self-test 也查，这里兜底）
  assertOfficialUpdaterConfig()
  const sha = dryRun ? git(['rev-parse', 'HEAD']) : env.GITHUB_SHA
  if (!sha) throw new Error('缺少 GITHUB_SHA（本次构建的提交）')
  const meta = resolveMeta({
    version: currentVersion(),
    ref: dryRun ? '' : (env.GITHUB_REF ?? ''),
    inputTag: values.tag ?? env.INPUT_TAG ?? '',
    runNumber: env.GITHUB_RUN_NUMBER ?? '0',
    hasKey: dryRun ? true : env.HAS_UPDATER_KEY === 'true',
    withoutKey: env.WITHOUT_UPDATER_KEY === 'true',
  })
  info(
    `版本 ${meta.version}，tag ${meta.tag}${meta.prerelease ? '（预发布）' : ''}${meta.test ? '，CI 测试草稿' : ''}`,
  )
  info(
    `更新器签名：${meta.updater ? '有私钥，生成 .sig 与 latest.json' : '没有私钥（或模拟缺少），只出安装包'}`,
  )
  const platforms = resolvePlatforms(values.platforms ?? env.INPUT_PLATFORMS, meta.test)
  info(`构建平台：${platforms.join('、')}`)
  // tag 已存在：必须指向本次构建的提交（手动触发时构建的是分支头，tag 可能指向别处）
  const tagCommit = git(['rev-parse', '-q', '--verify', `refs/tags/${meta.tag}^{commit}`])
  if (tagCommit && tagCommit !== sha) {
    throw new Error(
      `tag ${meta.tag} 指向 ${tagCommit.slice(0, 12)}，本次构建的是 ${sha.slice(0, 12)}：产物与说明会和 tag 对不上。请在 tag 所在的提交上触发（推 tag，或手动触发时选择该 tag 作为 ref）`,
    )
  }

  const gh = dryRun ? null : new GitHub()
  // 仓库可见性决定已安装的客户端能不能读到 releases/latest/download/latest.json（私有仓库要登录，更新器拿不到）
  const isPrivate = gh ? (await gh.request('GET', '')).private === true : values.private
  if (isPrivate) {
    warn(
      '仓库是私有的：已安装的客户端读不到 releases/latest/download/latest.json，应用内自动更新不可用（Release 说明顶部已注明）',
    )
  }
  const macCert = env.HAS_MAC_P12 === 'true'
  const body = releaseBody({
    changes: changelog(meta.tag, meta.version),
    updater: meta.updater,
    test: meta.test,
    version: meta.version,
    tag: meta.tag,
    repo: env.GITHUB_REPOSITORY ?? 'yzlltyyh/wisp',
    ref: meta.test ? sha : meta.tag,
    isPrivate,
    macCert,
  })
  const bodyFile = join(env.RUNNER_TEMP ?? tmpdir(), 'wisp-release-body.md')
  writeFileSync(bodyFile, body)
  appendSummary(`## Release 说明预览（${meta.tag}）\n\n${body}`)

  for (const [k, v] of Object.entries({
    tag: meta.tag,
    version: meta.version,
    prerelease: meta.prerelease,
    updater: meta.updater,
    test: meta.test,
    private: isPrivate,
    platforms: platforms.join(','),
    matrix: JSON.stringify(buildMatrix(platforms)),
    'body-file': bodyFile,
  })) {
    setOutput(k, v)
  }

  if (!gh) {
    console.log(`\n----- Release 正文（${bodyFile}）-----\n${body}`)
    return
  }

  const release = await upsertDraft(gh, {
    tag: meta.tag,
    sha,
    name: `${PRODUCT} ${meta.tag}`,
    body,
    prerelease: meta.prerelease,
  })
  setOutput('release-id', release.id)
  appendSummary(`草稿：${release.html_url}`)
}

if (isMain(import.meta.url)) await run(main)
