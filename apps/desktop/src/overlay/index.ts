/**
 * 截图 Overlay 的首帧核心（原生 TS：不引入 React / Motion / Tailwind 运行时）。
 * IPC 一律从 `../shared/ipc/overlay`（窄门面）导入，**不要**导入 `../shared/ipc` 桶或 `commands.ts`。
 *
 * 会话：
 * 1. 先订阅 `overlay:session`，订阅完成后再调 `overlayInit()`（无参数：Rust 按本窗口 label 反查显示器）；
 * 2. 返回 null = 空闲的预建 Overlay（Windows）或原生冻结帧还没装上（macOS），等 `overlay:session` 再 init；
 *    每次新会话先重置全部状态，按 session_id 去重（事件与首次 init 可能指向同一会话）；
 * 3. 首帧：`frozen_native`（macOS）时冻结帧由原生图层显示，页面不画冻结帧；否则先画 `half_url`（1/2 降采样）
 *    → 立即 `overlay_ready`（Rust 随即显示窗口）→ 再换成 `image_url`（全分辨率）。解码一律经 `fetchRgbaBitmap`
 *    （ImageData → createImageBitmap），位图画完即 `close()`；放大镜在全分辨率就绪前隐藏；
 * 4. 暗化层 opacity 0 → 1（颜色 `--dim-overlay` = 黑 45%），`presets.overlayDim`（--dur-slow + --ease-standard，
 *    减少动效时 100 ms）：根节点带上 data-on 时由 CSS 动画播放（会话结束去掉，下次会话重新开始），首帧路径上
 *    不需要读取 / 换算时长；
 * 5. 框选、放大镜、键盘、确认与飞入在交互模块（select.ts，动态加载，见 host.ts 的说明）；
 * 6. 不能出现没有出口的状态（Esc 随时取消）：交互模块加载完之前 Esc 与右键都直接取消；`overlay_init` 失败时
 *    有限次重试；还没有会话时按 Esc 会重新 init，
 *    2 s 内拿到的会话立即取消——macOS 的原生冻结帧在 `overlay_show` 期间就已上屏，而 `overlay_init` 在它返回之前
 *    回 null，这时的 Esc 要留给随后的 `overlay:session`，不能被 null 清掉；
 * 7. 没有 half_url（页面自己画全分辨率冻结帧）时首帧就是全分辨率帧，交互模块直接用它，不再拉取第二次；
 * 8. Rust 结束的会话（全局 Esc、再按截图快捷键、看门狗、commit 后的 done）先发 `overlay:end`：
 *    按 session_id 核对后立即收尾（结束交互、释放冻结帧与位图、去掉 data-on），**不再回调** cancel / done，重复收尾无害；
 * 9. 多屏：指针在本窗口里移动、而它还不是 key（`!document.hasFocus()`）时请求 `overlay_focus`，**每次进入本窗口至多
 *    一次**：请求标记在失去 key（blur）、指针离开本窗口、换会话时复位（A → B → A 回到 A 会再请求；请求了但窗口没成为
 *    key 时，指针离开再回来也会再请求）；请求失败时复位并冷却 500 ms 再允许重试（不会每次指针移动都发一次）。macOS 上
 *    非 key 的 WKWebView 收不到指针移动，那里主要靠原生跟随，这只是补充。
 * 前端只把载荷里的路径交给 `resolveAssetUrl`，不自己拼 `capture/`（资源 URL 的格式由 Rust 决定）；不调用任何窗口 API（Windows 上
 * Overlay 的显示 / 隐藏由 Rust 用 Win32 管理）。会话结束立即释放冻结帧缓冲区与位图（预建的 Overlay 空闲时不留大块内存）。
 */
import './overlay.css'
import {
  fetchRgbaBitmap,
  on,
  overlayCancel,
  overlayCommit,
  overlayDone,
  overlayFocus,
  overlayInit,
  overlayReady,
  resolveAssetUrl,
  type OverlayInit,
} from '../shared/ipc/overlay'
import type { Interaction, OverlayHost } from './host'

interface Live {
  init: OverlayInit
  ac: AbortController
  /** 首帧已上屏（交互模块据此开始拉全分辨率帧） */
  shown?: boolean
  /** 首帧就是全分辨率帧（没有 half_url；macOS 的原生冻结帧同样没有，交互模块按 frozen_native 区分） */
  full: boolean
}

/** 失败只记一条警告：IPC 错误（WispIpcError）自带命令名，帧解码错误自带资源路径，不再另附步骤名（Overlay 入口 ≤ 15 KB） */
const warn = (error: unknown): void => console.warn('overlay', error)

