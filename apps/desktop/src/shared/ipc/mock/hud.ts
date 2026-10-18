/**
 * mock 的开发浮条（HUD）：手动打开各视图、切换场景与 boot 参数。
 * 渲染在 Shadow DOM 里，不受应用样式影响；带 `?scenario=` 时默认隐藏（QA 截图），`?hud=1` 强制显示。
 * 这是开发工具而非产品界面，文案不走 i18n。
 */
import type { Boot, WindowKind } from '../boot'
import type { PanelView } from '../types'
import { SAMPLE, type MockFixtures } from './fixtures'
import type { MockStreams } from './streams'
import type { MockContext, MockEntry, Scenario } from './types'

interface HudOptions {
  boot: Boot
  kind: WindowKind
  ctx: MockContext
  fixtures: MockFixtures
  streams: MockStreams
  listScenarios(): Promise<Scenario[]>
  entryOf(s: Scenario): { entry: MockEntry; label: string }
}

const CSS = /* css */ `
:host { all: initial; }
.hud { position: fixed; left: 12px; bottom: 12px; z-index: 2147483647; display: flex; align-items: center; gap: 6px;
  padding: 6px; border-radius: 12px; font: 500 12px/1 -apple-system, "Segoe UI", system-ui, sans-serif; color: #f5f5f7;
  background: rgb(28 28 30 / .82); backdrop-filter: blur(16px) saturate(1.4);
  box-shadow: 0 0 0 .5px rgb(255 255 255 / .12), 0 8px 24px rgb(0 0 0 / .3); }
.hud.collapsed > :not(.logo) { display: none; }
.logo { width: 24px; height: 24px; display: grid; place-items: center; border-radius: 8px; cursor: pointer;
  background: linear-gradient(135deg, #7c9cff, #c48bff); color: white; font-weight: 700; }
button, select { all: unset; box-sizing: border-box; height: 24px; padding: 0 8px; border-radius: 7px; cursor: pointer;
  color: inherit; background: rgb(255 255 255 / .08); }
button:hover, select:hover { background: rgb(255 255 255 / .16); }
button:focus-visible, select:focus-visible { outline: 2px solid #7c9cff; outline-offset: 1px; }
button[aria-pressed=true] { background: #3b82f6; }
.sep { width: 1px; height: 16px; background: rgb(255 255 255 / .16); margin: 0 2px; }
select { max-width: 220px; line-height: 24px; }
`

const VIEWS: readonly PanelView[] = ['translate', 'ocr', 'clipboard', 'input', 'toast']

export function mountHud(o: HudOptions): void {
  const host = document.createElement('div')
  host.id = 'wisp-mock-hud'
  const shadow = host.attachShadow({ mode: 'open' })
  const style = document.createElement('style')
  style.textContent = CSS
  const bar = document.createElement('div')
  bar.className = 'hud'
  bar.setAttribute('role', 'toolbar')
  bar.setAttribute('aria-label', 'Wisp mock')
  shadow.append(style, bar)
  document.body.append(host)

  const logo = el('div', 'W', { class: 'logo', title: 'Wisp mock（点击折叠）' })
  logo.addEventListener('click', () => bar.classList.toggle('collapsed'))
  bar.append(logo)

  if (o.kind === 'panel') {
    for (const view of VIEWS) {
      bar.append(button(view, () => openView(o, view)))
    }
    bar.append(
      button('hide', () => o.ctx.requestHide('close_button')),
      sep(),
    )
  }

  const q = new URLSearchParams(location.search)
  const toggle = (key: string, value: string, label: string, pressed: boolean) =>
    button(label, () => reloadWith(key, pressed ? null : value), pressed)
  bar.append(
    toggle(
      'theme',
      o.boot.theme === 'dark' ? 'light' : 'dark',
      o.boot.theme === 'dark' ? '☾' : '☀',
      false,
    ),
    button(o.boot.os, () => reloadWith('os', nextOs(o.boot.os))),
    toggle(
      'frame',
      o.boot.frame_mode === 'native' ? 'css' : 'native',
      `frame:${o.boot.frame_mode}`,
      false,
    ),
    toggle('reduced', '1', 'reduced', o.boot.reduce_motion && q.get('reduced') === '1'),
    button(o.boot.ui_language, () =>
      reloadWith('lang', o.boot.ui_language === 'zh-CN' ? 'en-US' : 'zh-CN'),
    ),
  )

  const select = document.createElement('select')
  select.setAttribute('aria-label', 'scenario')
  select.append(new Option('scenario…', ''))
  bar.append(sep(), select)
  const here = entryOfPath()
  void o.listScenarios().then((list) => {
    // 场景列表要动态导入全部场景模块，可能在页面已经销毁之后才返回（单元测试结束、jsdom 环境已拆掉）
    if (typeof document === 'undefined') return
    for (const s of list) {
      if (o.entryOf(s).entry !== here) continue
      select.append(new Option(`${s.id} — ${s.label}`, s.id, false, q.get('scenario') === s.id))
    }
  })
  select.addEventListener('change', () => {
    const s = select.value
    const url = new URL(location.href)
    if (s) url.searchParams.set('scenario', s)
    else url.searchParams.delete('scenario')
    url.searchParams.set('hud', '1')
    location.href = url.toString()
  })
}

function openView(o: HudOptions, view: PanelView): void {
  const f = o.fixtures
  if (view === 'translate') {
    const id = o.streams.newRequestId()
    o.ctx.stream(
      id,
      o.ctx.textStream(SAMPLE.translation, {
        meta: { detected_source_lang: 'en', target_lang: 'zh-CN' },
      }),
    )
    o.ctx.preparePanel(f.payload.translate(id))
  } else if (view === 'ocr') {
    const id = o.streams.newRequestId()
    o.ctx.stream(id, o.ctx.textStream(SAMPLE.ocr, { chunkChars: 3 }))
    o.ctx.preparePanel(f.payload.ocr(id))
  } else {
    o.ctx.preparePanel(f.payload[view]())
  }
}

function entryOfPath(): MockEntry {
  const p = location.pathname
  if (p.endsWith('/settings.html')) return 'settings'
  if (p.endsWith('/pinned.html')) return 'pinned'
  if (p.endsWith('/overlay.html')) return 'overlay'
  return 'panel'
}

function nextOs(os: string): string {
  return os === 'macos' ? 'windows' : os === 'windows' ? 'linux' : 'macos'
}

function reloadWith(key: string, value: string | null): void {
  const url = new URL(location.href)
  if (value === null) url.searchParams.delete(key)
  else url.searchParams.set(key, value)
  location.href = url.toString()
}

function el(tag: string, text: string, attrs: Record<string, string> = {}): HTMLElement {
  const node = document.createElement(tag)
  node.textContent = text
  for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v)
  return node
}

function button(label: string, onClick: () => void, pressed?: boolean): HTMLButtonElement {
  const b = document.createElement('button')
  b.type = 'button'
  b.textContent = label
  if (pressed !== undefined) b.setAttribute('aria-pressed', String(pressed))
  b.addEventListener('click', onClick)
  return b
}

function sep(): HTMLElement {
  return el('span', '', { class: 'sep', 'aria-hidden': 'true' })
}
