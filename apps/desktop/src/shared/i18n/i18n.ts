/**
 * i18n 运行时：类型化 key + JSON 命名空间，无第三方依赖。
 *
 * - key 形如 `<命名空间>.<路径>`（`errors.hint.check_key`、`styles.natural`），与 Rust 下发的 `*_key` 字段同一套写法；
 * - 每个命名空间是 `src/locales/<lang>/<ns>.json`（zh-CN 为准，en-US 键集合必须一致，测试把关），由使用它的模块
 *   用 `defineMessages(ns, zh, en)` 注册，并通过模块增强把 zh-CN 的类型并入 `I18nCatalog`，`t()` 的 key 因此有类型检查；
 * - 本文件不导入任何 JSON：各窗口只加载自己用到的命名空间（Overlay 的体积预算只有 15 KB）。五个基础命名空间
 *   （common / errors / panel / hotkeys / styles）在 `messages.ts` 注册，`useT` 会确保它们已加载；
 * - 复数：`t(key, { count })` 依次查 `key_<Intl 复数类别>`、`key_other`、`key`；
 * - 插值：`{name}`；缺失参数原样保留占位符；
 * - 语言：`boot.ui_language`（Rust 已把 system 解析为 zh-CN / en-US），随 `appearance:changed` 即时切换；
 *   mock 下 `?lang=pseudo` 为伪本地化（`［ŵíśp…］`），用来发现硬编码字符串。
 *
 * 只依赖 `ipc/boot` 与 hooks/useAppearance（不经 `shared/ipc` 桶）。
 */
import { devFlags } from '../ipc/boot'
import { getAppearance, subscribeAppearance } from '../hooks/useAppearance'
import { pluralCategory } from './plural'

export type Locale = 'zh-CN' | 'en-US'

export type TParams = Record<string, string | number>

/**
 * 命名空间 → zh-CN 文案的类型。各命名空间用模块增强追加：
 * ```ts
 * declare module '../../shared/i18n/i18n' { interface I18nCatalog { translate: typeof zhTranslate } }
 * ```
 */
// oxlint-disable-next-line typescript/no-empty-interface, typescript/no-empty-object-type -- 由各命名空间以模块增强填充
export interface I18nCatalog {}

type Leaves<T, P extends string = ''> = {
  [K in keyof T & string]: T[K] extends string
    ? `${P}${K}`
    : T[K] extends object
      ? Leaves<T[K], `${P}${K}.`>
      : never
}[keyof T & string]

type PluralSuffix = 'zero' | 'one' | 'two' | 'few' | 'many' | 'other'
type PluralBase<K extends string> = K extends `${infer B}_${PluralSuffix}` ? B : never

type NamespaceKeys<N extends keyof I18nCatalog & string> =
  `${N}.${Leaves<I18nCatalog[N]>}` | `${N}.${PluralBase<Leaves<I18nCatalog[N]>>}`

/** 全部已注册命名空间的合法 key（含复数的基础 key） */
export type MessageKey = { [N in keyof I18nCatalog & string]: NamespaceKeys<N> }[keyof I18nCatalog &
  string]

/** 与 zh-CN 同形状、值为字符串的文案树（en-US 必须满足） */
export type MessageShape<T> = {
  readonly [K in keyof T]: T[K] extends string ? string : MessageShape<T[K]>
}

type Flat = ReadonlyMap<string, string>

const catalogs = new Map<string, Record<Locale, Flat>>()

/** 注册一个命名空间（重复注册以最后一次为准，便于 HMR） */
export function defineMessages<M extends object>(ns: string, zh: M, en: MessageShape<M>): void {
  catalogs.set(ns, { 'zh-CN': flatten(zh), 'en-US': flatten(en) })
  version++
  notify()
}

/** 某个命名空间是否已注册 */
export function hasNamespace(ns: string): boolean {
  return catalogs.has(ns)
}

function flatten(tree: object, prefix = '', out = new Map<string, string>()): Map<string, string> {
  for (const [k, v] of Object.entries(tree)) {
    const path = prefix ? `${prefix}.${k}` : k
    if (typeof v === 'string') out.set(path, v)
    else if (v && typeof v === 'object') flatten(v as object, path, out)
  }
  return out
}

// ---------------------------------------------------------------------------
// 当前语言

const pseudo = devFlags.pseudoLocale
let version = 0
const listeners = new Set<() => void>()

function notify(): void {
  for (const l of listeners) l()
}

export function getLocale(): Locale {
  return getAppearance().uiLanguage
}

/** 是否处于伪本地化模式（mock：`?lang=pseudo`） */
export function isPseudoLocale(): boolean {
  return pseudo
}

/** 语言或文案表变化时回调（React 侧见 useT） */
export function subscribeLocale(listener: () => void): () => void {
  listeners.add(listener)
  const unsubscribe = subscribeAppearance(listener)
  return () => {
    listeners.delete(listener)
    unsubscribe()
  }
}

