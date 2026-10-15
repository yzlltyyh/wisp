#!/usr/bin/env node
/**
 * scripts/release/updater-rehearsal.mjs：本机更新演练（scripts/release/updater-rehearsal.sh）的辅助命令。
 * 只依赖 Node 内置模块；minisign 校验与 latest.json 的生成复用 release 流水线的 lib.mjs（与 finalize 同一份代码）。
 *
 * 用法：node scripts/release/updater-rehearsal.mjs <命令> [选项]
 *   free-port                                   打印 127.0.0.1 上一个空闲的 TCP 端口
 *   overlay --port <n> --identifier <id> --out <文件> [--pubkey-file <测试公钥>]
 *                                               写演练构建叠加的 tauri 配置（本机 http 端点 + 演练钩子 + 单独的
 *                                               identifier），同时把 pubkey 换成一次性测试公钥（key id 必须与
 *                                               正式公钥不同；脚本总是给 --pubkey-file）。不给时不含 pubkey——只在
 *                                               自检里用（正式私钥只在 CI 里使用）
 *   verify --file <包> --sig <.sig> [--version <v>] [--pubkey-file <公钥>]
 *                                               用给定公钥（默认 tauri.conf.json 的正式公钥，与 finalize 相同）校验，
 *                                               并核对签名的 trusted comment 里记录的版本
 *   forge --in <包> --out <.sig> --version <v>   用一次性生成的密钥签名（格式与 tauri signer 相同）：「别的密钥签的包」
 *   tamper --in <包> --out <包>                  复制并翻转正中间的一个字节：「篡改过的包 + 真签名」
 *   manifest --tarball <文件名> --sig <.sig> --base-url <url> --version <v> --out <latest.json> [--notes <文本>]
 *                                               生成 latest.json（lib.mjs 的 buildLatestJson，下载地址换成本机服务器）
 *   serve --dir <目录> --port <n> --log <jsonl>  只读的本机 HTTP 服务器（只绑 127.0.0.1），每个请求记一行 JSON
 *   seed --home <隔离 HOME>                      写演练用的 settings.json / state.json（见 SEED_*）
 *   seed-history --home <隔离 HOME> --out <json> 往应用建好的 clipboard.db 写 1 条剪贴板记录与 1 条识别历史（应用必须已
 *                                               退出），把写入后种子记录的行数与摘要记进 <json>
 *   set-policy --home <隔离 HOME> --policy <notify|auto_install|off> [--reset-check]
 *                                               改更新策略；--reset-check 把 state.json 的 last_update_check_ms 清零，
 *                                               让下一次启动 30 s 的检查照常联网（应用必须已退出）
 *   snapshot --home <隔离 HOME> --out <json>     记录配置与数据目录的内容（JSON 解析后的值、数据库的表与行数、其他文件的
 *                                               sha256）
 *   compare --before <json> --after <json> --seed <json>
 *                                               核对更新前后的快照：只允许 ALLOWED_STATE_DRIFT 里的字段变化；两份快照
 *                                               都必须有演练设置、种子 state 与 seed-history 写入的种子记录
 *   --self-test                                 离线自检（不需要构建产物）
 */
