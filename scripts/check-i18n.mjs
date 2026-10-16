#!/usr/bin/env node
/**
 * scripts/check-i18n.mjs：i18n 检查。
 *
 * 用法：node scripts/check-i18n.mjs [--self-test]
 *   --self-test  用构造样例检查词法器、测试代码跳过与硬编码文案规则（不读仓库文件之外的东西，CI 用）
 *
 * 检查项：
 *   ① zh-CN / en-US 的命名空间文件一一对应，每个命名空间展开后的 key 集合一致，叶子都是非空字符串，
 *      同一 key 在两种语言里的占位符（`{name}`）集合一致；
 *   ② Rust 下发的 i18n key（wisp-types crate 文档「Rust 下发的 i18n key」：`errors.platform.*`、`errors.hint.*`、
 *      `settings.providers.template.*`、`settings.providers.region.*`、`hotkeys.*`）：扫描 `crates/*\/src` 与
 *      `apps/desktop/src-tauri/src` 中以这些前缀开头的字符串字面量，两种语言都必须有对应条目（前两段是设置字段
 *      路径的，如 `hotkeys.copy_wait_ms`，不算 key）。用一个小词法器区分注释、字符串（含原始字符串）与字符字面量；
 *      只跳过 `#[cfg(test)]` / `#[cfg(all(test, …))]` 标注的那一个条目（`mod tests { … }`、函数、`use` 等，按花括号
 *      配对），`#[cfg(test)] mod x;` 声明的测试模块文件与 `#![cfg(test)]` 的文件整个跳过。`format!` 拼出的动态 key
 *      无法静态检查，需要在改动说明里列出并人工核对；
 *   ③ 硬编码文案（伪本地化 `?lang=pseudo` 的静态版）：TSX 按 JSX 结构扫描（jsxCopy，不是逐行正则）——子节点里的每一段
 *      JSX 文本（开始标签、`{…}`、子元素之后的文字都算）、`title` / `aria-label` / `placeholder` / `alt` / `label` 等
 *      属性的字面值、属性与子节点表达式里被渲染出来的字符串（`open ? 'Open' : 'Close'`、`name ?? 'Untitled'`）；
 *      TS / TSX 中直接写进 DOM 的字面量（`textContent` / `innerText` / `title` / `placeholder` / `ariaLabel` 等赋值、
 *      `setAttribute('aria-label' | 'title' | …, '…')`、`createTextNode('…')`——Overlay 是原生 TS + DOM）。出现中文或
 *      英文单词（计量单位除外）即报错，用户可见文案必须经 `t()`。测试、浏览器 mock 不检查；确需原样显示的（品牌名、
 *      开发者模式面板）在同一行或上一行写 `i18n-ignore`，整个文件用 `i18n-ignore-file`。
 */
import assert from 'node:assert/strict'
import { readdirSync, readFileSync } from 'node:fs'
import { basename, dirname, join } from 'node:path'
import {
  APP_DIR,
  ROOT,
  TAURI_DIR,
  error,
  isMain,
  lineOf,
  ok,
  regexEnd,
  rel,
  run,
  stripJsComments,
  walk,
  warn,
} from './lib/common.mjs'

const LOCALES = join(APP_DIR, 'src', 'locales')
const LANGS = ['zh-CN', 'en-US']
/** Rust 下发 key 的前缀（与 wisp-types 的文档表一致；新增产出方时同步这里） */
export const RUST_KEY_PREFIXES = [
  'errors.platform.',
  'errors.capture.',
  'errors.hint.',
  'settings.providers.template.',
  'settings.providers.region.',
  'hotkeys.',
  // URL 预览提示、测试连接的附加提示、开机自启的问题、剪贴板历史不可用的横幅
  'settings.providers.url_warning.',
  'settings.providers.test_notice.',
  'settings.general.autostart_issue.',
  'clipboard.history_',
  // 同步命令的错误消息（IpcError.message 下发文案 key，前端经 ipcErrorText 翻译）
  'errors.sync.',
  // 离线模型的设备适配说明（wisp-local 的 fitness 下发 Fitness.reason_key）
  'settings.offline.fitness.',
]
/**
 * 已知缺失、暂时允许的 key（每次运行都会告警，不会被遗忘）。目前为空：新增条目时写明缺失的原因与补齐的条件。
 */
export const KNOWN_MISSING = new Set([])

// ---------------------------------------------------------------------------
// ① locales

/** 展开嵌套对象：`{ a: { b: 'x' } }` → `Map { 'a.b' => 'x' }`；非字符串叶子记为问题 */
export function flatten(obj, prefix, out, problems, where) {
  for (const [k, v] of Object.entries(obj)) {
    const key = prefix ? `${prefix}.${k}` : k
    if (v !== null && typeof v === 'object' && !Array.isArray(v))
      flatten(v, key, out, problems, where)
    else if (typeof v !== 'string') problems.push({ file: where, msg: `${key} 的值必须是字符串` })
    else if (v.trim() === '') problems.push({ file: where, msg: `${key} 的文案为空` })
    else out.set(key, v)
  }
  return out
}