/** 语言与文案表的组合版本号（useSyncExternalStore 的快照） */
export function localeSnapshot(): string {
  return `${getLocale()}#${version}`
}

// ---------------------------------------------------------------------------
// 查找与插值

const warned = new Set<string>()

function lookup(locale: Locale, ns: string, path: string): string | undefined {
  const cat = catalogs.get(ns)
  if (!cat) return undefined
  return cat[locale].get(path) ?? cat['en-US'].get(path) ?? cat['zh-CN'].get(path)
}

function resolve(key: string, params: TParams | undefined): string | undefined {
  const dot = key.indexOf('.')
  if (dot <= 0) return undefined
  const ns = key.slice(0, dot)
  const path = key.slice(dot + 1)
  const locale = pseudo ? 'en-US' : getLocale()
  const count = params?.count
  if (typeof count === 'number') {
    const plural =
      lookup(locale, ns, `${path}_${pluralCategory(count, locale)}`) ??
      lookup(locale, ns, `${path}_other`)
    if (plural !== undefined) return plural
  }
  return lookup(locale, ns, path)
}

const PLACEHOLDER = /\{(\w+)\}/g

export function interpolate(template: string, params: TParams | undefined): string {
  if (!params) return template
  return template.replace(PLACEHOLDER, (whole, name: string) => {
    const v = params[name]
    return v === undefined ? whole : String(v)
  })
}

/** 翻译。key 不存在时返回 key 本身（开发期在控制台提示一次） */
export function t(key: MessageKey, params?: TParams): string {
  return translateKey(key, params)
}

/**
 * 运行期才知道的 key（Rust 下发的 `hint` / `reason_key` / `message_key` 等）。
 * 找不到时返回 `fallback`（默认 null），调用方决定退回什么；给了 `fallback` 时，文案里有 params 没提供的占位符
 * （例如冲突报告缺了 `{action}`）同样返回 `fallback`。
 */
export function tKey(key: string, params?: TParams): string | null
export function tKey(key: string, params: TParams | undefined, fallback: string): string
export function tKey(key: string, params?: TParams, fallback: string | null = null): string | null {
  const msg = resolve(key, params)
  if (msg === undefined) return fallback
  // 有兜底时，参数没给全（文案里还剩 `{action}` 这类占位符）同样退回兜底，不把占位符显示给用户；开发期提示一次
  if (fallback !== null && hasUnfilled(msg, params)) {
    if (import.meta.env.DEV && !warned.has(`${key}#params`)) {
      warned.add(`${key}#params`)
      console.warn(`[i18n] 文案参数不全：${key}`, params)
    }
    return fallback
  }
  return finish(msg, params)
}

/** 文案里有没有 params 没提供的占位符 */
function hasUnfilled(template: string, params: TParams | undefined): boolean {
  for (const [, name] of template.matchAll(PLACEHOLDER)) {
    if (params?.[name as string] === undefined) return true
  }
  return false
}

/** 运行期 key 是否存在 */
export function hasKey(key: string): boolean {
  return resolve(key, undefined) !== undefined
}

function translateKey(key: string, params: TParams | undefined): string {
  const msg = resolve(key, params)
  if (msg === undefined) {
    if (!warned.has(key)) {
      warned.add(key)
      console.warn(`[i18n] 缺少文案：${key}`)
    }
    return key
  }
  return finish(msg, params)
}

function finish(msg: string, params: TParams | undefined): string {
  const text = interpolate(msg, params)
  return pseudo ? pseudoLocalize(text) : text
}

// ---------------------------------------------------------------------------
// 伪本地化：重音字母 + 全角括号 + 约 30% 的长度膨胀，占位符与换行保持原样

const ACCENTS: Record<string, string> = {
  a: 'á',
  c: 'ç',
  e: 'é',
  i: 'í',
  n: 'ñ',
  o: 'ó',
  s: 'ś',
  u: 'ú',
  w: 'ŵ',
  y: 'ý',
  A: 'Á',
  C: 'Ç',
  E: 'É',
  I: 'Í',
  N: 'Ñ',
  O: 'Ó',
  S: 'Ś',
  U: 'Ú',
  W: 'Ŵ',
  Y: 'Ý',
}

/** 占位符（`{name}`，Trans 的节点参数）：原样保留，否则 `Trans` 找不到它们 */
const PSEUDO_KEEP = /(\{\w+\})/

export function pseudoLocalize(text: string): string {
  let out = ''
  let visible = 0
  // split 带捕获组：奇数下标是占位符
  for (const [i, part] of text.split(PSEUDO_KEEP).entries()) {
    if (i % 2 === 1) {
      out += part
      continue
    }
    for (const ch of part) out += ACCENTS[ch] ?? ch
    visible += part.length
  }
  const pad = '·'.repeat(Math.max(1, Math.round(visible * 0.3)))
  return `［${out}${pad}］`
}
