/**
 * 悬浮剪贴板：常驻在屏幕一角的剪贴板视图，点一条就粘贴到当前应用、不抢焦点。
 *
 * 复用剪贴板视图（`host = 'pinned'`、`sessionId = 0`），宿主差异（占满窗口、不抢焦点、Esc 交还焦点而不是关闭、
 * 只复制时在窗口里轻提示、头部的置顶 / 关闭按钮）都由视图的宿主适配（views/clipboard/host.ts）按 [`ClipFloatContext`]
 * 给出，这里只负责窗口：
 *
 * - **首帧交接**：先订阅 `clipboard:changed` / `clipboard:queue`，再 `clipboard_float_ready` 取初始状态（继承的查询、
 *   筛选、光标与置顶；此后 Rust 把改动同时推给这个窗口），视图同步渲染首帧（继承的状态在布局阶段写入）之后
 *   `clipboard_float_reveal`——Rust 在同一帧显示窗口、隐藏面板，没有空帧；
 * - **窗口控件**：整条头部可拖动（`data-tauri-drag-region`），置顶开关与 ✕ 来自操作注册表的头部按钮；⌘W 关闭（即销毁，
 *   关着时零开销），⌘, 打开设置；
 * - **同步**：指针移入时取回其他设备的改动（Rust 节流），获得焦点时 Rust 自己取。
 *
 * 卡片视觉与固定窗口一致（背景纯色；Linux 透明窗口自己画圆角、描边与阴影，见 floating.css）。
 */
import { X } from 'lucide-react'
import { useEffect, useMemo, useRef, useState } from 'react'
import { api, boot, getTransport } from '../shared/ipc'
import type { ClipFloatInit } from '../shared/ipc/types'
import { useKeyScope } from '../shared/keys/useKeyScope'
import { MotionRoot } from '../shared/motion/MotionRoot'
import { useT } from '../shared/i18n/useT'
import { IconButton } from '../shared/ui'
import type { PayloadOf, ViewHost } from '../panel/types'
import { awaitClipboardView, ClipboardView, installClipboardEvents } from '../views/clipboard'
import { ClipFloatContext, type ClipFloatShell } from '../views/clipboard/host'
import './floating.css'

function closeWindow(): void {
  void api.clipboardFloatClose().catch(() => getTransport().window.close())
}

/** 根元素（窗口内容区）的尺寸：Tauri 里等于窗口尺寸；浏览器 mock 里是模拟窗口的尺寸 */
function rootSize(): { width: number; height: number } {
  const root = document.getElementById('root')
  return {
    width: root?.clientWidth || window.innerWidth,
    height: root?.clientHeight || window.innerHeight,
  }
}

function useWindowFocus(): boolean {
  const [focused, setFocused] = useState(() => document.hasFocus())
  useEffect(() => {
    const on = () => setFocused(true)
    const off = () => setFocused(false)
    window.addEventListener('focus', on)
    window.addEventListener('blur', off)
    return () => {
      window.removeEventListener('focus', on)
      window.removeEventListener('blur', off)
    }
  }, [])
  return focused
}

export function FloatingApp() {
  const [init, setInit] = useState<ClipFloatInit | null>(null)
  const [failed, setFailed] = useState(false)
  useEffect(() => {
    // 先订阅再 ready：ready 之后的每条改动都推给这个窗口，订阅晚了会漏；视图挂上之前到达的改动由视图挂上时补拉
    installClipboardEvents()
    awaitClipboardView()
    let live = true
    api.clipboardFloatReady().then(
      (i) => live && setInit(i),
      () => live && setFailed(true),
    )
    return () => {
      live = false
    }
  }, [])
  return (
    <MotionRoot>
      {init ? <FloatWindow init={init} /> : failed ? <FloatFallback /> : null}
    </MotionRoot>
  )
}

function FloatWindow({ init }: { init: ClipFloatInit }) {
  const focused = useWindowFocus()
  const [onTop, setOnTop] = useState(init.on_top)
  const [size, setSize] = useState(rootSize)
  const margin = boot.shadow_margin

  // 窗口尺寸变化（用户缩放）：只在真的变化时回调，空闲时没有任何计时器
  useEffect(() => {
    const root = document.getElementById('root')
    if (!root) return
    const observer = new ResizeObserver(() => setSize(rootSize()))
    observer.observe(root)
    return () => observer.disconnect()
  }, [])

  // 视图挂载后（继承的状态已在它的布局阶段写入）交接上屏：只做一次
  const revealed = useRef(false)
  useEffect(() => {
    if (revealed.current) return
    revealed.current = true
    void api.clipboardFloatReveal().catch(() => undefined)
  }, [])

  const shell = useMemo<ClipFloatShell>(
    () => ({
      seed: { query: init.query, filter: init.filter, selectedId: init.selected_id },
      onTop,
      // 开关立即响应；失败时回到原状态（与窗口实际状态一致）。新值由 Rust 记为下次打开的默认值
      setOnTop: (wanted) => {
        setOnTop(wanted)
        api.clipboardFloatSetOnTop(wanted).catch(() => setOnTop(!wanted))
      },
      close: closeWindow,
    }),
    [init, onTop],
  )

  useKeyScope(
    'clipfloat',
    [
      { combo: 'Mod+KeyW', allowInInput: true, run: closeWindow },
      {
        combo: 'Mod+Comma',
        allowInInput: true,
        run: () => void api.windowOpen({ kind: 'settings', section: 'clipboard' }),
      },
    ],
    { layer: 'global' },
  )

  const payload = useMemo<PayloadOf<'clipboard'>>(
    () => ({ kind: 'clipboard', ...init.payload }),
    [init.payload],
  )
  const host: ViewHost = 'pinned'
  return (
    // 指针移入只是同步的提示（Rust 节流），不是交互
    // oxlint-disable-next-line jsx-a11y/no-static-element-interactions
    <main
      className="floating"
      data-margin={margin > 0 || undefined}
      data-testid="floating-app"
      onPointerEnter={() => void api.clipboardFloatHover().catch(() => undefined)}
    >
      <div className="floating-card">
        <div className="floating-content">
          <ClipFloatContext value={shell}>
            <ClipboardView
              sessionId={0}
              payload={payload}
              phase="shown"
              focused={focused}
              host={host}
              frame={boot.frame_mode}
              placement="center"
              maxHeight={Math.max(0, size.height - 2 * margin)}
              requestHide={closeWindow}
              switchView={() => undefined}
              activate={() => undefined}
            />
          </ClipFloatContext>
        </div>
      </div>
    </main>
  )
}

/** 初始状态取不到（窗口已在关闭）：只留一个可以关闭的空卡片，不让窗口一片空白 */
function FloatFallback() {
  const t = useT()
  useKeyScope('clipfloat', [{ combo: 'Mod+KeyW', allowInInput: true, run: closeWindow }], {
    layer: 'global',
  })
  return (
    <main
      className="floating"
      data-margin={boot.shadow_margin > 0 || undefined}
      data-testid="floating-app"
    >
      <div className="floating-card">
        <header className="floating-fallback" data-tauri-drag-region>
          <span className="floating-fallback-title" data-tauri-drag-region>
            {t('clipboard.float.unavailable')}
          </span>
          <IconButton icon={X} label={t('clipboard.action.close')} onClick={closeWindow} />
        </header>
      </div>
    </main>
  )
}