/* oxlint-disable no-await-in-loop -- 自检里逐个请求本机服务器 */
import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { createHash, generateKeyPairSync, randomBytes, sign } from 'node:crypto'
import {
  appendFileSync,
  copyFileSync,
  createReadStream,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { createServer } from 'node:http'
import { createServer as createNetServer } from 'node:net'
import { tmpdir } from 'node:os'
import { basename, dirname, join, relative, resolve, sep } from 'node:path'
import { parseArgs } from 'node:util'
import { ROOT, TAURI_DIR, info, isMain, ok, run, walk } from '../lib/common.mjs'
import {
  buildLatestJson,
  configuredPublicKey,
  parsePublicKey,
  parseSignature,
  verifySignature,
} from './lib.mjs'

/** 演练构建打开钩子的插件配置项（与 Rust `updater::rehearsal::CONFIG_KEY` 一致，自检核对） */
export const REHEARSAL_CONFIG_KEY = 'wispUpdaterRehearsal'
/** 演练构建默认的 identifier：与正式版、性能测量（.perf / .perfc）都不同，不经单实例锁转发给正在运行的 Wisp */
export const REHEARSAL_IDENTIFIER = 'io.github.yzlltyyh.wisp.rehearsal'

/** macOS 上配置与数据目录（dirs::config_dir / data_local_dir 都是 Application Support，再显式拼 wisp） */
export function wispDir(home) {
  return join(home, 'Library', 'Application Support', 'wisp')
}

// ---------------------------------------------------------------------------
// 叠加配置

/**
 * 演练构建叠加的 tauri 配置（JSON Merge Patch：`plugins.updater` 只替换 endpoints、加上 http 许可，windows 段保留正式值）。
 * `testPubkey`：测试密钥模式下的一次性测试公钥，替换 pubkey（演练构建只认测试密钥签的包，本机不产生任何正式签名的包）；
 * 不给时 pubkey 保留正式值（只在自检里用：脚本总是给测试公钥）
 */
export function rehearsalOverlay({ port, identifier = REHEARSAL_IDENTIFIER, testPubkey }) {
  if (!Number.isInteger(port) || port <= 0 || port > 65535) throw new Error(`端口不对：${port}`)
  if (!/^[A-Za-z0-9.-]+$/.test(identifier) || identifier === 'io.github.yzlltyyh.wisp')
    throw new Error(`identifier 必须与正式版不同：${identifier}`)
  return {
    identifier,
    plugins: {
      updater: {
        endpoints: [`http://127.0.0.1:${port}/latest.json`],
        dangerousInsecureTransportProtocol: true,
        ...(testPubkey ? { pubkey: testPubkey } : {}),
      },
      [REHEARSAL_CONFIG_KEY]: { enabled: true },
    },
  }
}

/**
 * 叠加配置不能削弱签名校验：不得放开降级或证书校验；pubkey 只能是调用方给出的一次性测试公钥，且 key id 不能是正式公钥的
 * （测试密钥模式）——叠加配置永远不会把正式公钥换成别的「正式」密钥
 */
export function assertOverlaySafe(
  overlay,
  { testPubkey, officialKeyId = configuredPublicKey().keyId } = {},
) {
  const updater = overlay?.plugins?.updater ?? {}
  for (const key of [
    'allowDowngrades',
    'dangerousAcceptInvalidCerts',
    'dangerousAcceptInvalidHostnames',
  ]) {
    if (key in updater) throw new Error(`叠加配置不能设置 plugins.updater.${key}`)
  }
  if ('pubkey' in updater) {
    if (!testPubkey || updater.pubkey !== testPubkey)
      throw new Error(
        '叠加配置不能设置 plugins.updater.pubkey（测试密钥模式只能是给出的一次性测试公钥）',
      )
    if (parsePublicKey(testPubkey).keyId === officialKeyId)
      throw new Error(`测试公钥的 key id 与正式公钥 ${officialKeyId} 相同：不是一次性测试密钥`)
  }
  for (const url of updater.endpoints ?? []) {
    const u = new URL(url)
    if (u.protocol !== 'https:' && u.hostname !== '127.0.0.1')
      throw new Error(`演练的 http 端点只能是 127.0.0.1：${url}`)
  }
}

// ---------------------------------------------------------------------------
// 签名

const b64 = (s) => Buffer.from(s).toString('base64')

/** 签名的 trusted comment 里记录的版本（tauri-cli 写成 `…\tversion:x.y.z`；没有时为 null） */
export function signedVersion(trustedComment) {
  for (const field of trustedComment.split('\t')) {
    if (field.startsWith('version:')) return field.slice('version:'.length)
  }
  return null
}

/** 用正式公钥（或传入的公钥）校验，返回 `{ keyId, trustedComment, version }` */
export function verifyFile(data, sigText, { version, pub = configuredPublicKey() } = {}) {
  const sig = verifySignature(data, sigText.trim(), pub)
  const signed = signedVersion(sig.trustedComment)
  if (version !== undefined && signed !== version)
    throw new Error(`签名记录的版本是 ${signed ?? '（没有）'}，不是 ${version}`)
  return { keyId: sig.keyId, trustedComment: sig.trustedComment, version: signed }
}

/** 一次性的 minisign 密钥对（与 tauri signer 相同的「文本再 base64」形式） */
export function throwawayKey() {
  const { publicKey, privateKey } = generateKeyPairSync('ed25519')
  const raw = publicKey.export({ format: 'der', type: 'spki' }).subarray(-32)
  const keyNum = randomBytes(8)
  const id = Buffer.from(keyNum.toReversed()).toString('hex').toUpperCase()
  const pub = b64(
    `untrusted comment: minisign public key: ${id}\n${b64(Buffer.concat([Buffer.from('Ed'), keyNum, raw]))}\n`,
  )
  return { pub, privateKey, keyNum, id }
}

/** 用一次性密钥签名（预哈希 ED，trusted comment 与 tauri-cli 的格式一致，带版本） */
export function forgeSignature(data, { key = throwawayKey(), file, version, now = Date.now() }) {
  const sig = sign(null, createHash('blake2b512').update(data).digest(), key.privateKey)
  const tc = `timestamp:${Math.floor(now / 1000)}\tfile:${file}\tversion:${version}`
  const global = sign(null, Buffer.concat([sig, Buffer.from(tc)]), key.privateKey)
  const text = `untrusted comment: signature from tauri secret key\n${b64(Buffer.concat([Buffer.from('ED'), key.keyNum, sig]))}\ntrusted comment: ${tc}\n${b64(global)}\n`
  return { sig: b64(text), key }
}

/** 复制一份并翻转正中间的一个字节（返回被改的偏移） */
export function tamper(src, dst) {
  const data = readFileSync(src)
  if (data.length < 2) throw new Error(`${src} 太小`)
  const at = Math.floor(data.length / 2)
  data[at] ^= 0xff
  writeFileSync(dst, data)
  return at
}

// ---------------------------------------------------------------------------
// latest.json

/** latest.json：与 finalize 同一个生成函数，下载地址换成本机服务器 */
export function rehearsalManifest({ version, notes, tarball, signature, baseUrl, pubDate }) {
  const latest = buildLatestJson({
    version,
    notes,
    pubDate: pubDate ?? new Date().toISOString(),
    repo: 'yzlltyyh/wisp',
    tag: `v${version}`,
    entries: [{ id: 'app', name: basename(tarball), signature: signature.trim() }],
  })
  const url = new URL(tarball, baseUrl.endsWith('/') ? baseUrl : `${baseUrl}/`).href
  for (const p of Object.values(latest.platforms)) p.url = url
  return latest
}

// ---------------------------------------------------------------------------
// 本机 HTTP 服务器

const TYPES = { '.json': 'application/json', '.sig': 'text/plain; charset=utf-8' }

/** 启动只读的静态文件服务器（只绑 127.0.0.1）。返回 `{ server, port }` */
export function startServer({ dir, port = 0, log }) {
  const root = resolve(dir)
  const record = (entry) => {
    if (log) appendFileSync(log, `${JSON.stringify({ t: new Date().toISOString(), ...entry })}\n`)
  }
  const server = createServer((req, res) => {
    const method = req.method ?? 'GET'
    let path = '/'
    try {
      path = decodeURIComponent(new URL(req.url ?? '/', 'http://127.0.0.1').pathname)
    } catch {
      path = '\0'
    }
    const ua = req.headers['user-agent'] ?? ''
    const file = resolve(root, `.${path}`)
    const inside = file.startsWith(root + sep)
    let size = -1
    if (inside && !path.includes('\0')) {
      try {
        const st = statSync(file)
        if (st.isFile()) size = st.size
      } catch {
        size = -1
      }
    }
    if (!['GET', 'HEAD'].includes(method) || size < 0) {
      const status = ['GET', 'HEAD'].includes(method) ? 404 : 405
      res.writeHead(status, { 'content-type': 'text/plain', 'cache-control': 'no-store' })
      res.end(status === 404 ? 'not found\n' : 'method not allowed\n')
      record({ method, path, status, bytes: 0, ua })
      return
    }
    const ext = file.slice(file.lastIndexOf('.'))
    res.writeHead(200, {
      'content-type': TYPES[ext] ?? 'application/octet-stream',
      'content-length': String(size),
      'cache-control': 'no-store',
    })
    record({ method, path, status: 200, bytes: method === 'HEAD' ? 0 : size, ua })
    if (method === 'HEAD') res.end()
    else createReadStream(file).pipe(res)
  })
  return new Promise((ok_, fail) => {
    server.once('error', fail)
    server.listen(port, '127.0.0.1', () => ok_({ server, port: server.address().port }))
  })
}

/** 127.0.0.1 上一个空闲端口 */
export function freePort() {
  return new Promise((ok_, fail) => {
    const s = createNetServer()
    s.once('error', fail)
    s.listen(0, '127.0.0.1', () => {
      const { port } = s.address()
      s.close(() => ok_(port))
    })
  })
}

// ---------------------------------------------------------------------------
// 隔离 HOME 的设置与数据

/** 五个全局动作（与 wisp-types `HotkeyAction::as_str` 一致，自检核对） */
export const HOTKEY_ACTIONS = [
  'translate_selection',
  'translate_input',
  'ocr',
  'ocr_translate',
  'clipboard',
]
/** 不绑定的快捷键（wisp-types `Binding::NONE`，自检核对） */
export const BINDING_NONE = 'none'

/**
 * 演练用的 settings.json：更新策略按场景切换；不写登录项、静默启动；日志 debug（脚本据此核对「启动 30 s 的检查被跳过」
 * 等 debug 行）；外观与语言用非默认值，更新后核对仍在。
 *
 * 隔离 HOME 管不到的系统级资源一律关掉：**剪贴板监听**（不把用户复制的内容记进工作目录）、**五个全局快捷键都不绑定**
 * （不和正在运行的 Wisp 抢 ⌥D / ⌥S 等组合键；按 ⌥S 还会以演练 identifier 触发屏幕录制的 TCC 请求——TCC 按代码签名
 * 记账，不看 HOME）、**扩展触发**（浮标、双击修饰键、鼠标按键）保持关闭
 */
export const SEED_SETTINGS = {
  schema_version: 1,
  general: {
    launch_at_login: false,
    start_hidden: true,
    ui_language: 'en-US',
    update_policy: 'notify',
  },
  hotkeys: {
    bindings: Object.fromEntries(HOTKEY_ACTIONS.map((a) => [a, BINDING_NONE])),
    bobber: { enabled: false },
    double_tap: { enabled: false },
    mouse: { enabled: false },
  },
  clipboard: { enabled: false },
  appearance: { theme: 'dark', accent: '#e11d48', font_size: 16 },
  advanced: { log_level: 'debug' },
}

/** 更新后 settings.json 必须保留的演练设置：SEED_SETTINGS 去掉按场景切换的 `general.update_policy` */
export function expectedSettings() {
  const s = structuredClone(SEED_SETTINGS)
  delete s.general.update_policy
  return s
}

/** 演练用的 state.json：引导已完成（不弹引导窗口），带几项非默认值，更新后核对仍在 */
export const SEED_STATE = {
  schema_version: 1,
  onboarding_done: true,
  onboarding_version: 1,
  pinned_on_top_default: false,
  usage: {
    '2026-10-01': {
      'wpr-rehearsal/demo-model': { requests: 3, input_tokens: 120, output_tokens: 45 },
    },
  },
}

/**
 * 更新前后允许变化的 state.json 字段：检查时间、清理时间、窗口几何与面板位置（启动 / 检查本身就会写）；
 * `permissions_seen_version` 是判定「更新后授权失效」的参照版本——授权没有失效时每次查询都记成当前版本，
 * 版本号一变它就跟着变（例如 0.1.0 → 0.1.1），这是设计如此
 */
export const ALLOWED_STATE_DRIFT = [
  'last_update_check_ms',
  'last_vacuum_ms',
  'settings_window',
  'clipboard_positions',
  'permissions_seen_version',
]

/** 更新后 state.json 必须保留的种子值：SEED_STATE 去掉 ALLOWED_STATE_DRIFT 里的字段 */
export function expectedState() {
  return Object.fromEntries(
    Object.entries(SEED_STATE).filter(([k]) => !ALLOWED_STATE_DRIFT.includes(k)),
  )
}

/**
 * `actual` 是否包含 `expected` 的每一个取值（对象逐键递归，其余按值相等）。返回不符合的路径与取值（空数组 = 都在）
 */
export function missingValues(expected, actual, path = '') {
  if (expected && typeof expected === 'object' && !Array.isArray(expected)) {
    if (!actual || typeof actual !== 'object' || Array.isArray(actual))
      return [`${path || '（根）'}：期望对象，实际 ${JSON.stringify(actual) ?? '（无）'}`]
    return Object.entries(expected).flatMap(([k, v]) =>
      missingValues(v, actual[k], path ? `${path}.${k}` : k),
    )
  }
  return stable(expected) === stable(actual)
    ? []
    : [`${path}：期望 ${JSON.stringify(expected)}，实际 ${JSON.stringify(actual) ?? '（无）'}`]
}

export function seed(home) {
  const dir = wispDir(home)
  mkdirSync(dir, { recursive: true })
  writeFileSync(join(dir, 'settings.json'), `${JSON.stringify(SEED_SETTINGS, null, 2)}\n`)
  writeFileSync(join(dir, 'state.json'), `${JSON.stringify(SEED_STATE, null, 2)}\n`)
  return dir
}

function readJsonFile(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

export function setPolicy(home, policy, resetCheck) {
  if (!['notify', 'auto_install', 'off'].includes(policy)) throw new Error(`未知的策略 ${policy}`)
  const dir = wispDir(home)
  const settingsPath = join(dir, 'settings.json')
  const settings = readJsonFile(settingsPath)
  settings.general = { ...settings.general, update_policy: policy }
  writeFileSync(settingsPath, `${JSON.stringify(settings, null, 2)}\n`)
  if (resetCheck) {
    const statePath = join(dir, 'state.json')
    const state = existsSync(statePath) ? readJsonFile(statePath) : {}
    state.last_update_check_ms = 0
    writeFileSync(statePath, `${JSON.stringify(state, null, 2)}\n`)
  }
}

function sha256(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex')
}

const digest = (s) => createHash('sha256').update(s).digest('hex')

/**
 * SQLite 数据库的表与行数、剪贴板 / 识别历史的逐行摘要、种子记录的行数与摘要（没有 sqlite3 命令时为 null）
 */
function sqliteSummary(path) {
  const q = (sql) =>
    spawnSync('sqlite3', ['-readonly', '-batch', '-noheader', path, sql], { encoding: 'utf8' })
  const tables = q("select name from sqlite_master where type = 'table' order by name")
  if (tables.error || tables.status !== 0) return null
  const out = {
    user_version: q('pragma user_version').stdout.trim(),
    tables: {},
    rows: {},
    seed: {},
  }
  const names = tables.stdout.split('\n').filter(Boolean)
  for (const name of names) {
    out.tables[name] = Number(q(`select count(*) from "${name.replaceAll('"', '""')}"`).stdout)
  }
  // 剪贴板历史与识别历史的内容摘要（不含图片），核对更新前后逐行一致；种子记录单独记一份，核对它们确实还在
  const seedText = SEED_HISTORY_TEXT.replaceAll("'", "''")
  for (const [table, cols] of Object.entries(HISTORY_DIGEST)) {
    if (!names.includes(table)) continue
    const rows = q(`select ${cols} from ${table} order by id`)
    if (rows.status === 0) out.rows[table] = digest(rows.stdout)
    const where = `from ${table} where text = '${seedText}'`
    const seeded = q(`select ${cols} ${where} order by id`)
    const count = q(`select count(*) ${where}`)
    if (seeded.status === 0 && count.status === 0)
      out.seed[table] = { count: Number(count.stdout), digest: digest(seeded.stdout) }
  }
  return out
}

/** 参与内容摘要的列（与 wisp-clipboard 的 schema 对应；BLOB 取十六进制，输出里不夹原始字节） */
const HISTORY_DIGEST = {
  items: 'id, kind, created_at, last_used_at, use_count, pinned, hex(hash), text_preview, text',
  ocr_history: 'id, created_at, mode, model, text',
}

/** 种子历史的内容（更新后按内容摘要核对） */
export const SEED_HISTORY_TEXT = 'Wisp updater rehearsal: this record must survive the update'

/**
 * 往 0.1.0 建好的 clipboard.db 里写 1 条剪贴板记录（置顶，不会被条数上限清理）与 1 条识别历史（应用必须已退出；表结构见
 * wisp-clipboard `schema.rs`，FTS 由触发器同步）。返回写入后种子记录的行数与摘要 `{ text, rows: { items, ocr_history } }`，
 * 更新后的快照必须有同样的两行（`checkExpectations`）；没有 sqlite3 时返回 null
 */
export function seedHistory(home, now = Date.now()) {
  const db = join(wispDir(home), 'clipboard.db')
  if (!existsSync(db)) throw new Error(`${db} 不存在：先让应用启动一次`)
  const text = SEED_HISTORY_TEXT.replaceAll("'", "''")
  const sql = [
    'BEGIN;',
    `INSERT INTO items(kind, created_at, last_used_at, use_count, pinned, hash, text_preview, text)
       VALUES ('text', ${now}, ${now}, 2, 1, randomblob(32), '${text}', '${text}');`,
    `INSERT INTO ocr_history(created_at, mode, model, text) VALUES (${now}, 'markdown', 'rehearsal-model', '${text}');`,
    'COMMIT;',
  ].join('\n')
  const r = spawnSync('sqlite3', ['-batch', db], { input: sql, encoding: 'utf8' })
  if (r.error?.code === 'ENOENT') return null
  if (r.status !== 0) throw new Error(`写入 clipboard.db 失败：${r.stderr.trim()}`)
  const rows = sqliteSummary(db)?.seed ?? {}
  for (const table of Object.keys(HISTORY_DIGEST)) {
    if (rows[table]?.count !== 1)
      throw new Error(`写入后 ${table} 里的种子记录不是 1 条：${stable(rows[table])}`)
  }
  return { text: SEED_HISTORY_TEXT, rows }
}

/** 配置与数据目录的快照：JSON 文件存解析后的值，数据库存表与行数，其他文件存 sha256（WAL / SHM 只记是否存在） */
export function snapshot(home) {
  const dir = wispDir(home)
  const files = {}
  for (const path of walk(dir).toSorted()) {
    const name = relative(dir, path).split(sep).join('/')
    if (/\.(db-wal|db-shm)$/.test(name) || name.includes('.wisp-write-test-')) {
      files[name] = { present: true }
    } else if (name.endsWith('.json')) {
      try {
        files[name] = { json: readJsonFile(path) }
      } catch {
        files[name] = { sha256: sha256(path), invalidJson: true }
      }
    } else if (name.endsWith('.db')) {
      files[name] = { sqlite: sqliteSummary(path), bytes: statSync(path).size }
    } else {
      files[name] = { sha256: sha256(path) }
    }
  }
  return { dir, files }
}

const stable = (v) =>
  JSON.stringify(v, (_, x) =>
    x && typeof x === 'object' && !Array.isArray(x)
      ? Object.fromEntries(Object.entries(x).toSorted(([a], [b]) => (a < b ? -1 : 1)))
      : x,
  )

/** 比较两份快照，返回不允许的差异（空数组 = 通过）与允许的变化 */
export function compareSnapshots(before, after) {
  const problems = []
  const allowed = []
  for (const [name, b] of Object.entries(before.files)) {
    const a = after.files[name]
    if (!a) {
      problems.push(`${name}：更新后不见了`)
      continue
    }
    if (b.present) continue
    if (name === 'state.json' && b.json && a.json) {
      const keys = new Set([...Object.keys(b.json), ...Object.keys(a.json)])
      for (const k of keys) {
        if (stable(b.json[k]) === stable(a.json[k])) continue
        if (ALLOWED_STATE_DRIFT.includes(k)) allowed.push(`state.json ${k}`)
        else
          problems.push(
            `state.json ${k}：${stable(b.json[k]) ?? '（无）'} → ${stable(a.json[k]) ?? '（无）'}`,
          )
      }
      continue
    }
    if (b.sqlite && a.sqlite) {
      if (stable(b.sqlite) !== stable(a.sqlite))
        problems.push(`${name}：${stable(b.sqlite)} → ${stable(a.sqlite)}`)
      continue
    }
    if (stable(b) !== stable(a)) problems.push(`${name}：内容变了`)
  }
  const added = Object.keys(after.files).filter((n) => !(n in before.files))
  return { problems, allowed, added }
}

/**
 * 快照里必须有的东西（`compareSnapshots` 只核对前后相同，两份一样缺也会通过）：演练设置（`expectedSettings`）、种子 state
 * （`expectedState`）、seed-history 写入的种子记录（行数与摘要都与写入时一致）。返回问题列表（空数组 = 都在）
 */
export function checkExpectations(snap, { seeded, label = '' } = {}) {
  const at = (name) => `${label ? `${label} ` : ''}${name}`
  const problems = []
  for (const [name, expected] of [
    ['settings.json', expectedSettings()],
    ['state.json', expectedState()],
  ]) {
    const json = snap.files[name]?.json
    if (!json) problems.push(`${at(name)}：缺失或不是合法 JSON`)
    else for (const m of missingValues(expected, json)) problems.push(`${at(name)} ${m}`)
  }
  if (seeded?.rows) {
    const db = snap.files['clipboard.db']?.sqlite
    if (!db) problems.push(`${at('clipboard.db')}：缺失或读不了（没有 sqlite3？）`)
    else {
      for (const [table, want] of Object.entries(seeded.rows)) {
        const got = db.seed?.[table]
        if (stable(got) !== stable(want))
          problems.push(
            `${at('clipboard.db')} ${table} 的种子记录：写入时 ${stable(want)}，快照里 ${stable(got) ?? '（无）'}`,
          )
      }
    }
  }
  return problems
}

// ---------------------------------------------------------------------------
// 命令行

/** tauri signer 写的 `.pub`：一行 base64（与 tauri.conf.json 的 pubkey 同一种写法） */
function readPubkey(path) {
  const pub = readFileSync(path, 'utf8').replace(/\s+/g, '')
  parsePublicKey(pub) // 格式不对时在这里报错
  return pub
}

function need(values, ...names) {
  for (const n of names) if (values[n] === undefined) throw new Error(`缺少 --${n}`)
}

async function main() {
  const { values, positionals } = parseArgs({
    allowPositionals: true,
    options: {
      port: { type: 'string' },
      identifier: { type: 'string', default: REHEARSAL_IDENTIFIER },
      out: { type: 'string' },
      file: { type: 'string' },
      sig: { type: 'string' },
      version: { type: 'string' },
      in: { type: 'string' },
      tarball: { type: 'string' },
      'base-url': { type: 'string' },
      notes: { type: 'string', default: '' },
      dir: { type: 'string' },
      log: { type: 'string' },
      home: { type: 'string' },
      policy: { type: 'string' },
      'reset-check': { type: 'boolean', default: false },
      before: { type: 'string' },
      after: { type: 'string' },
      seed: { type: 'string' },
      'pubkey-file': { type: 'string' },
      'self-test': { type: 'boolean', default: false },
    },
  })
  if (values['self-test']) return selfTest()
  const [cmd] = positionals
  switch (cmd) {
    case 'free-port':
      console.log(await freePort())
      return
    case 'overlay': {
      need(values, 'port', 'out')
      const testPubkey = values['pubkey-file'] ? readPubkey(values['pubkey-file']) : undefined
      const overlay = rehearsalOverlay({
        port: Number(values.port),
        identifier: values.identifier,
        testPubkey,
      })
      assertOverlaySafe(overlay, { testPubkey })
      writeFileSync(values.out, `${JSON.stringify(overlay, null, 2)}\n`)
      ok(
        `叠加配置 ${values.out}：端点 ${overlay.plugins.updater.endpoints[0]}，identifier ${overlay.identifier}，${testPubkey ? `一次性测试公钥 ${parsePublicKey(testPubkey).keyId}` : '正式公钥（不叠加 pubkey）'}`,
      )
      return
    }
    case 'verify': {
      need(values, 'file', 'sig')
      const testPubkey = values['pubkey-file'] ? readPubkey(values['pubkey-file']) : undefined
      const r = verifyFile(readFileSync(values.file), readFileSync(values.sig, 'utf8'), {
        version: values.version,
        ...(testPubkey ? { pub: parsePublicKey(testPubkey) } : {}),
      })
      ok(
        `${basename(values.file)}：${testPubkey ? '测试公钥' : '正式公钥'} ${r.keyId} 校验通过，签名记录的版本 ${r.version ?? '（没有）'}`,
      )
      info(`trusted comment: ${r.trustedComment.replaceAll('\t', '  ')}`)
      return
    }
    case 'forge': {
      need(values, 'in', 'out', 'version')
      const { sig, key } = forgeSignature(readFileSync(values.in), {
        file: basename(values.in),
        version: values.version,
      })
      writeFileSync(values.out, sig)
      ok(`用一次性密钥 ${key.id} 签名 → ${values.out}（私钥只在内存里，没有落盘）`)
      return
    }
    case 'tamper': {
      need(values, 'in', 'out')
      const at = tamper(values.in, values.out)
      ok(`${values.out}：翻转了偏移 ${at} 的一个字节`)
      return
    }
    case 'manifest': {
      need(values, 'tarball', 'sig', 'base-url', 'version', 'out')
      const latest = rehearsalManifest({
        version: values.version,
        notes: values.notes,
        tarball: values.tarball,
        signature: readFileSync(values.sig, 'utf8'),
        baseUrl: values['base-url'],
      })
      writeFileSync(values.out, `${JSON.stringify(latest, null, 2)}\n`)
      ok(`latest.json → ${latest.platforms['darwin-aarch64'].url}`)
      return
    }
    case 'serve': {
      need(values, 'dir', 'port')
      const { server, port } = await startServer({
        dir: values.dir,
        port: Number(values.port),
        log: values.log,
      })
      console.log(`serving ${resolve(values.dir)} on http://127.0.0.1:${port}/`)
      const stop = () => server.close(() => process.exit(0))
      process.on('SIGTERM', stop)
      process.on('SIGINT', stop)
      return
    }
    case 'seed':
      need(values, 'home')
      ok(`已写入演练设置 ${seed(values.home)}`)
      return
    case 'seed-history': {
      need(values, 'home', 'out')
      const seeded = seedHistory(values.home)
      writeFileSync(values.out, `${JSON.stringify(seeded ?? { skipped: 'no sqlite3' }, null, 2)}\n`)
      if (seeded)
        ok(`clipboard.db：写入 1 条剪贴板记录与 1 条识别历史（摘要记在 ${basename(values.out)}）`)
      else info('没有 sqlite3：不写种子历史')
      return
    }
    case 'set-policy':
      need(values, 'home', 'policy')
      setPolicy(values.home, values.policy, values['reset-check'])
      ok(`更新策略 = ${values.policy}${values['reset-check'] ? '，last_update_check_ms = 0' : ''}`)
      return
    case 'snapshot':
      need(values, 'home', 'out')
      writeFileSync(values.out, `${JSON.stringify(snapshot(values.home), null, 2)}\n`)
      return
    case 'compare': {
      need(values, 'before', 'after', 'seed')
      const before = readJsonFile(values.before)
      const after = readJsonFile(values.after)
      const seeded = readJsonFile(values.seed)
      const r = compareSnapshots(before, after)
      for (const a of r.allowed) info(`  允许的变化：${a}`)
      for (const a of r.added) info(`  新增文件：${a}`)
      const problems = [
        ...r.problems,
        ...checkExpectations(before, { seeded, label: '更新前' }),
        ...checkExpectations(after, { seeded, label: '更新后' }),
      ]
      if (problems.length) throw new Error(`更新前后的数据不一致：\n  ${problems.join('\n  ')}`)
      if (seeded.rows)
        info(
          `  种子记录还在：${Object.entries(seeded.rows)
            .map(([t, v]) => `${t} ${v.count} 条`)
            .join('、')}`,
        )
      else info(`  没有核对种子记录（${seeded.skipped ?? 'seed-history 没有写入'}）`)
      info('  演练设置与种子 state 的取值都在（expectedSettings / expectedState）')
      ok('设置与数据在更新前后一致')
      return
    }
    default:
      throw new Error(`未知命令 ${cmd ?? '（空）'}；见文件头部的用法`)
  }
}

// ---------------------------------------------------------------------------
// 自检

async function selfTest() {
  // 叠加配置：只换端点、打开演练钩子；不碰公钥
  const overlay = rehearsalOverlay({ port: 18765 })
  assert.equal(overlay.plugins.updater.endpoints[0], 'http://127.0.0.1:18765/latest.json')
  assert.equal(overlay.plugins[REHEARSAL_CONFIG_KEY].enabled, true)
  assert.equal(overlay.identifier, REHEARSAL_IDENTIFIER)
  assertOverlaySafe(overlay)
  assert.throws(() => rehearsalOverlay({ port: 0 }), /端口/)
  assert.throws(() => rehearsalOverlay({ port: 1, identifier: 'io.github.yzlltyyh.wisp' }), /不同/)
  // 测试密钥模式：pubkey 换成一次性测试公钥；只接受给出的那一把，key id 不能是正式的
  const officialB64 = readJsonFile(join(TAURI_DIR, 'tauri.conf.json')).plugins.updater.pubkey
  const testKey = throwawayKey()
  const withTest = rehearsalOverlay({ port: 18765, testPubkey: testKey.pub })
  assert.equal(withTest.plugins.updater.pubkey, testKey.pub)
  assertOverlaySafe(withTest, { testPubkey: testKey.pub })
  assert.throws(() => assertOverlaySafe(withTest), /pubkey/)
  assert.throws(() => assertOverlaySafe(withTest, { testPubkey: throwawayKey().pub }), /pubkey/)
  assert.throws(
    () =>
      assertOverlaySafe(rehearsalOverlay({ port: 1, testPubkey: officialB64 }), {
        testPubkey: officialB64,
      }),
    /正式公钥/,
  )
  assert.equal('pubkey' in overlay.plugins.updater, false, '不给测试公钥时不叠加 pubkey')
  for (const bad of [
    { pubkey: 'x' },
    { pubkey: officialB64 },
    { allowDowngrades: true },
    { dangerousAcceptInvalidCerts: true },
    { endpoints: ['http://example.com/latest.json'] },
  ]) {
    assert.throws(() => assertOverlaySafe({ plugins: { updater: bad } }), /叠加配置|127/)
  }
  // 与 Rust 侧的配置项名一致
  const rust = readFileSync(join(TAURI_DIR, 'src', 'updater', 'rehearsal.rs'), 'utf8')
  assert.ok(rust.includes(`pub const CONFIG_KEY: &str = "${REHEARSAL_CONFIG_KEY}";`))
  // 演练设置把五个全局快捷键都设为不绑定：动作列表与 Binding::NONE 跟 wisp-types 一致（新增动作时这里先失败）
  const hotkeyRs = readFileSync(join(ROOT, 'crates', 'wisp-types', 'src', 'hotkey.rs'), 'utf8')
  const asStr = /pub const fn as_str\(self\) -> &'static str \{([\s\S]*?)\n {4}\}/.exec(hotkeyRs)
  assert.ok(asStr, 'wisp-types hotkey.rs 里找不到 HotkeyAction::as_str')
  assert.deepEqual(
    [...asStr[1].matchAll(/=> "([a-z_]+)"/g)].map((m) => m[1]),
    HOTKEY_ACTIONS,
    'HotkeyAction 的动作变了：同步 HOTKEY_ACTIONS',
  )
  assert.ok(hotkeyRs.includes(`pub const NONE: &'static str = "${BINDING_NONE}";`))
  assert.deepEqual(Object.keys(SEED_SETTINGS.hotkeys.bindings), HOTKEY_ACTIONS)
  assert.ok(Object.values(SEED_SETTINGS.hotkeys.bindings).every((b) => b === BINDING_NONE))
  for (const k of ['bobber', 'double_tap', 'mouse'])
    assert.equal(SEED_SETTINGS.hotkeys[k].enabled, false, k)
  assert.equal(SEED_SETTINGS.clipboard.enabled, false)
  assert.equal(expectedSettings().general.update_policy, undefined)
  assert.equal(
    SEED_SETTINGS.general.update_policy,
    'notify',
    'expectedSettings 不能改动 SEED_SETTINGS',
  )
  // 正式配置里没有演练项、端点是 https
  const conf = readJsonFile(join(TAURI_DIR, 'tauri.conf.json'))
  assert.equal(conf.plugins[REHEARSAL_CONFIG_KEY], undefined)
  assert.ok(conf.plugins.updater.endpoints.every((u) => u.startsWith('https://')))
  assert.notEqual(conf.plugins.updater.dangerousInsecureTransportProtocol, true)

  // 签名：一次性密钥签的包用它自己的公钥能过，用正式公钥不过（key id 不一致）；篡改后不过；版本核对
  const work = mkdtempSync(join(tmpdir(), 'wisp-rehearsal-selftest-'))
  try {
    const pkg = join(work, 'Wisp.app.tar.gz')
    writeFileSync(pkg, randomBytes(4096))
    const { sig, key } = forgeSignature(readFileSync(pkg), {
      file: 'Wisp.app.tar.gz',
      version: '0.1.1',
    })
    const pub = parsePublicKey(key.pub)
    const r = verifyFile(readFileSync(pkg), sig, { version: '0.1.1', pub })
    assert.equal(r.version, '0.1.1')
    assert.equal(r.keyId, key.id)
    assert.throws(
      () => verifyFile(readFileSync(pkg), sig, { version: '0.1.2', pub }),
      /不是 0\.1\.2/,
    )
    assert.throws(() => verifyFile(readFileSync(pkg), sig), /key id/)
    const bad = join(work, 'tampered.tar.gz')
    const at = tamper(pkg, bad)
    assert.equal(at, 2048)
    const diff = [...readFileSync(bad)].filter((b, i) => b !== readFileSync(pkg)[i]).length
    assert.equal(diff, 1)
    assert.throws(() => verifyFile(readFileSync(bad), sig, { pub }), /不匹配/)
    assert.equal(parseSignature(sig).algorithm, 'ED')
    assert.equal(signedVersion('timestamp:1\tfile:a\tversion:1.2.3'), '1.2.3')
    assert.equal(signedVersion('timestamp:1\tfile:a'), null)

    // latest.json：两个 darwin 键、地址指向本机服务器
    const latest = rehearsalManifest({
      version: '0.1.1',
      notes: 'n',
      tarball: 'Wisp_0.1.1_aarch64.app.tar.gz',
      signature: `${sig}\n`,
      baseUrl: 'http://127.0.0.1:18765',
      pubDate: '2026-10-12T00:00:00.000Z',
    })
    assert.deepEqual(Object.keys(latest.platforms), ['darwin-aarch64', 'darwin-aarch64-app'])
    assert.equal(
      latest.platforms['darwin-aarch64'].url,
      'http://127.0.0.1:18765/Wisp_0.1.1_aarch64.app.tar.gz',
    )
    assert.equal(latest.platforms['darwin-aarch64'].signature, sig)
    assert.equal(latest.version, '0.1.1')

    // 本机服务器：能取到文件、记日志；目录穿越 / 不存在 / 写方法被拒
    const www = join(work, 'www')
    mkdirSync(join(www, 'sub'), { recursive: true })
    copyFileSync(pkg, join(www, 'sub', 'a.tar.gz'))
    writeFileSync(join(www, 'latest.json'), JSON.stringify(latest))
    writeFileSync(join(work, 'secret.txt'), 'outside')
    const log = join(work, 'server.jsonl')
    const { server, port } = await startServer({ dir: www, log })
    try {
      const base = `http://127.0.0.1:${port}`
      const j = await fetch(`${base}/latest.json`)
      assert.equal(j.status, 200)
      assert.equal(j.headers.get('content-type'), 'application/json')
      assert.equal((await j.json()).version, '0.1.1')
      const f = await fetch(`${base}/sub/a.tar.gz`)
      assert.equal(f.headers.get('content-length'), '4096')
      assert.deepEqual(Buffer.from(await f.arrayBuffer()), readFileSync(pkg))
      for (const p of ['/../secret.txt', '/%2e%2e/secret.txt', '/missing', '/sub']) {
        const res = await fetch(`${base}${p}`)
        assert.equal(res.status, 404, p)
        await res.text()
      }
      const post = await fetch(`${base}/latest.json`, { method: 'POST', body: 'x' })
      assert.equal(post.status, 405)
      await post.text()
    } finally {
      await new Promise((r2) => server.close(r2))
    }
    const lines = readFileSync(log, 'utf8')
      .trim()
      .split('\n')
      .map((l) => JSON.parse(l))
    assert.equal(lines[0].path, '/latest.json')
    assert.equal(lines[1].bytes, 4096)
    assert.ok(lines.some((l) => l.status === 405))
    assert.ok((await freePort()) > 0)

    // 设置与数据：种子、改策略、快照与比较
    const home = join(work, 'home')
    seed(home)
    setPolicy(home, 'auto_install', true)
    const dir = wispDir(home)
    assert.equal(readJsonFile(join(dir, 'settings.json')).general.update_policy, 'auto_install')
    assert.equal(readJsonFile(join(dir, 'settings.json')).clipboard.enabled, false)
    assert.equal(readJsonFile(join(dir, 'state.json')).last_update_check_ms, 0)
    assert.deepEqual(readJsonFile(join(dir, 'settings.json')).hotkeys, SEED_SETTINGS.hotkeys)
    assert.throws(() => setPolicy(home, 'sometimes'), /未知/)
    writeFileSync(join(dir, 'blob.bin'), 'b')
    const before = snapshot(home)
    // 种子写好、只改了策略：演练设置与种子 state 都在（update_policy 与 last_update_check_ms 不算）
    assert.deepEqual(checkExpectations(before), [])
    const state = readJsonFile(join(dir, 'state.json'))
    state.last_update_check_ms = 123
    writeFileSync(join(dir, 'state.json'), JSON.stringify(state))
    writeFileSync(join(dir, 'new.txt'), 'n')
    let cmp = compareSnapshots(before, snapshot(home))
    assert.deepEqual(cmp.problems, [])
    assert.deepEqual(cmp.allowed, ['state.json last_update_check_ms'])
    assert.deepEqual(cmp.added, ['new.txt'])
    state.onboarding_done = false
    writeFileSync(join(dir, 'state.json'), JSON.stringify(state))
    rmSync(join(dir, 'blob.bin'))
    const settings = readJsonFile(join(dir, 'settings.json'))
    settings.appearance.accent = '#000000'
    writeFileSync(join(dir, 'settings.json'), JSON.stringify(settings))
    cmp = compareSnapshots(before, snapshot(home))
    assert.equal(cmp.problems.length, 3, cmp.problems.join('\n'))
    assert.ok(cmp.problems.some((p) => p.startsWith('state.json onboarding_done')))
    assert.ok(cmp.problems.some((p) => p.startsWith('blob.bin')))
    assert.ok(cmp.problems.some((p) => p.startsWith('settings.json')))
    assert.deepEqual(checkExpectations(snapshot(home), { label: '更新后' }), [
      '更新后 settings.json appearance.accent：期望 "#e11d48"，实际 "#000000"',
      '更新后 state.json onboarding_done：期望 true，实际 false',
    ])
    // 前后两份一样缺（例如「更新前」的快照拍晚了、安装时丢了数据）：compareSnapshots 看不出来，checkExpectations 能
    const settingsNoHotkeys = readJsonFile(join(dir, 'settings.json'))
    delete settingsNoHotkeys.hotkeys
    writeFileSync(join(dir, 'settings.json'), JSON.stringify(settingsNoHotkeys))
    const lostA = snapshot(home)
    const lostB = snapshot(home)
    assert.deepEqual(compareSnapshots(lostA, lostB).problems, [])
    assert.ok(checkExpectations(lostB).some((p) => p.startsWith('settings.json hotkeys：期望对象')))
    rmSync(join(dir, 'settings.json'))
    assert.ok(checkExpectations(snapshot(home)).includes('settings.json：缺失或不是合法 JSON'))
    assert.deepEqual(missingValues({ a: { b: 1 } }, { a: { b: 1, c: 2 }, d: 3 }), [])
    assert.deepEqual(missingValues({ a: [1, 2] }, { a: [2, 1] }), ['a：期望 [1,2]，实际 [2,1]'])
    assert.deepEqual(missingValues({ a: 1 }, {}), ['a：期望 1，实际 （无）'])

    // 种子历史（有 sqlite3 时）：按 wisp-clipboard 的表结构建库，写入后摘要变化；内容一改摘要就不同。
    // 系统的 sqlite3 太旧、建不了这套表（例如 ubuntu-22.04 的 3.37）时：macOS 上照样判失败（演练只在
    // macOS 上跑，那里的 sqlite3 必须能建表），其他平台跳过这一段并说明
    const hasSqlite = spawnSync('sqlite3', ['-version']).status === 0
    const db = join(dir, 'clipboard.db')
    const schema = readFileSync(join(ROOT, 'crates', 'wisp-clipboard', 'src', 'schema.rs'), 'utf8')
    const ddl = [...schema.matchAll(/const M\d_UP: &str = r"([\s\S]*?)";/g)].map((m) => m[1])
    assert.equal(ddl.length, 2, 'schema.rs 的迁移条数变了：同步 HISTORY_DIGEST 与 seedHistory')
    let created = null
    if (hasSqlite) {
      created = spawnSync('sqlite3', ['-batch', db], { input: ddl.join('\n'), encoding: 'utf8' })
      if (created.status !== 0 && process.platform !== 'darwin') {
        const version = spawnSync('sqlite3', ['-version'], { encoding: 'utf8' }).stdout.split(
          ' ',
        )[0]
        console.log(
          `  （系统 sqlite3 ${version} 建不了 wisp-clipboard 的表：${created.stderr.trim().split('\n')[0]}——跳过种子历史部分）`,
        )
        created = null
      }
    }
    if (created) {
      assert.equal(created.status, 0, `sqlite3 建表失败：${created.stderr}`)
      seed(home) // 上面改坏的设置与 state 恢复成种子
      const emptySnap = snapshot(home)
      const empty = emptySnap.files['clipboard.db'].sqlite
      assert.equal(empty.tables.items, 0)
      assert.deepEqual(empty.seed.items, { count: 0, digest: digest('') })
      const manifest = seedHistory(home, 1_760_000_000_000)
      assert.equal(manifest.text, SEED_HISTORY_TEXT)
      assert.deepEqual(Object.keys(manifest.rows), ['items', 'ocr_history'])
      const seededSnap = snapshot(home)
      const seeded = seededSnap.files['clipboard.db'].sqlite
      assert.equal(seeded.tables.items, 1)
      assert.equal(seeded.tables.ocr_history, 1)
      assert.notEqual(seeded.rows.items, empty.rows.items)
      assert.deepEqual(seeded.seed, manifest.rows)
      assert.deepEqual(checkExpectations(seededSnap, { seeded: manifest }), [])
      // 种子记录必须在：写入之前拍的快照（「更新前」拍早了）不算
      assert.deepEqual(
        checkExpectations(emptySnap, { seeded: manifest }).map((p) => p.split('：')[0]),
        ['clipboard.db items 的种子记录', 'clipboard.db ocr_history 的种子记录'],
      )
      // 前后一样缺（安装时丢了历史）：compareSnapshots 看不出来，checkExpectations 能
      spawnSync('sqlite3', ['-batch', db, 'delete from ocr_history'])
      const lost1 = snapshot(home)
      assert.deepEqual(compareSnapshots(lost1, snapshot(home)).problems, [])
      assert.deepEqual(
        checkExpectations(lost1, { seeded: manifest, label: '更新后' }).map(
          (p) => p.split('：')[0],
        ),
        ['更新后 clipboard.db ocr_history 的种子记录'],
      )
      // 内容一改，逐行摘要与种子摘要都不同
      spawnSync('sqlite3', ['-batch', db, 'update items set use_count = 9'])
      const changed = snapshot(home).files['clipboard.db'].sqlite
      assert.notEqual(changed.rows.items, seeded.rows.items)
      assert.notEqual(changed.seed.items.digest, manifest.rows.items.digest)
      assert.equal(changed.seed.items.count, 1)
      // 没有 sqlite3 时 seed-history 写 { skipped }，不核对种子记录
      assert.deepEqual(checkExpectations(lost1, { seeded: { skipped: 'no sqlite3' } }), [])
    }
  } finally {
    rmSync(work, { recursive: true, force: true })
  }
  assert.equal(dirname(wispDir('/h')), '/h/Library/Application Support')
  ok('updater-rehearsal 自检通过')
}

if (isMain(import.meta.url)) await run(main)