/** 文案里的占位符名集合 */
export function placeholders(s) {
  return new Set([...s.matchAll(/\{\s*([A-Za-z_][\w]*)\s*\}/g)].map((m) => m[1]))
}

function loadLocales(problems) {
  const byLang = {}
  for (const lang of LANGS) {
    const dir = join(LOCALES, lang)
    let files = []
    try {
      files = readdirSync(dir).filter((f) => f.endsWith('.json'))
    } catch {
      problems.push({ file: rel(dir), msg: '语言目录不存在' })
    }
    byLang[lang] = new Map()
    for (const f of files) {
      const path = join(dir, f)
      let json
      try {
        json = JSON.parse(readFileSync(path, 'utf8'))
      } catch (e) {
        problems.push({
          file: rel(path),
          msg: `不是合法的 JSON：${e instanceof Error ? e.message : e}`,
        })
        continue
      }
      if (json === null || typeof json !== 'object' || Array.isArray(json)) {
        problems.push({ file: rel(path), msg: '顶层必须是对象' })
        continue
      }
      byLang[lang].set(f.replace(/\.json$/, ''), flatten(json, '', new Map(), problems, rel(path)))
    }
  }
  return byLang
}

function checkLocales(byLang, problems) {
  const [a, b] = LANGS
  const nsA = new Set(byLang[a].keys())
  const nsB = new Set(byLang[b].keys())
  for (const ns of nsA)
    if (!nsB.has(ns))
      problems.push({ file: `${rel(LOCALES)}/${b}`, msg: `缺少命名空间 ${ns}.json` })
  for (const ns of nsB)
    if (!nsA.has(ns))
      problems.push({ file: `${rel(LOCALES)}/${a}`, msg: `缺少命名空间 ${ns}.json` })
  let keys = 0
  for (const ns of nsA) {
    const ka = byLang[a].get(ns)
    const kb = byLang[b].get(ns)
    if (!kb) continue
    keys += ka.size
    for (const [k, v] of ka) {
      if (!kb.has(k)) {
        problems.push({ file: `${rel(LOCALES)}/${b}/${ns}.json`, msg: `缺少 ${k}（${a} 有）` })
        continue
      }
      const pa = [...placeholders(v)].toSorted().join(',')
      const pb = [...placeholders(kb.get(k))].toSorted().join(',')
      if (pa !== pb) {
        problems.push({
          file: `${rel(LOCALES)}/${b}/${ns}.json`,
          msg: `${k} 的占位符不一致：${a} {${pa}}，${b} {${pb}}`,
        })
      }
    }
    for (const k of kb.keys()) {
      if (!ka.has(k))
        problems.push({ file: `${rel(LOCALES)}/${a}/${ns}.json`, msg: `缺少 ${k}（${b} 有）` })
    }
  }
  return { namespaces: nsA.size, keys }
}

// ---------------------------------------------------------------------------
// ② Rust 下发的 key

/**
 * Rust 源码的轻量词法扫描：返回 `{ code, skeleton }`，两者与原文等长、换行位置不变（行号仍然准确）。
 * - code：注释换成空格，字符串与字符字面量原样保留（用来找 key 字面量）；
 * - skeleton：注释、字符串与字符字面量的内容都换成空格（用来配对花括号、识别属性与条目边界）。
 * 处理行注释、可嵌套的块注释、普通 / 字节字符串（含转义）、原始字符串 `r#"…"#`、字符字面量 `'"'`、`'\''`，
 * 以及与字符字面量形似的生命周期 `'a`。
 */
