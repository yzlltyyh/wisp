/**
 * scripts/lib/common.mjs：各脚本共用的小工具——仓库路径、日志、GitHub Actions 输出、文件遍历。
 * 只依赖 Node 内置模块（脚本在 pnpm install 之前也要能跑）。
 */
import { appendFileSync, readdirSync, readFileSync, realpathSync, statSync } from 'node:fs'
import { dirname, join, relative, sep } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

/** 当前模块是否是被直接执行的入口（被其他脚本 import 时为 false） */
export function isMain(moduleUrl) {
  const entry = process.argv[1]
  if (!entry) return false
  try {
    return pathToFileURL(realpathSync(entry)).href === moduleUrl
  } catch {
    return false
  }
}

/** 运行入口函数：未捕获的异常打印为错误并以 1 退出（不打印堆栈，除非设置 WISP_DEBUG=1） */
export async function run(main) {
  try {
    await main()
  } catch (e) {
    error(e instanceof Error ? e.message : String(e))
    if (process.env.WISP_DEBUG === '1' && e instanceof Error) console.error(e.stack)
    process.exitCode = 1
  }
}

/** 仓库根目录 */
export const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..', '..')
/** 前端工程目录 */
export const APP_DIR = join(ROOT, 'apps', 'desktop')
/** Tauri 工程目录 */
export const TAURI_DIR = join(APP_DIR, 'src-tauri')

const color = process.stdout.isTTY && !process.env.NO_COLOR
const paint = (code, s) => (color ? `\x1b[${code}m${s}\x1b[0m` : s)
const inActions = process.env.GITHUB_ACTIONS === 'true'

/** 绿色的通过信息 */
export function ok(msg) {
  console.log(`${paint('32', '✓')} ${msg}`)
}
/** 普通信息 */
export function info(msg) {
  console.log(msg)
}
/** 警告：在 GitHub Actions 里同时生成注解 */
export function warn(msg, file) {
  if (inActions) console.log(`::warning${file ? ` file=${file}` : ''}::${escapeAnnotation(msg)}`)
  else console.warn(`${paint('33', '!')} ${msg}`)
}
/** 错误：在 GitHub Actions 里同时生成注解（不退出，由调用方汇总后设置退出码） */
export function error(msg, file, line) {
  if (inActions) {
    const loc = file ? ` file=${file}${line ? `,line=${line}` : ''}` : ''
    console.log(`::error${loc}::${escapeAnnotation(msg)}`)
  } else {
    console.error(`${paint('31', '✗')} ${file ? `${file}${line ? `:${line}` : ''}: ` : ''}${msg}`)
  }
}
function escapeAnnotation(s) {
  return String(s).replaceAll('%', '%25').replaceAll('\r', '%0D').replaceAll('\n', '%0A')
}

/** 写 GitHub Actions 的 step 输出（不在 Actions 里时打印到标准输出） */
export function setOutput(name, value) {
  const line = String(value).includes('\n')
    ? `${name}<<__WISP_EOF__\n${value}\n__WISP_EOF__\n`
    : `${name}=${value}\n`
  if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, line)
  console.log(`${name}=${value}`)
}

/** 追加到 GitHub Actions 的作业摘要（不在 Actions 里时忽略） */
export function appendSummary(markdown) {
  if (process.env.GITHUB_STEP_SUMMARY)
    appendFileSync(process.env.GITHUB_STEP_SUMMARY, `${markdown}\n`)
}

