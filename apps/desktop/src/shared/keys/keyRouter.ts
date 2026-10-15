/**
 * 键盘路由。
 *
 * - 在 `window` 的 keydown **捕获阶段**统一处理，先于页面里的输入框，避免快捷键被吞掉；
 * - IME 组合输入期间（`isComposing` 或 keyCode 229）直接放行，选字时按 Enter / 数字不会误触发；
 * - 作用域分三层，依次尝试：`overlay`（打开的菜单 / 弹层 / 确认条）> `view`（当前视图）> `global`（⌘W、⌘,、Esc 关闭）；
 *   同层内后注册的优先。Esc 天然遵守「浮层先关、视图可先清空搜索、最后关闭面板」的优先级：
 *   上层的 Esc 处理函数返回 `false` 表示没有处理，继续交给下一层；
 * - `modal` 作用域（打开的菜单）挡住下层：没匹配到的键不再交给视图，而是正常送到浮层自己的元素；
 * - 绑定默认在输入框里不生效（Esc 除外），需要时写 `allowInInput: true`（例如 ⌘1–9、⌘P）；
 * - 匹配成功默认 `preventDefault` + `stopPropagation`，`preventDefault: false` 时只调用处理函数。
 *
 * `Mod` 在 macOS 上是 ⌘，在其他平台是 Ctrl；组合写法见 combo.ts。
 */
import { boot } from '../ipc/boot'
import { matchesEvent, parseCombo, type ParsedCombo } from './combo'

export interface KeyContext {
  /** 焦点是否在可编辑元素（input / textarea / contenteditable）里 */
  inInput: boolean
  /** 事件目标 */
  target: EventTarget | null
}

export interface KeyBinding {
  /** 例如 `Mod+KeyC`、`Escape`、`Shift+ArrowDown`、`Mod+Shift+Digit1` */
  combo: string
  when?: (ctx: KeyContext) => boolean
  allowInInput?: boolean
  /** 默认 true */
  preventDefault?: boolean
  /** 按住不放时的重复事件是否触发，默认 true */
  repeat?: boolean
  /** 返回 `false` 表示没有处理，交给下一个作用域 */
  run: (event: KeyboardEvent, ctx: KeyContext) => boolean | void
}

export type KeyLayer = 'global' | 'view' | 'overlay'

export interface ScopeOptions {
  layer?: KeyLayer
  /** 挡住下层作用域（菜单打开时） */
  modal?: boolean
}

interface CompiledBinding {
  parsed: ParsedCombo
  escape: boolean
  binding: KeyBinding
}

interface ScopeEntry {
  readonly scope: string
  readonly layer: KeyLayer
  readonly modal: boolean
  readonly seq: number
  /** 读取最新的绑定（useKeyScope 用 ref 提供，渲染时不必反复注册） */
  readonly bindings: () => readonly KeyBinding[]
  compiled: { source: readonly KeyBinding[]; list: CompiledBinding[] } | null
}

const LAYER_ORDER: Record<KeyLayer, number> = { overlay: 0, view: 1, global: 2 }

const scopes: ScopeEntry[] = []
let seq = 0
let installs = 0
let detach: (() => void) | null = null

/** 安装全局路由（引用计数；多次安装只挂一个监听），返回卸载函数 */
export function installKeyRouter(): () => void {
  installs++
  if (installs === 1 && typeof window !== 'undefined') {
    window.addEventListener('keydown', dispatch, { capture: true })
    detach = () => window.removeEventListener('keydown', dispatch, { capture: true })
  }
  let done = false
  return () => {
    if (done) return
    done = true
    installs--
    if (installs === 0) {
      detach?.()
      detach = null
    }
  }
}

/** 注册一个作用域；`bindings` 可以是数组或返回数组的函数。返回注销函数 */
export function registerScope(
  scope: string,
  bindings: readonly KeyBinding[] | (() => readonly KeyBinding[]),
  options: ScopeOptions = {},
): () => void {
  const entry: ScopeEntry = {
    scope,
    layer: options.layer ?? 'view',
    modal: options.modal ?? false,
    seq: ++seq,
    bindings: typeof bindings === 'function' ? bindings : () => bindings,
    compiled: null,
  }
  scopes.push(entry)
  return () => {
    const i = scopes.indexOf(entry)
    if (i >= 0) scopes.splice(i, 1)
  }
}

/** 当前已注册的作用域名（调试 / 测试） */
export function activeScopes(): string[] {
  return ordered().map((s) => s.scope)
}

function ordered(): ScopeEntry[] {
  return [...scopes].sort((a, b) => LAYER_ORDER[a.layer] - LAYER_ORDER[b.layer] || b.seq - a.seq)
}

function compile(entry: ScopeEntry): CompiledBinding[] {
  const source = entry.bindings()
  if (entry.compiled?.source === source) return entry.compiled.list
  const list = source.map((binding) => {
    const parsed = parseCombo(binding.combo, boot.os)
    return { parsed, escape: parsed.code === 'Escape', binding }
  })
  entry.compiled = { source, list }
  return list
}

function isEditable(el: Element | null): boolean {
  if (!el) return false
  if (el instanceof HTMLTextAreaElement) return !el.readOnly && !el.disabled
  if (el instanceof HTMLInputElement) {
    const type = el.type
    return (
      !el.readOnly &&
      !el.disabled &&
      type !== 'checkbox' &&
      type !== 'radio' &&
      type !== 'button' &&
      type !== 'range'
    )
  }
  return el instanceof HTMLElement && el.isContentEditable
}

/** 路由入口（也供测试直接调用） */
export function dispatch(event: KeyboardEvent): void {
  // IME 组合输入期间一律放行
  if (event.isComposing || event.keyCode === 229) return
  if (event.defaultPrevented) return
  const ctx: KeyContext = {
    inInput: isEditable(document.activeElement),
    target: event.target,
  }
  for (const entry of ordered()) {
    for (const { parsed, escape, binding } of compile(entry)) {
      if (!matchesEvent(parsed, event)) continue
      if (event.repeat && binding.repeat === false) continue
      if (ctx.inInput && !escape && !binding.allowInInput) continue
      if (binding.when && !binding.when(ctx)) continue
      if (binding.run(event, ctx) === false) continue
      if (binding.preventDefault !== false) {
        event.preventDefault()
        event.stopPropagation()
      }
      return
    }
    // 打开的菜单挡住下层：没匹配的键交给菜单元素自己处理（方向键、首字母跳转）
    if (entry.modal) return
  }
}