export function lexRust(text) {
  const n = text.length
  const code = text.split('')
  const skel = text.split('')
  const blank = (arr, from, to) => {
    for (let k = from; k < to; k++) if (arr[k] !== '\n') arr[k] = ' '
  }
  const ident = (c) => c !== undefined && /[A-Za-z0-9_]/.test(c)
  let i = 0
  while (i < n) {
    const c = text[i]
    const next = text[i + 1]
    if (c === '/' && next === '/') {
      const end = text.indexOf('\n', i)
      const to = end < 0 ? n : end
      blank(code, i, to)
      blank(skel, i, to)
      i = to
    } else if (c === '/' && next === '*') {
      let depth = 1
      let j = i + 2
      while (j < n && depth > 0) {
        if (text[j] === '/' && text[j + 1] === '*') {
          depth++
          j += 2
        } else if (text[j] === '*' && text[j + 1] === '/') {
          depth--
          j += 2
        } else j++
      }
      blank(code, i, j)
      blank(skel, i, j)
      i = j
    } else if (
      (c === 'r' || (c === 'b' && next === 'r')) &&
      !ident(text[i - 1]) &&
      /^b?r#*"/.test(text.slice(i, i + 260))
    ) {
      // 原始字符串：r"…"、r#"…"#、br"…"
      const m = /^b?r(#*)"/.exec(text.slice(i, i + 260))
      const close = `"${m[1]}`
      const bodyStart = i + m[0].length
      const end = text.indexOf(close, bodyStart)
      const to = end < 0 ? n : end + close.length
      blank(skel, bodyStart, to - close.length)
      i = to
    } else if (c === '"' || (c === 'b' && next === '"' && !ident(text[i - 1]))) {
      let j = c === 'b' ? i + 2 : i + 1
      const bodyStart = j
      while (j < n && text[j] !== '"') j += text[j] === '\\' ? 2 : 1
      blank(skel, bodyStart, j)
      i = j + 1
    } else if (c === "'") {
      // 字符字面量：'x'、'\n'、'\''、'\u{1F600}'；否则是生命周期或标签（'a、'static）
      const m = /^'(?:\\(?:u\{[0-9A-Fa-f]{1,6}\}|x[0-9A-Fa-f]{2}|.)|[^\\'\n])'/u.exec(
        text.slice(i, i + 16),
      )
      if (m) {
        blank(skel, i + 1, i + m[0].length - 1)
        i += m[0].length
      } else i++
    } else i++
  }
  return { code: code.join(''), skeleton: skel.join('') }
}

const TEST_ATTR = /#\[\s*cfg\s*\(\s*(?:test|all\s*\(\s*test\b[^\]]*)\s*\)\s*\]/g

/**
 * 去掉注释与测试代码之后的 Rust 源码（与原文等长、行号不变），以及 `#[cfg(test)] mod x;` 声明的测试模块名。
 * 只跳过 `#[cfg(test)]` 后面紧跟的那一个条目：先越过同一条目上的其他属性，再越过条目本身——遇到 `{` 按花括号配对
 * 到对应的 `}`（`mod tests { … }`、`fn`、`impl` 等），先遇到 `;` 则到分号为止（`use`、`mod x;`、`const` 等）。
 * 文件开头的 `#![cfg(test)]` 表示整个文件都是测试代码。
 */