/** 读取 JSON 文件 */
export function readJson(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

/** 相对仓库根的路径（统一用 `/`，用于输出与注解） */
export function rel(path) {
  return relative(ROOT, path).split(sep).join('/')
}

/**
 * 递归列出目录下的文件（返回绝对路径）。`skip(name, absPath)` 返回 true 的目录不进入。
 * 目录不存在时返回空数组。
 */
export function walk(dir, skip = () => false) {
  const out = []
  let entries
  try {
    entries = readdirSync(dir, { withFileTypes: true })
  } catch {
    return out
  }
  for (const e of entries) {
    const p = join(dir, e.name)
    if (e.isDirectory()) {
      if (!skip(e.name, p)) out.push(...walk(p, skip))
    } else if (e.isFile()) {
      out.push(p)
    }
  }
  return out
}

/** 文件大小（字节）；不存在时返回 null */
export function sizeOf(path) {
  try {
    return statSync(path).size
  } catch {
    return null
  }
}

/** 字节数的可读形式：KiB / MiB，保留两位小数 */
export function formatBytes(n) {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(2)} KiB`
  return `${(n / 1024 / 1024).toFixed(2)} MiB`
}

/** 1-based 行号 */
export function lineOf(text, index) {
  let line = 1
  for (let i = 0; i < index && i < text.length; i++) if (text.charCodeAt(i) === 10) line++
  return line
}

/** JS / TS 中可以出现在正则字面量之前的关键字（之后的 `/` 是正则，不是除号） */
const REGEX_AFTER_WORD = new Set([
  'return',
  'typeof',
  'instanceof',
  'in',
  'of',
  'new',
  'delete',
  'void',
  'throw',
  'case',
  'do',
  'else',
  'yield',
  'await',
])

/**
 * 从 `i`（开头的 `/`）读一个正则字面量，返回结束位置（标志之后）；同一行内没有闭合的 `/` 时返回 -1（不是正则）
 */
export function regexEnd(text, i) {
  let inClass = false
  for (let j = i + 1; j < text.length; j++) {
    const c = text[j]
    if (c === '\n') return -1
    if (c === '\\') j++
    else if (inClass) inClass = c !== ']'
    else if (c === '[') inClass = true
    else if (c === '/') {
      let k = j + 1
      while (k < text.length && /[a-z]/i.test(text[k])) k++
      return k
    }
  }
  return -1
}

/**
 * 去掉 JS / TS 的行注释与块注释（JSX 的花括号注释也在内），长度与换行不变（行号、列号仍然准确）。
 * 一个小词法器：字符串、模板字面量（`${…}` 可以嵌套模板）与正则字面量里的 `//`、`/*` 都不是注释；`://` 也不当
 * 注释（JSX 文本里的 URL 不是字符串字面量）。正则与除号按前一个记号区分（标识符、数字、右括号之后是除号；
 * JSX 结束标签 `</` 里的 `/` 两者都不是）。
 * JSX 文本里的撇号（`Don't`）会被当成字符串开头，但普通字符串到行尾为止，影响不会超出这一行。
 */
export function stripJsComments(text) {
  const out = text.split('')
  const n = text.length
  const blank = (from, to) => {
    for (let k = from; k < to; k++) if (out[k] !== '\n') out[k] = ' '
  }
  /** 每层未结束的模板表达式 `${` 处的花括号深度 */
  const templates = []
  let depth = 0
  /** 上一个记号能否结束一个表达式（标识符、数字、字符串、右括号）：决定 `/` 是除号还是正则 */
  let valueBefore = false
  /** 从模板字面量的正文（`from`）读到结束的反引号或下一个 `${`，返回之后的位置 */
  const templateBody = (from) => {
    for (let j = from; j < n; j++) {
      if (text[j] === '\\') j++
      else if (text[j] === '`') return j + 1
      else if (text[j] === '$' && text[j + 1] === '{') {
        templates.push(depth)
        return j + 2
      }
    }
    return n
  }
  let i = 0
  while (i < n) {
    const c = text[i]
    if (c === '/' && text[i + 1] === '/' && text[i - 1] !== ':') {
      const end = text.indexOf('\n', i)
      const to = end < 0 ? n : end
      blank(i, to)
      i = to
    } else if (c === '/' && text[i + 1] === '*') {
      const end = text.indexOf('*/', i + 2)
      const to = end < 0 ? n : end + 2
      blank(i, to)
      i = to
    } else if (c === '/') {
      // `</`：JSX 结束标签，不是正则的开头
      const end = valueBefore || text[i - 1] === '<' ? -1 : regexEnd(text, i)
      i = end < 0 ? i + 1 : end
      valueBefore = end >= 0
    } else if (c === '"' || c === "'") {
      let j = i + 1
      while (j < n && text[j] !== c && text[j] !== '\n') j += text[j] === '\\' ? 2 : 1
      i = j + 1
      valueBefore = true
    } else if (c === '`') {
      i = templateBody(i + 1)
      valueBefore = true
    } else if (c === '}' && templates.length && templates.at(-1) === depth) {
      templates.pop()
      i = templateBody(i + 1)
      valueBefore = true
    } else {
      if (c === '{') depth++
      else if (c === '}') depth--
      if (/[\w$]/.test(c)) {
        let j = i + 1
        while (j < n && /[\w$]/.test(text[j])) j++
        valueBefore = !REGEX_AFTER_WORD.has(text.slice(i, j))
        i = j
        continue
      }
      if (!/\s/.test(c)) valueBefore = c === ')' || c === ']' || c === '}'
      i++
    }
  }
  return out.join('')
}
