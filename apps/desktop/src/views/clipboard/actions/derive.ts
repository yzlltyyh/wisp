/**
 * 从操作注册表派生各个界面：菜单项（分组之间加分隔线）、按钮组、键位表（按键位层）、提示条与显示的快捷键。
 * 都是纯函数：执行经调用方给的 `exec`（视图里由 runner 提供，测试里可以换成记录器）。
 */
import type { LucideIcon } from 'lucide-react'
import type { OsKind } from '../../../shared/ipc/types'
import type { ViewHost } from '../../../panel/types'
import type { TFunction } from '../../../shared/i18n/useT'
import { canonicalCombo } from '../../../shared/keys/combo'
import type { KeyBinding, KeyLayer } from '../../../shared/keys/keyRouter'
import type { MenuEntry, MenuPage } from '../../../shared/ui'
import { ariaKeyshortcut } from '../keys'
import { ACTIONS, actionById } from './registry'
import type {
  ActionCtx,
  ActionEntry,
  ClipAction,
  ClipKeyLayer,
  HintItem,
  HintMode,
  KeySpec,
  MenuSurface,
} from './types'

/** 执行一个操作（runner 在这里补上运行环境；返回 false 表示没有处理） */
export type ExecAction = (a: ClipAction<unknown>, c: ActionCtx, arg?: unknown) => boolean | void

/** 键位层 → 键盘路由的作用域（名称、层级、是否模态） */
export const KEY_SCOPES: Readonly<
  Record<
    ClipKeyLayer,
    { readonly scope: string; readonly layer: KeyLayer; readonly modal: boolean }
  >
> = {
  list: { scope: 'clipboard', layer: 'view', modal: false },
  preview: { scope: 'clipboard-preview', layer: 'overlay', modal: false },
  // 清空确认是模态的：没匹配的键不再交给视图层（另有一层把剩下的按键整个吞掉，见 ClipboardView）
  clear: { scope: 'clipboard-clear', layer: 'overlay', modal: true },
  confirm: { scope: 'clipboard-delete', layer: 'overlay', modal: false },
  // 芯片条：←/→ / ↩ / Esc 在这一层，其余按键照常落到列表（↑↓ 换条目即退出芯片条）
  chips: { scope: 'clipboard-chips', layer: 'overlay', modal: false },
  // 命名条：焦点在输入框里，没匹配的键交给输入框本身（打字），不落到列表的快捷键上
  rename: { scope: 'clipboard-rename', layer: 'overlay', modal: true },
}

const DEFAULT_LAYERS: readonly ClipKeyLayer[] = ['list']

export function keyLayers(k: KeySpec<unknown>): readonly ClipKeyLayer[] {
  return k.layers ?? DEFAULT_LAYERS
}

/** 这个平台上的组合串；null 表示不绑定 */
export function comboOf(k: KeySpec<unknown>, os: OsKind): string | null {
  return typeof k.combo === 'function' ? k.combo(os) : k.combo
}

/**
 * 操作是否适用于上下文：宿主、作用对象数（arity）与操作自己的条件。不适用时界面不列出，快捷键拦下但不执行
 */
export function applies(a: ClipAction<unknown>, c: ActionCtx): boolean {
  if (a.hosts && !a.hosts.includes(c.host)) return false
  switch (a.arity) {
    case 'one':
      if (!c.target) return false
      break
    case 'many':
      if (c.marked.length === 0) return false
      break
    case 'one_or_many':
      if (!c.target && c.marked.length === 0) return false
      break
    case 'none':
      break
  }
  return a.when ? a.when(c) : true
}

/** 一层里可显示的快捷键（去重，按声明顺序）：第一个就是菜单 / 提示条上显示的那个 */
export function displayCombos(
  a: ClipAction<unknown>,
  c: ActionCtx,
  layer: ClipKeyLayer = 'list',
): string[] {
  const out: string[] = []
  for (const k of a.keys ?? []) {
    if (!keyLayers(k).includes(layer)) continue
    if (k.display === false || (k.display && !k.display(c))) continue
    const combo = comboOf(k, c.os)
    if (combo && !out.includes(combo)) out.push(combo)
  }
  return out
}

export function displayCombo(
  a: ClipAction<unknown>,
  c: ActionCtx,
  layer: ClipKeyLayer = 'list',
): string | null {
  return displayCombos(a, c, layer)[0] ?? null
}

