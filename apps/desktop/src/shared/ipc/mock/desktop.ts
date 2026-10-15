/**
 * mock 的假桌面：渐变壁纸 + 一个「别的应用」窗口和一段高亮的选区文字，
 * 用来检验面板的半透明材质、阴影与定位（锚点取选区矩形）。只用于透明窗口（panel / pinned / overlay）。
 *
 * 这里模拟的是 Wisp 之外的系统环境，所以直接写颜色字面量，不受「颜色只能用设计令牌」约束（这条规则只约束应用代码）。
 */
import type { Boot } from '../boot'
import type { Rect } from '../types'

const STYLE = /* css */ `
#wisp-mock-desktop { position: fixed; inset: 0; z-index: 0; overflow: hidden; user-select: none;
  font-family: -apple-system, "SF Pro Text", "Segoe UI Variable Text", "Segoe UI", "PingFang SC", "Microsoft YaHei UI", sans-serif; }
#wisp-mock-desktop .wall { position: absolute; inset: 0;
  background:
    radial-gradient(60% 70% at 18% 22%, #f6b8d0 0%, transparent 60%),
    radial-gradient(55% 65% at 82% 18%, #a9c6ff 0%, transparent 62%),
    radial-gradient(70% 70% at 70% 88%, #ffd6a5 0%, transparent 60%),
    radial-gradient(60% 60% at 20% 90%, #b8f0d8 0%, transparent 60%),
    linear-gradient(135deg, #e9e4ff, #dcecff); }
#wisp-mock-desktop[data-theme=dark] .wall {
  background:
    radial-gradient(60% 70% at 18% 22%, #5b2a64 0%, transparent 60%),
    radial-gradient(55% 65% at 82% 18%, #1f3d7a 0%, transparent 62%),
    radial-gradient(70% 70% at 70% 88%, #6a3b1f 0%, transparent 60%),
    radial-gradient(60% 60% at 20% 90%, #134a3a 0%, transparent 60%),
    linear-gradient(135deg, #14121f, #0d1726); }
#wisp-mock-desktop[data-os=windows] .wall {
  background:
    radial-gradient(45% 60% at 50% 62%, #6aa8ff 0%, transparent 70%),
    radial-gradient(70% 70% at 30% 30%, #b9d6ff 0%, transparent 70%),
    linear-gradient(160deg, #dbe9ff, #8fb6f5); }
#wisp-mock-desktop[data-os=windows][data-theme=dark] .wall {
  background:
    radial-gradient(45% 60% at 50% 62%, #1d4f9e 0%, transparent 70%),
    radial-gradient(70% 70% at 30% 30%, #13284d 0%, transparent 70%),
    linear-gradient(160deg, #0a1322, #0f2a52); }
#wisp-mock-desktop .menubar { position: absolute; left: 0; right: 0; top: 0; height: 24px; display: flex; align-items: center;
  gap: 18px; padding: 0 14px; font-size: 13px; color: #1d1d1f; background: rgb(255 255 255 / .45); backdrop-filter: blur(24px) saturate(1.6); }
#wisp-mock-desktop[data-theme=dark] .menubar { color: #f5f5f7; background: rgb(30 30 32 / .45); }
#wisp-mock-desktop .menubar b { font-weight: 600; }
#wisp-mock-desktop .taskbar { position: absolute; left: 0; right: 0; bottom: 0; height: 48px; display: flex; align-items: center;
  justify-content: center; gap: 8px; background: rgb(243 243 243 / .78); backdrop-filter: blur(30px) saturate(1.4);
  border-top: 1px solid rgb(0 0 0 / .06); }
#wisp-mock-desktop[data-theme=dark] .taskbar { background: rgb(32 32 32 / .78); border-top-color: rgb(255 255 255 / .06); }
#wisp-mock-desktop .taskbar i { width: 32px; height: 32px; border-radius: 6px; background: rgb(0 0 0 / .08); }
#wisp-mock-desktop[data-theme=dark] .taskbar i { background: rgb(255 255 255 / .1); }
#wisp-mock-desktop .app { position: absolute; left: 7%; top: 9%; width: min(760px, 70vw); height: min(520px, 70vh);
  border-radius: 12px; overflow: hidden; background: #ffffff; color: #1d1d1f;
  box-shadow: 0 0 0 .5px rgb(0 0 0 / .18), 0 22px 70px rgb(0 0 0 / .28); }
#wisp-mock-desktop[data-os=windows] .app { border-radius: 8px; }
#wisp-mock-desktop[data-theme=dark] .app { background: #1e1e20; color: #e8e8ea;
  box-shadow: 0 0 0 .5px rgb(255 255 255 / .14), 0 22px 70px rgb(0 0 0 / .6); }
#wisp-mock-desktop .titlebar { height: 38px; display: flex; align-items: center; gap: 8px; padding: 0 14px;
  font-size: 13px; color: #6e6e73; border-bottom: 1px solid rgb(0 0 0 / .06); }
#wisp-mock-desktop[data-theme=dark] .titlebar { color: #98989d; border-bottom-color: rgb(255 255 255 / .08); }
#wisp-mock-desktop .titlebar .dots { display: flex; gap: 8px; margin-right: 10px; }
#wisp-mock-desktop .titlebar .dots i { width: 12px; height: 12px; border-radius: 50%; background: #ff5f57; }
#wisp-mock-desktop .titlebar .dots i:nth-child(2) { background: #febc2e; }
#wisp-mock-desktop .titlebar .dots i:nth-child(3) { background: #28c840; }
#wisp-mock-desktop[data-os=windows] .titlebar .dots { display: none; }
#wisp-mock-desktop .doc { padding: 28px 40px; font-size: 15px; line-height: 1.7; }
#wisp-mock-desktop .doc h1 { font-size: 24px; line-height: 1.3; margin: 0 0 14px; font-weight: 650; }
#wisp-mock-desktop .doc p { margin: 0 0 14px; }
#wisp-mock-desktop .doc mark { color: inherit; background: #b3d7ff; border-radius: 2px; }
#wisp-mock-desktop[data-theme=dark] .doc mark { background: #3f638b; }
`

