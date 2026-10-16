/**
 * 窗口启动信息（Boot）。
 *
 * Rust 创建窗口时通过 initialization_script 注入 `window.__WISP_BOOT__`，并在同一段脚本里先给 `<html>`
 * 设置 data-* 属性与 `--accent`，保证 CSS 首帧生效（防止主题闪烁）。本模块读取它、补齐缺省值，
 * 并再次（幂等地）写一遍属性。浏览器 mock 下没有注入脚本，改为从 URL 参数合成：
 * `?label=panel&theme=dark&os=windows&lang=en-US&reduced=1&material=solid&frame=css&accent=%23FF6B6B&font=15&dev=1`
 * 另有 `route=<分组 id | 引导步骤>`（settings / onboarding 的初始路由）与 `anchor=<分组内锚点>`（settings 专用）。
 *
 * `Boot` 的字段定义在 types.ts（与 Rust `wisp_types::Boot` 对应，黄金样例校验键集合）。
 */
import type { AppearanceState, Boot, OsKind, WindowTarget } from './types'

export type { Boot } from './types'

/**
 * mock / 兜底用的阴影边距（css 帧模式）。真实值只来自 Rust 注入的 `Boot.shadow_margin`
 * （windows::sizing::SHADOW_MARGIN），这里仅在没有注入脚本时近似。
 */
const FALLBACK_SHADOW_MARGIN = 24

declare global {
  interface Window {
    __WISP_BOOT__?: Partial<Boot>
  }
}

/** 窗口种类（由 label 推导） */
export type WindowKind = 'panel' | 'settings' | 'onboarding' | 'pinned' | 'overlay'

export function windowKindOf(label: string): WindowKind {
  if (label === 'settings' || label === 'onboarding' || label === 'panel') return label
  // 悬浮剪贴板与固定窗口同属独立的卡片窗口（透明 / 系统圆角、不可选中文字），样式一致
  if (label.startsWith('pinned-') || label === 'clipfloat') return 'pinned'
  if (label.startsWith('overlay-')) return 'overlay'
  return 'panel'
}

/** `pinned-<id>` → `<id>`（`pinned_init` 的参数） */
export function pinnedIdOf(label: string): string | null {
  return label.startsWith('pinned-') ? label.slice('pinned-'.length) : null
}

/** `overlay-<n>` → n */
export function overlayIndexOf(label: string): number | null {
  if (!label.startsWith('overlay-')) return null
  const n = Number(label.slice('overlay-'.length))
  return Number.isInteger(n) ? n : null
}

/** 开发 / mock 专用开关（生产包恒为默认值） */
export interface DevFlags {
  /** `?lang=pseudo`：伪本地化 */
  pseudoLocale: boolean
  /** `?hidden_raf=0`：prepare 到 ready 之间屏蔽 rAF / ResizeObserver */
  hiddenRafBlocked: boolean
  /** `?scenario=<id>` */
  scenario: string | null
}

export const devFlags: DevFlags = readDevFlags()

/** 本窗口的 boot 信息；`appearance:changed` 时由 `applyAppearance` 原地更新 */
export const boot: Boot = readBoot()

applyBootAttributes(boot)

/** 把 boot 写到 `<html>`：data-os / data-theme / data-reduced-motion / data-material / data-frame / data-window / lang / --accent / --shadow-margin */
export function applyBootAttributes(b: Boot): void {
  if (typeof document === 'undefined') return
  const root = document.documentElement
  root.dataset.os = b.os
  root.dataset.theme = b.theme
  root.dataset.reducedMotion = String(b.reduce_motion)
  root.dataset.material = b.material
  root.dataset.frame = b.frame_mode
  root.dataset.window = windowKindOf(b.label)
  root.lang = b.ui_language
  if (b.accent) root.style.setProperty('--accent', b.accent)
  else root.style.removeProperty('--accent')
  root.style.setProperty('--shadow-margin', `${b.shadow_margin}px`)
}

/**
 * 开发者模式切换（`settings:changed` 时调用）：`boot.dev = developer_mode || debug_build`。
 * 不是响应式的——组件在下一次渲染（例如下一次 panel:prepare）时读到新值
 */
export function applyDeveloperMode(developerMode: boolean): void {
  boot.dev = developerMode || boot.debug_build
}

/** `appearance:changed` → 更新 boot 并重写属性（所有窗口共用） */
export function applyAppearance(a: AppearanceState): void {
  boot.theme = a.theme
  boot.accent = a.accent
  boot.reduce_motion = a.reduce_motion
  boot.reduce_transparency = a.reduce_transparency
  boot.material = a.material
  boot.font_size = a.font_size
  boot.os = a.os
  boot.ui_language = a.ui_language
  applyBootAttributes(boot)
}

// ---------------------------------------------------------------------------