/** 某个参数对应的组合串（⌘n 角标：第 n 条） */
export function comboForArg(
  a: ClipAction<unknown>,
  os: OsKind,
  arg: unknown,
  layer: ClipKeyLayer = 'list',
): string | null {
  for (const k of a.keys ?? []) {
    if (k.arg === arg && keyLayers(k).includes(layer)) return comboOf(k, os)
  }
  return null
}

export function iconOf(a: ClipAction<unknown>, c: ActionCtx): LucideIcon | undefined {
  return a.iconFor ? a.iconFor(c) : a.icon
}

export function labelOf(a: ClipAction<unknown>, c: ActionCtx, t: TFunction): string {
  return a.label ? t(a.label(c)) : a.id
}

/**
 * 某个界面上的操作（按注册表顺序），每项带上执行入口。多选时菜单只列能作用于多条的操作（只作用于一条的「预览」
 * 「命名…」等不出现：菜单作用于整个选择，与 Finder 一致）
 */
export function actionEntries(
  surface: MenuSurface,
  c: ActionCtx,
  t: TFunction,
  exec: ExecAction,
): ActionEntry[] {
  const out: ActionEntry[] = []
  const many = c.marked.length > 0
  for (const a of ACTIONS) {
    const spec = a.surfaces?.[surface]
    if (!spec || !applies(a, c) || (spec.when && !spec.when(c))) continue
    if (many && a.arity === 'one') continue
    out.push({
      id: a.id,
      group: a.group,
      label: labelOf(a, c, t),
      icon: iconOf(a, c),
      shortcut: displayCombo(a, c),
      disabled: a.enabled ? !a.enabled(c) : false,
      danger: a.danger === true,
      checked: a.checked?.(c),
      submenu: a.page !== undefined,
      run: () => {
        exec(a, c, spec.arg)
      },
    })
  }
  return out
}

/** 子页的返回行与无障碍名称（调用方给出文案，保持这里与 i18n 无关） */
export interface PageChrome {
  readonly back: string
  readonly backAria: string
  readonly label: string
}

/**
 * 某个操作的子页（「粘贴为…」、「提取」）：每项执行注册表里的另一个操作；`back` 回到上一页。操作不适用或没有子页时
 * 为 null
 */
export function subPage(
  a: ClipAction<unknown>,
  c: ActionCtx,
  t: TFunction,
  exec: ExecAction,
  chrome: PageChrome,
  back: () => void,
): MenuPage | null {
  if (!a.page || !applies(a, c)) return null
  const items: MenuEntry[] = a.page(c, t).map((it) => ({
    id: it.key,
    label: it.label,
    icon: it.icon,
    ...(it.hint ? { hint: it.hint } : {}),
    onSelect: () => {
      exec(actionById(it.action) as ClipAction<unknown>, c, it.arg)
    },
  }))
  return {
    key: a.id,
    label: chrome.label,
    items,
    back: { label: chrome.back, ariaLabel: chrome.backAria, onSelect: back },
  }
}

/** 菜单项：分组之间加分隔线 */
export function menuEntries(
  surface: MenuSurface,
  c: ActionCtx,
  t: TFunction,
  exec: ExecAction,
): MenuEntry[] {
  const out: MenuEntry[] = []
  let group: string | null = null
  for (const e of actionEntries(surface, c, t, exec)) {
    if (group !== null && e.group !== group) out.push({ type: 'separator', id: `sep-${e.group}` })
    group = e.group
    out.push({
      id: e.id,
      label: e.label,
      icon: e.icon,
      ...(e.shortcut ? { shortcut: e.shortcut } : {}),
      ...(e.disabled ? { disabled: true } : {}),
      ...(e.danger ? { danger: true } : {}),
      ...(e.checked !== undefined ? { checked: e.checked } : {}),
      ...(e.submenu ? { submenu: true } : {}),
      onSelect: e.run,
    })
  }
  return out
}

/** 悬浮窗工具栏的按钮 */
export function toolbarButtons(c: ActionCtx, t: TFunction, exec: ExecAction): ActionEntry[] {
  return actionEntries('toolbar', c, t, exec)
}