/** 挂载 Overlay；返回卸载函数（页面整个生命周期只挂一次，入口不需要它；测试用来隔离用例） */
export function mountOverlay(root: HTMLElement): () => void {
  const life = new AbortController()
  const { signal } = life
  const frame = document.createElement('canvas')
  const dim = document.createElement('div')
  frame.className = 'ov-frame'
  dim.className = 'ov-dim'
  root.dataset.overlay = ''
  root.replaceChildren(frame, dim)
  const ctx = frame.getContext('2d', { alpha: false })
  let live: Live | null = null
  let ui: Interaction | null = null
  /** 还没有会话时按过 Esc：在这个时刻（performance.now）之前拿到的会话立即取消 */
  let escUntil = 0
  let tries = 0
  /**
   * 这次进入本窗口之后已经请求过成为 key：失去 key、指针离开、换会话、请求失败时复位（文件头第 9 条）
   */
  let focused = false
  /** 请求失败后的冷却：这个时刻（performance.now）之前不再请求 */
  let focusAt = 0

  const finish = (): void => {
    ui?.end()
    if (!live) return
    live.ac.abort()
    live = null
    frame.width = frame.height = 0
    root.removeAttribute('data-on')
  }
  const cancel = (): void => {
    const cur = live
    if (!cur) return
    finish()
    overlayCancel(cur.init.session_id).catch(warn)
  }
  const host: OverlayHost = {
    root,
    signal,
    frame,
    dim,
    cancel,
    commit: overlayCommit,
    done() {
      const cur = live
      finish()
      if (cur) overlayDone(cur.init.session_id).catch(warn)
    },
    load: (url) => fetchRgbaBitmap(resolveAssetUrl(url), live?.ac.signal),
    paint: (b) => {
      if (live) paint(b, live.init.image_size)
    },
  }

  // 交互模块与 init / 拉帧并行加载；加载完之前由这里处理 Esc
  import('./select').then((m) => {
    if (signal.aborted) return
    ui = m.attach(host)
    if (!live) return
    ui.begin(live.init)
    if (live.shown) ui.shown(live.full)
  }, warn)
  addEventListener(
    'keydown',
    (e) => {
      if (e.key !== 'Escape') return
      if (live) {
        if (!ui) cancel()
      } else {
        escUntil = performance.now() + 2000
        init()
      }
    },
    { signal },
  )
  // 右键同样随时取消：交互模块还没加载完（负载高时可能要几百毫秒）时也不能吞掉；加载完之后右键由交互模块的
  // pointerdown 处理，这里只阻止系统菜单
  root.oncontextmenu = (e) => {
    e.preventDefault()
    if (!ui) cancel()
  }
  root.onpointermove = () => {
    if (live && !focused && !document.hasFocus() && performance.now() >= focusAt) {
      focused = true
      overlayFocus(live.init.session_id).catch((error: unknown) => {
        focused = false
        focusAt = performance.now() + 500
        warn(error)
      })
    }
  }
  root.onpointerleave = () => (focused = false)
  addEventListener('blur', () => (focused = false), { signal })

  /** 位图铺满冻结帧 canvas（像素尺寸 = 显示器物理像素，全分辨率帧 1:1 绘制） */
  const paint = (b: ImageBitmap, { width, height }: OverlayInit['image_size']): void => {
    frame.width = width
    frame.height = height
    ctx?.drawImage(b, 0, 0, width, height)
  }

  async function start(init: OverlayInit): Promise<void> {
    if (live?.init.session_id === init.session_id) return
    finish()
    const cur: Live = { init, ac: new AbortController(), full: !init.half_url }
    live = cur
    focused = false
    if (performance.now() < escUntil) {
      escUntil = 0
      return cancel()
    }
    document.documentElement.dataset.reducedMotion = String(init.reduced_motion)
    frame.hidden = init.frozen_native
    ui?.begin(init)
    if (!init.frozen_native) {
      try {
        // host.load 按当前会话（live === cur）的 signal 拉取
        const first = await host.load(init.half_url ?? init.image_url)
        // 解码期间会话已结束 / 被取代：不能再按旧尺寸分配整块 backing store、盖住新会话的冻结帧
        if (live !== cur) return first.close()
        paint(first, init.image_size)
        first.close()
      } catch (error) {
        // 冻结帧拿不到就不能显示 Overlay：取消本次截图，交还焦点
        warn(error)
        if (live === cur) cancel()
        return
      }
    }
    // 首帧就是冻结帧：画完立即 ready，暗化层同时开始渐入（CSS 动画，overlay.css）；
    // 全分辨率帧由交互模块随后拉取
    root.toggleAttribute('data-on', true)
    overlayReady(init.session_id, init.monitor.id).catch(warn)
    cur.shown = true
    ui?.shown(cur.full)
  }

  const init = (): void => {
    overlayInit().then(
      (i) => {
        tries = 0
        if (i && !signal.aborted) void start(i)
      },
      (error: unknown) => {
        warn(error)
        // 有限次重试（100 / 200 / 300 ms），之后靠 Esc 或下一次 overlay:session
        if (++tries < 4 && !signal.aborted) setTimeout(init, 100 * tries)
      },
    )
  }
  // 先订阅、再 init（防竞态）；订阅失败也照常 init（新建的 Overlay 不发事件）
  on('overlay:session', (e) => {
    if (live?.init.session_id !== e.session_id) init()
  })
    .then((unlisten) => signal.addEventListener('abort', unlisten), warn)
    .finally(init)
  on('overlay:end', (e) => {
    if (live?.init.session_id === e.session_id) finish()
  }).then((unlisten) => signal.addEventListener('abort', unlisten), warn)
  return () => {
    finish()
    life.abort()
  }
}