function readBoot(): Boot {
  const injected = typeof window === 'undefined' ? undefined : window.__WISP_BOOT__
  const base = defaultBoot()
  if (injected) return { ...base, ...injected }
  if (__WISP_MOCK__) return { ...base, ...bootFromQuery() }
  return base
}

function defaultBoot(): Boot {
  const os = detectOs()
  return {
    label: labelFromPath(),
    os,
    os_version: '',
    theme: media('(prefers-color-scheme: dark)') ? 'dark' : 'light',
    accent: '',
    reduce_motion: media('(prefers-reduced-motion: reduce)'),
    reduce_transparency: media('(prefers-reduced-transparency: reduce)'),
    material: 'solid',
    frame_mode: os === 'macos' ? 'native' : 'css',
    shadow_margin: os === 'macos' ? 0 : FALLBACK_SHADOW_MARGIN,
    font_size: 14,
    ui_language: detectLanguage(),
    dev: false,
    debug_build: false,
    route: null,
  }
}

/** mock：URL 参数 → Boot 字段（未给出的保持缺省值） */
function bootFromQuery(): Partial<Boot> {
  const q = query()
  const out: Partial<Boot> = {}
  const label = q.get('label')
  if (label) out.label = label
  const os = q.get('os')
  if (os === 'macos' || os === 'windows' || os === 'linux') {
    out.os = os
    out.frame_mode = os === 'macos' ? 'native' : 'css'
  }
  const route = q.get('route')
  if (route) out.route = routeFromQuery(label ?? labelFromPath(), route, q.get('anchor'))
  const theme = q.get('theme')
  if (theme === 'light' || theme === 'dark') out.theme = theme
  const lang = q.get('lang')
  if (lang === 'zh-CN' || lang === 'en-US') out.ui_language = lang
  const reduced = q.get('reduced')
  if (reduced !== null) out.reduce_motion = reduced === '1' || reduced === 'true'
  const material = q.get('material')
  if (material === 'native' || material === 'solid') out.material = material
  else out.material = (out.os ?? detectOs()) === 'macos' ? 'native' : 'solid'
  const frame = q.get('frame')
  if (frame === 'native' || frame === 'css') out.frame_mode = frame
  const frameMode = out.frame_mode ?? (detectOs() === 'macos' ? 'native' : 'css')
  out.shadow_margin = frameMode === 'native' ? 0 : FALLBACK_SHADOW_MARGIN
  const accent = q.get('accent')
  if (accent && /^#[0-9a-f]{6}$/i.test(accent)) out.accent = accent
  const font = Number(q.get('font'))
  if (Number.isInteger(font) && font >= 12 && font <= 18) out.font_size = font
  const dev = q.get('dev')
  if (dev !== null) out.dev = dev === '1' || dev === 'true'
  return out
}

/** mock：`?route=`（+ settings 的 `&anchor=`）→ WindowTarget（settings 为分组 id，onboarding 为步骤序号） */
function routeFromQuery(label: string, route: string, anchor: string | null): WindowTarget | null {
  if (label === 'onboarding') {
    const step = Number(route)
    return Number.isInteger(step) && step >= 0 ? { kind: 'onboarding', step } : null
  }
  if (label === 'settings') {
    return anchor
      ? { kind: 'settings', section: route, anchor }
      : { kind: 'settings', section: route }
  }
  return null
}

function readDevFlags(): DevFlags {
  if (!__WISP_MOCK__) return { pseudoLocale: false, hiddenRafBlocked: false, scenario: null }
  const q = query()
  return {
    pseudoLocale: q.get('lang') === 'pseudo',
    hiddenRafBlocked: q.get('hidden_raf') === '0',
    scenario: q.get('scenario'),
  }
}

function query(): URLSearchParams {
  return new URLSearchParams(typeof location === 'undefined' ? '' : location.search)
}

/** 入口文件名推导 label（mock / 兜底）：panel.html → panel；pinned.html → pinned-mock；overlay.html → overlay-0 */
function labelFromPath(): string {
  const path = typeof location === 'undefined' ? '' : location.pathname
  if (path.endsWith('/settings.html')) return 'settings'
  if (path.endsWith('/pinned.html')) return 'pinned-mock'
  if (path.endsWith('/overlay.html')) return 'overlay-0'
  return 'panel'
}

function detectOs(): OsKind {
  const ua = typeof navigator === 'undefined' ? '' : navigator.userAgent
  if (/Windows/i.test(ua)) return 'windows'
  if (/Mac OS X|Macintosh/i.test(ua)) return 'macos'
  return 'linux'
}

function detectLanguage(): 'zh-CN' | 'en-US' {
  const lang = typeof navigator === 'undefined' ? '' : navigator.language
  return lang.toLowerCase().startsWith('zh') ? 'zh-CN' : 'en-US'
}

function media(q: string): boolean {
  return typeof matchMedia === 'function' && matchMedia(q).matches
}