/** 菜单打开时：让菜单里列出的操作做准备 */
export function prepareSurface(
  surface: MenuSurface,
  c: ActionCtx,
  prepare: (a: ClipAction<unknown>) => void,
): void {
  for (const a of ACTIONS) {
    const spec = a.surfaces?.[surface]
    if (spec && a.prepare && applies(a, c) && (!spec.when || spec.when(c))) prepare(a)
  }
}

/**
 * 一个键位层的绑定。同一组合的快捷键合成一条绑定：按声明顺序取第一个条件满足的执行；按住的重复事件只交给允许
 * 重复的键，其余拦下但不执行；操作不适用（例如列表为空时的 ⌘P）同样拦下，不落到 WebView 的默认行为。
 * `ctxFor` 在按键时才构造上下文（读最新的 store）
 */
export function keyBindings(
  layer: ClipKeyLayer,
  os: OsKind,
  host: ViewHost,
  ctxFor: (repeat: boolean) => ActionCtx,
  exec: ExecAction,
): KeyBinding[] {
  const byCombo = new Map<
    string,
    { combo: string; specs: { a: ClipAction<unknown>; k: KeySpec<unknown> }[] }
  >()
  for (const a of ACTIONS) {
    if (a.hosts && !a.hosts.includes(host)) continue
    for (const k of a.keys ?? []) {
      if (k.owner === 'shell' || !keyLayers(k).includes(layer)) continue
      const combo = comboOf(k, os)
      if (!combo) continue
      const key = canonicalCombo(combo, os)
      const slot = byCombo.get(key)
      if (slot) slot.specs.push({ a, k })
      else byCombo.set(key, { combo, specs: [{ a, k }] })
    }
  }
  return [...byCombo.values()].map(({ combo, specs }) => ({
    combo,
    // 焦点一直在搜索框里：绑定都要在输入框里生效
    allowInInput: true,
    when: () => {
      const c = ctxFor(false)
      return specs.some(({ k }) => !k.when || k.when(c))
    },
    run: (e) => {
      const c = ctxFor(e.repeat)
      const hit = specs.find(({ k }) => !k.when || k.when(c))
      if (!hit || (e.repeat && hit.k.repeat !== 'run') || !applies(hit.a, c)) return undefined
      return exec(hit.a, c, hit.k.arg)
    },
  }))
}

/** 提示条的一组提示（按显示顺序）；文案固定，不随选中项变化 */
export function hintItems(mode: HintMode, c: ActionCtx, t: TFunction): HintItem[] {
  const out: (HintItem & { order: number })[] = []
  for (const a of ACTIONS) {
    const h = a.hints?.[mode]
    if (!h) continue
    const combos = displayCombos(a, c, h.layer ?? 'list')
    // 此刻没有可显示的键（例如只选了一条时的连续粘贴）：不占提示条的位置
    if (combos.length === 0) continue
    out.push({
      id: a.id,
      combos: h.all ? combos : combos.slice(0, 1),
      label: t(typeof h.label === 'function' ? h.label(c) : h.label),
      priority: h.priority,
      order: h.order,
    })
  }
  return out
    .sort((x, y) => x.order - y.order)
    .map(({ id, combos, label, priority }) => ({ id, combos, label, priority }))
}

export interface NthShortcut {
  /** ⌘n 角标显示的组合 */
  readonly combo: string | null
  /** 读屏的 `aria-keyshortcuts`：粘贴与纯文本粘贴第 n 条 */
  readonly aria: string | undefined
}

/** 每个平台算一次（行组件逐行取用，保持 O(1)） */
const nthCache = new Map<OsKind, readonly NthShortcut[]>()

/** 第 n 条（1–9）的快速粘贴键；超出范围时为 null */
export function nthShortcut(os: OsKind, n: number): NthShortcut | null {
  let table = nthCache.get(os)
  if (!table) {
    const normal = actionById('paste_nth')
    const plain = actionById('paste_plain_nth')
    table = Array.from({ length: 9 }, (_, i): NthShortcut => {
      const combo = comboForArg(normal, os, i + 1)
      const combos = [combo, comboForArg(plain, os, i + 1)].filter((c): c is string => !!c)
      return {
        combo,
        aria: combos.length > 0 ? combos.map((c) => ariaKeyshortcut(c, os)).join(' ') : undefined,
      }
    })
    nthCache.set(os, table)
  }
  return table[n - 1] ?? null
}