export function rustCode(text) {
  const { code, skeleton } = lexRust(text)
  if (/#!\[\s*cfg\s*\(\s*test\s*\)\s*\]/.test(skeleton)) {
    return { code: code.replace(/[^\n]/g, ' '), testModules: [] }
  }
  const out = code.split('')
  const testModules = []
  for (const m of skeleton.matchAll(TEST_ATTR)) {
    let j = m.index + m[0].length
    // 越过后续属性（#[…]，可能跨行、含嵌套方括号）与空白
    for (;;) {
      while (j < skeleton.length && /\s/.test(skeleton[j])) j++
      if (skeleton[j] !== '#' || skeleton[j + 1] !== '[') break
      let depth = 0
      for (; j < skeleton.length; j++) {
        if (skeleton[j] === '[') depth++
        else if (skeleton[j] === ']' && --depth === 0) {
          j++
          break
        }
      }
    }
    const itemStart = j
    let end = skeleton.length
    let nest = 0 // 圆括号 / 方括号的深度：`[u8; 4]`、`fn f(a: [u8; 2])` 里的分号不是条目结尾
    for (; j < skeleton.length; j++) {
      const ch = skeleton[j]
      if (ch === '(' || ch === '[') nest++
      else if (ch === ')' || ch === ']') nest--
      if (nest > 0) continue
      if (ch === ';') {
        end = j + 1
        break
      }
      if (ch === '{') {
        let depth = 0
        for (; j < skeleton.length; j++) {
          if (skeleton[j] === '{') depth++
          else if (skeleton[j] === '}' && --depth === 0) break
        }
        end = j + 1
        break
      }
    }
    const item = skeleton.slice(itemStart, end)
    const decl = /^(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;$/.exec(
      item.trim(),
    )
    if (decl) testModules.push(decl[1])
    for (let k = m.index; k < end && k < out.length; k++) if (out[k] !== '\n') out[k] = ' '
  }
  return { code: out.join(''), testModules }
}

/** `#[cfg(test)] mod name;` 对应的文件：`foo/mod.rs`、`lib.rs`、`main.rs` 里的子模块在同目录，`foo.rs` 的在 `foo/` 下 */
export function testModuleFiles(file, names) {
  const dir = dirname(file)
  const stem = basename(file, '.rs')
  const base = ['mod', 'lib', 'main'].includes(stem) ? dir : join(dir, stem)
  return names.flatMap((name) => [join(base, `${name}.rs`), join(base, name, 'mod.rs')])
}

let settingsShape
/**
 * 是否为设置字段路径（如 `hotkeys.copy_wait_ms`、`hotkeys.bindings.ocr`，见 settings 校验与 ConfigIssue）而不是 i18n key：
 * 前两段是 Settings 黄金样例里存在的「分组.字段」即视为字段路径。设置分组 `hotkeys` 与 i18n 命名空间 `hotkeys` 同名，
 * 不排除就会误报。
 */
export function isSettingsPath(key) {
  settingsShape ??= JSON.parse(
    readFileSync(join(APP_DIR, 'src', 'shared', 'ipc', 'fixtures', 'Settings.json'), 'utf8'),
  )
  const [group, field] = key.split('.')
  const section = settingsShape[group]
  return (
    section !== null &&
    typeof section === 'object' &&
    field !== undefined &&
    Object.hasOwn(section, field)
  )
}

/** Rust 源码中的 key 字面量：`[{ key, line }]`（code 为 rustCode 的输出） */
export function rustKeys(code) {
  const out = []
  for (const m of code.matchAll(/"([a-z][a-z0-9_]*(?:\.[a-z0-9_]+)+)"/g)) {
    if (RUST_KEY_PREFIXES.some((p) => m[1].startsWith(p)) && !isSettingsPath(m[1])) {
      out.push({ key: m[1], line: lineOf(code, m.index) })
    }
  }
  return out
}

function checkRustKeys(byLang, problems, warnings) {
  const files = [
    ...walk(join(ROOT, 'crates'), (n) => n === 'target' || n === 'tests' || n === 'benches'),
    ...walk(join(TAURI_DIR, 'src')),
  ].filter((f) => f.endsWith('.rs') && !/[\\/](?:tests|benches)[\\/]/.test(f))
  const lookup = (lang, key) => {
    const [ns, ...rest] = key.split('.')
    return byLang[lang].get(ns)?.has(rest.join('.')) ?? false
  }
  // 先整体扫一遍：`#[cfg(test)] mod x;` 声明的测试模块文件不检查
  const scanned = files.map((file) => ({ file, ...rustCode(readFileSync(file, 'utf8')) }))
  const testFiles = new Set(scanned.flatMap((f) => testModuleFiles(f.file, f.testModules)))
  const seen = new Set()
  for (const { file, code } of scanned) {
    if (testFiles.has(file)) continue
    for (const { key, line } of rustKeys(code)) {
      seen.add(key)
      const missing = LANGS.filter((l) => !lookup(l, key))
      if (missing.length === 0) continue
      const msg = `Rust 下发的 key ${key} 在 ${missing.join(' / ')} 中没有文案`
      if (KNOWN_MISSING.has(key))
        warnings.push({ file: rel(file), line, msg: `${msg}（已登记的待补项）` })
      else problems.push({ file: rel(file), line, msg })
    }
  }
  for (const k of KNOWN_MISSING) {
    if (!seen.has(k))
      warnings.push({
        file: 'scripts/check-i18n.mjs',
        msg: `KNOWN_MISSING 中的 ${k} 已不再出现，请删除这一项`,
      })
  }
  return seen.size
}

// ---------------------------------------------------------------------------
// ③ 硬编码文案

/** 不需要翻译的计量单位（`{p95} ms`、`{w}px`、`{n} KB`）：数值后面跟的单位不算文案 */
const UNITS = /\b(?:ms|px|pt|em|rem|fps|Hz|kHz|dpi|ppi|[kKMG]i?B)\b/g

/** 判断一段文本是否像用户可见的文案：含中日韩文字，或含两个字母以上的英文单词（HTML 实体与计量单位除外） */
export function looksLikeCopy(s) {
  const t = s
    .replace(/&[a-z]+;|&#\d+;/g, ' ')
    .replace(UNITS, ' ')
    .trim()
  return /[\u3040-\u30ff\u3400-\u9fff\uac00-\ud7af]/.test(t) || /\b[A-Za-z]{2,}\b/.test(t)
}

/** 值是用户可见文案的 JSX 属性（组件的 `label` 属性按惯例也是文案） */
const COPY_ATTRS = new Set([
  'title',
  'aria-label',
  'aria-description',
  'aria-placeholder',
  'aria-roledescription',
  'aria-valuetext',
  'placeholder',
  'alt',
  'label',
])
/** 之后可以直接出现表达式（JSX、正则）的关键字 */
const EXPR_KEYWORDS = new Set([
  'return',
  'case',
  'default',
  'yield',
  'await',
  'typeof',
  'void',
  'delete',
  'throw',
  'else',
  'do',
  'in',
  'of',
])
/** 表达式里「被渲染出来」的字符串字面量前后的运算符：`a ? 'x' : 'y'`、`a && 'x'`、`a ?? 'x'`、`a || 'x'` */
const RENDER_BEFORE = new Set(['?', ':', '&&', '||', '??'])
const RENDER_AFTER = new Set([':', '||', '??'])
const OPERATOR_CHARS = /[=!<>&|?:+\-*%^~]/
const TAG_NAME = /[A-Za-z_$][\w$.:-]*/y

/**
 * 在 TSX 代码（已去掉注释）里找出 JSX 中的字面文案，返回 `[{ index, text, label }]`（label 用于报错信息）。
 * 一个小的递归下降扫描器：
 * - JS 代码里的 `<`：前一个记号不是值（标识符、数字、右括号），且后面紧跟标签名或 `>` 时按 JSX 元素解析——
 *   `Array<T>`、`fn<T>(…)`、`a < b` 都不会被当成标签；`<T,>(x) => …` 这类解析失败的回退为普通代码；
 * - 开始标签：逐个解析属性，`{…}` 的值按 JS 表达式配对花括号（`onClick={() => a > b && go()}` 里的 `>` 不会
 *   提前结束标签）；文案属性（COPY_ATTRS）的字符串值，以及值表达式里被渲染出来的字符串（`open ? "Open" :
 *   "Close"`、`label ?? 'Untitled'`；`t('key')` 这样作为参数的不算）记为文案；
 * - 子节点：开始标签之后、`{…}` 之后、子元素的结束标签之后，到下一个 `<` 或 `{` 之前的每一段文字都是 JSX 文本
 *   （`<p>{n} items left</p>`、`<span>{count} 条记录</span>`）；`{…}` 里被渲染出来的字符串同样算文案。
 *   JSX 文本里不可能出现 `>` 或 `}`（TS 会报错）：遇到时说明把类型参数误当成了标签，整个元素回退。
 */
export function jsxCopy(code) {
  const found = []
  const n = code.length
  const isSpace = (c) => c === ' ' || c === '\n' || c === '\t' || c === '\r'
  const skipSpace = (i) => {
    while (i < n && isSpace(code[i])) i++
    return i
  }
  const nameAt = (i) => {
    TAG_NAME.lastIndex = i
    return TAG_NAME.exec(code)?.[0] ?? ''
  }
  /** 普通字符串字面量：返回结束位置（引号之后）；行内没有闭合时返回 -1 */
  const stringEnd = (i) => {
    const q = code[i]
    for (let j = i + 1; j < n; j++) {
      if (code[j] === '\\') j++
      else if (code[j] === q) return j + 1
      else if (code[j] === '\n') return -1
    }
    return -1
  }
  /** 模板字面量：返回 `{ end, text }`（text 为静态部分，`${…}` 换成空格）；没有闭合时返回 null */
  const template = (i) => {
    let text = ''
    for (let j = i + 1; j < n; j++) {
      const c = code[j]
      if (c === '\\') {
        text += code[j + 1] ?? ''
        j++
      } else if (c === '`') return { end: j + 1, text }
      else if (c === '$' && code[j + 1] === '{') {
        const close = expression(j + 2, null)
        if (close < 0) return null
        text += ' '
        j = close
      } else text += c
    }
    return null
  }
  /** 把一段表达式顶层记号里「被渲染出来」的字符串记为文案 */
  const collectRendered = (tokens, what) => {
    tokens.forEach((t, k) => {
      if (t.type !== 'str') return
      const before = tokens[k - 1]
      const after = tokens[k + 1]
      const okBefore = !before || (before.type === 'op' && RENDER_BEFORE.has(before.value))
      const okAfter = !after || (after.type === 'op' && RENDER_AFTER.has(after.value))
      if (okBefore && okAfter)
        found.push({ index: t.index, text: t.value, label: `${what}… '${t.value.trim()}' …}` })
    })
  }
  /**
   * 扫描 JS 代码，返回与之配对的 `}` 的位置（`close` 为 false 时扫到文件末尾、返回 n）；括号不配对时返回 -1。
   * `tokens` 不为 null 时收集顶层（括号深度为 0）的记号
   */
  function expression(i, tokens, close = true) {
    const stack = []
    let value = false
    const top = () => tokens !== null && stack.length === 0
    while (i < n) {
      const c = code[i]
      if (isSpace(c)) {
        i++
        continue
      }
      if (c === '"' || c === "'") {
        const end = stringEnd(i)
        if (end < 0) {
          // 行内没有闭合的引号（回退后的 JSX 文本里的撇号等）：当作普通字符跳过
          i++
          value = false
          continue
        }
        if (top()) tokens.push({ type: 'str', index: i + 1, value: code.slice(i + 1, end - 1) })
        i = end
        value = true
      } else if (c === '`') {
        const t = template(i)
        if (!t) return close ? -1 : n
        if (top()) tokens.push({ type: 'str', index: i + 1, value: t.text })
        i = t.end
        value = true
      } else if (c === '/' && !value && code[i - 1] !== '<' && regexEnd(code, i) > 0) {
        i = regexEnd(code, i)
        value = true
        if (top()) tokens.push({ type: 'value' })
      } else if (c === '(' || c === '[' || c === '{') {
        stack.push(c)
        i++
        value = false
      } else if (c === ')' || c === ']' || c === '}') {
        if (stack.length === 0) {
          if (close && c === '}') return i
          if (close) return -1
        } else stack.pop()
        i++
        value = true
        if (top()) tokens.push({ type: 'value' })
      } else if (
        c === '<' &&
        !value &&
        (code[i + 1] === '>' || /[A-Za-z_$]/.test(code[i + 1] ?? ''))
      ) {
        const end = element(i)
        if (end > 0) {
          if (top()) tokens.push({ type: 'value' })
          i = end
          value = true
        } else {
          if (top()) tokens.push({ type: 'op', value: '<' })
          i++
          value = false
        }
      } else if (/[\w$]/.test(c)) {
        let j = i + 1
        while (j < n && /[\w$]/.test(code[j])) j++
        const word = code.slice(i, j)
        value = !EXPR_KEYWORDS.has(word)
        if (top()) tokens.push({ type: 'value', value: word })
        i = j
      } else if (OPERATOR_CHARS.test(c)) {
        let j = i + 1
        while (j < n && OPERATOR_CHARS.test(code[j]) && !(code[j] === '<' && code[j - 1] !== '<'))
          j++
        if (top()) tokens.push({ type: 'op', value: code.slice(i, j) })
        i = j
        value = false
      } else {
        // `.`、`,`、`;`、`@` 等
        if (top()) tokens.push({ type: 'op', value: c })
        i++
        value = false
      }
    }
    return close ? -1 : n
  }
  /** 解析 `i` 处（`<`）开始的 JSX 元素，返回元素之后的位置；不是合法的 JSX 时撤销本元素的发现并返回 -1 */
  function element(i) {
    const mark = found.length
    const fail = () => {
      found.length = mark
      return -1
    }
    let j = i + 1
    let name = ''
    if (code[j] === '>') j++
    else {
      name = nameAt(j)
      if (!name) return fail()
      j += name.length
      for (;;) {
        j = skipSpace(j)
        const c = code[j]
        if (c === '/' && code[j + 1] === '>') return j + 2
        if (c === '>') {
          j++
          break
        }
        if (c === '{') {
          // {...props}
          const end = expression(j + 1, null)
          if (end < 0) return fail()
          j = end + 1
          continue
        }
        const attr = nameAt(j)
        if (!attr) return fail()
        j = skipSpace(j + attr.length)
        if (code[j] !== '=') continue
        j = skipSpace(j + 1)
        const v = code[j]
        if (v === '"' || v === "'") {
          // JSX 属性字符串没有转义、可以跨行
          const end = code.indexOf(v, j + 1)
          if (end < 0) return fail()
          if (COPY_ATTRS.has(attr)) {
            const text = code.slice(j + 1, end)
            found.push({ index: j + 1, text, label: `${attr}="${text.trim()}"` })
          }
          j = end + 1
        } else if (v === '{') {
          const tokens = COPY_ATTRS.has(attr) ? [] : null
          const end = expression(j + 1, tokens)
          if (end < 0) return fail()
          if (tokens) collectRendered(tokens, `${attr}={`)
          j = end + 1
        } else if (v === '<') {
          const end = element(j)
          if (end < 0) return fail()
          j = end
        } else return fail()
      }
    }
    // 子节点
    for (;;) {
      let k = j
      while (k < n && code[k] !== '<' && code[k] !== '{') k++
      if (k >= n) return fail()
      const text = code.slice(j, k)
      if (text.includes('>') || text.includes('}')) return fail()
      const lead = text.length - text.trimStart().length
      if (text.trim()) found.push({ index: j + lead, text: text.trim(), label: text.trim() })
      j = k
      if (code[j] === '{') {
        const tokens = []
        const end = expression(j + 1, tokens)
        if (end < 0) return fail()
        collectRendered(tokens, '{')
        j = end + 1
      } else if (code[j + 1] === '/') {
        let e = skipSpace(j + 2)
        const closing = nameAt(e)
        if (closing !== name) return fail()
        e = skipSpace(e + closing.length)
        if (code[e] !== '>') return fail()
        return e + 1
      } else {
        const end = element(j)
        if (end < 0) return fail()
        j = end
      }
    }
  }
  expression(0, null, false)
  return found
}

/** 直接写进 DOM 的用户可见字面量（Overlay 等原生 TS + DOM 代码） */
const DOM_COPY = [
  /\.(?:textContent|innerText|title|placeholder|alt|ariaLabel|ariaDescription|ariaPlaceholder)\s*=\s*(?:'([^'\n]*)'|"([^"\n]*)"|`([^`$]*)`)/g,
  /\.setAttribute\(\s*['"](?:title|placeholder|alt|aria-label|aria-description|aria-placeholder)['"]\s*,\s*(?:'([^'\n]*)'|"([^"\n]*)"|`([^`$]*)`)/g,
  /\bcreateTextNode\(\s*(?:'([^'\n]*)'|"([^"\n]*)"|`([^`$]*)`)/g,
]

/** 硬编码文案：`jsx` 为真时另查 JSX 文本、文案属性与 JSX 里被渲染的字符串（TSX）；DOM 赋值两种文件都查 */
export function hardcodedCopy(text, { jsx = true } = {}) {
  if (/i18n-ignore-file/.test(text)) return []
  const lines = text.split('\n')
  const code = stripJsComments(text)
  const hits = []
  if (jsx) {
    for (const h of jsxCopy(code)) if (looksLikeCopy(h.text)) hits.push(h)
  }
  for (const re of DOM_COPY) {
    for (const m of code.matchAll(re)) {
      const v = m[1] ?? m[2] ?? m[3] ?? ''
      if (looksLikeCopy(v)) hits.push({ index: m.index, label: m[0] })
    }
  }
  const out = []
  for (const h of hits.toSorted((a, b) => a.index - b.index)) {
    const line = lineOf(code, h.index)
    if (/i18n-ignore/.test(lines[line - 1] ?? '') || /i18n-ignore/.test(lines[line - 2] ?? ''))
      continue
    out.push({
      line,
      msg: `硬编码文案「${h.label.replace(/\s+/g, ' ').slice(0, 60)}」：用户可见文字必须经 t()`,
    })
  }
  return out
}

function checkHardcoded(problems) {
  const src = join(APP_DIR, 'src')
  const files = walk(src).filter((f) => {
    const r = f
      .slice(src.length + 1)
      .split('\\')
      .join('/')
    return (
      /\.tsx?$/.test(r) &&
      !r.endsWith('.d.ts') &&
      !r.startsWith('shared/ipc/mock/') &&
      !r.startsWith('test/') &&
      !r.includes('/__tests__/') &&
      !/\.test\.tsx?$/.test(r)
    )
  })
  for (const f of files) {
    for (const v of hardcodedCopy(readFileSync(f, 'utf8'), { jsx: f.endsWith('.tsx') }))
      problems.push({ file: rel(f), line: v.line, msg: v.msg })
  }
  return files.length
}

// ---------------------------------------------------------------------------
// 自检

function selfTest() {
  const keysOf = (src) => rustKeys(rustCode(src).code).map((k) => `${k.key}@${k.line}`)
  // 早出现的测试辅助函数只跳过它自己，后面的生产代码照常扫描
  const early = [
    'fn a() {}',
    '#[cfg(test)]',
    '#[allow(dead_code)]',
    'fn helper() -> &\'static str { "errors.platform.test_only" }',
    "pub fn real() -> &'static str {",
    '    "errors.platform.after_helper"',
    '}',
    '#[cfg(test)]',
    'use std::fmt;',
    'const X: [u8; 2] = [1, 2]; fn after_use() -> &\'static str { "errors.hint.after_use" }',
    '#[cfg(all(test, target_os = "macos"))]',
    'mod tests { fn t() { let _ = "errors.platform.in_tests"; if true { } } }',
    'fn tail() -> &\'static str { "errors.hint.tail" }',
  ].join('\n')
  assert.deepEqual(keysOf(early), [
    'errors.platform.after_helper@6',
    'errors.hint.after_use@10',
    'errors.hint.tail@13',
  ])
  // 注释剥离不能吃掉字符串里的 URL；字符字面量与生命周期、原始字符串、嵌套块注释
  const lexed = [
    'let url = "https://example.com/a"; let k = "errors.hint.after_url"; // "errors.hint.in_comment"',
    "let q = '\"'; let s = \"errors.hint.after_quote_char\"; fn f<'a>(x: &'a str) {}",
    'let r = r#"raw "errors.hint.raw" text"#; /* outer /* "errors.hint.nested" */ still */ let z = "errors.hint.z";',
  ].join('\n')
  assert.deepEqual(keysOf(lexed), [
    'errors.hint.after_url@1',
    'errors.hint.after_quote_char@2',
    'errors.hint.raw@3',
    'errors.hint.z@3',
  ])
  // `#[cfg(test)] mod name;` 的模块文件、`#![cfg(test)]` 的整个文件
  const decl = rustCode('#[cfg(test)]\nmod contract_tests;\npub fn x() {}')
  assert.deepEqual(decl.testModules, ['contract_tests'])
  assert.ok(
    testModuleFiles(join('src', 'ipc', 'mod.rs'), decl.testModules).includes(
      join('src', 'ipc', 'contract_tests.rs'),
    ),
  )
  assert.ok(
    testModuleFiles(join('src', 'app.rs'), ['tests']).includes(join('src', 'app', 'tests.rs')),
  )
  assert.deepEqual(keysOf('#![cfg(test)]\nfn t() { "errors.hint.x" }'), [])
  // 硬编码文案：TSX 的 JSX 文本 / 属性 / 被渲染的字符串，TS 的 DOM 赋值；URL 后面的同行代码仍然检查
  const lines = (src, opts) => hardcodedCopy(src, opts).map((v) => v.line)
  const tsx = [
    /* 1 */ 'const u = "https://example.com" ; const el = <p title="Close panel">{t("a")}</p>',
    /* 2 */ 'const ok = <Button aria-label={t("close")} />',
    /* 3 */ 'function f<T>(x: Array<T>) { return x.length < 3 && y > 2 }',
    /* 4 */ '// <p>注释里的文字</p>',
    /* 5 */ 'const row = <span><Icon name="ok" /> 已授权</span>',
    /* 6 */ 'const link = <a href={url}>→ https://api.example.com/v1</a>',
    /* 7 */ 'switch (k) { case "a": return <A /> case "b": return <B /> }',
    /* 8 */ 'const s = <span>保存</span> // i18n-ignore（同一行或下一行的文案都豁免）',
    /* 9 */ 'const s2 = <span>取消</span>',
    // `{…}` 之后、子元素结束标签之后的文字
    /* 10 */ 'const a = <p>{n} items left</p>',
    /* 11 */ "const b = <p><b>{t('a')}</b> trailing words</p>",
    /* 12 */ 'const c = <span>{count} 条记录</span>',
    // 属性表达式里被渲染的字符串；属性值里的 `>` 不会提前结束开始标签
    /* 13 */ 'const d = <button aria-label={open ? "Open menu" : "Close menu"} />',
    /* 14 */ 'const e = <button onClick={() => a > b && go()}>Save now</button>',
    // 不是文案：普通变量赋值、data-* 属性、t() 的参数、比较用的字符串、单位、空白
    /* 15 */ "const label = 'settings'",
    /* 16 */ 'const g = <div data-label="panel" title={t(\'x\')} />',
    /* 17 */ "const h = <p>{state === 'done' ? <Check /> : t('pending')}</p>",
    /* 18 */ "const i = <span>{p95} ms{' '}</span>",
    // 回退：泛型箭头函数不是标签，之后的 JSX 照常检查
    /* 19 */ 'const id = <T,>(x: T) => x; const j = <em>{x} more</em>',
    // 子节点表达式里被渲染的字符串、模板字面量的静态部分、跨行的 JSX 文本
    /* 20 */ "const k = <p>{busy ? 'Loading…' : null}</p>",
    /* 21 */ 'const l = <p>{`${n} files`}</p>',
    /* 22 */ 'const m = (',
    /* 23 */ '  <div>',
    /* 24 */ '    {n}',
    /* 25 */ '    条结果',
    /* 26 */ '  </div>',
    /* 27 */ ')',
  ].join('\n')
  assert.deepEqual(lines(tsx), [1, 5, 6, 10, 11, 12, 13, 13, 14, 19, 20, 21, 25])
  const dom = hardcodedCopy(
    [
      "hint.textContent = '拖动选择区域'",
      "btn.setAttribute('aria-label', `Cancel`)",
      "el.title = t('overlay.cancel')",
      "el.textContent = ''",
      "el.setAttribute('data-state', 'idle')",
      "root.append(document.createTextNode('Esc to cancel'))",
      'badge.textContent = `${w} × ${h}`',
    ].join('\n'),
    { jsx: false },
  )
  assert.deepEqual(
    dom.map((v) => v.line),
    [1, 2, 6],
  )
  // .ts 文件不按 JSX 解析
  assert.deepEqual(lines('const x = a < b && c > d ? "Yes" : "No"', { jsx: false }), [])
  ok('check-i18n 自检通过（词法器、测试代码跳过、硬编码文案规则）')
}

function main() {
  if (process.argv.includes('--self-test')) return selfTest()
  const problems = []
  const warnings = []
  const byLang = loadLocales(problems)
  const { namespaces, keys } = checkLocales(byLang, problems)
  const rustCount = checkRustKeys(byLang, problems, warnings)
  const tsx = checkHardcoded(problems)
  for (const w of warnings) warn(`${w.file}${w.line ? `:${w.line}` : ''}: ${w.msg}`, w.file)
  for (const p of problems) error(p.msg, p.file, p.line)
  if (problems.length) throw new Error(`i18n 检查未通过：${problems.length} 处问题`)
  ok(`locales：${namespaces} 个命名空间、${keys} 个 key，zh-CN / en-US 对齐，占位符一致`)
  ok(
    warnings.length
      ? `Rust 下发的 key：${rustCount} 个；除 ${warnings.length} 处已登记的待补项（见上方告警）外两种语言都有文案`
      : `Rust 下发的 key：${rustCount} 个，两种语言都有文案`,
  )
  ok(`${tsx} 个 TS / TSX 文件中没有硬编码文案`)
}

if (isMain(import.meta.url)) await run(main)