const DOC = `
<h1>Wisp — 设计笔记</h1>
<p>常驻后台的工具首先要安静：空闲时几乎不占 CPU，不弹无关的通知，也不抢走你正在使用的应用的焦点。
<mark id="wisp-mock-selection">Wisp stays out of your way: it idles at near-zero CPU, wakes in milliseconds, and streams the translation right next to your cursor.</mark></p>
<p>The panel appears where your attention already is. Motion is short and purposeful — it explains where things come from and where they go, then gets out of the way.</p>
<p>截图 OCR、划词翻译与剪贴板共用同一套面板外壳；视觉、动效与键盘路由在三端保持一致，平台差异收敛在系统集成层。</p>
`

export interface MockDesktop {
  readonly element: HTMLElement
  /** 模拟选区的矩形（页面坐标）——面板默认锚点 */
  selectionRect(): Rect
  /** 「别的应用」窗口矩形（Overlay 窗口检测用） */
  windowRects(): Rect[]
  /** 工作区（扣除菜单栏 / 任务栏） */
  workArea(): Rect
}

export function mountDesktop(boot: Boot): MockDesktop {
  const style = document.createElement('style')
  style.id = 'wisp-mock-desktop-style'
  style.textContent = STYLE
  document.head.append(style)

  const el = document.createElement('div')
  el.id = 'wisp-mock-desktop'
  el.dataset.os = boot.os
  el.dataset.theme = boot.theme
  el.setAttribute('aria-hidden', 'true')
  const mac = boot.os === 'macos'
  el.innerHTML = `
    <div class="wall"></div>
    ${mac ? '<div class="menubar"><b></b><b>Notes</b><span>File</span><span>Edit</span><span>View</span><span>Window</span></div>' : ''}
    <div class="app"><div class="titlebar"><span class="dots"><i></i><i></i><i></i></span>Notes — design.md</div>
      <div class="doc">${DOC}</div></div>
    ${boot.os === 'windows' ? '<div class="taskbar"><i></i><i></i><i></i><i></i><i></i></div>' : ''}`
  document.body.prepend(el)

  const toRect = (r: DOMRect): Rect => ({ x: r.left, y: r.top, width: r.width, height: r.height })

  return {
    element: el,
    selectionRect() {
      const mark = el.querySelector('#wisp-mock-selection')
      if (!mark) return { x: innerWidth * 0.15, y: innerHeight * 0.25, width: 1, height: 20 }
      // 多行选区：取最后一行（面板出现在选区末尾下方）
      const rects = mark.getClientRects()
      const last = rects[rects.length - 1]
      const first = rects[0]
      if (!first || !last) return toRect(mark.getBoundingClientRect())
      return {
        x: first.left,
        y: first.top,
        width: Math.max(1, first.width),
        height: last.bottom - first.top,
      }
    },
    windowRects() {
      return [...el.querySelectorAll('.app')].map((w) => toRect(w.getBoundingClientRect()))
    },
    workArea() {
      const top = mac ? 24 : 0
      const bottom = boot.os === 'windows' ? 48 : 0
      return { x: 0, y: top, width: innerWidth, height: innerHeight - top - bottom }
    },
  }
}
