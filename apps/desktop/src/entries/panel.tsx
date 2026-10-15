/**
 * 面板入口：只负责引导和挂载。四个视图在 panel/views.ts 中静态导入。
 */
import '../shared/styles/tailwind.css'
import { StrictMode } from 'react'
import { flushSync } from 'react-dom'
import { createRoot } from 'react-dom/client'
import { startAppearanceSync, whenReady } from '../shared/ipc'
import { PanelApp } from '../panel/shell/PanelApp'
import { hasEarlyPrepare, installPanelEvents } from '../panel/shell/usePanelLifecycle'
import { installClipboardEvents } from '../views/clipboard'

whenReady(() => {
  // 页面级的时间点（每页一次；window lab 的冷唤出测量读取它拆分「页面加载 → ready」）
  performance.mark('wisp:entry')
  // 面板事件在渲染之前就订阅（冷页面上 Rust 的 prepare 不必等 React 第一次渲染与外壳的 effect）；
  // 冷唤出时 Rust 随 Boot 送进来的 prepare 由它当作第一条 prepare 存下（重新加载出来的页面不用）
  installPanelEvents()
  const cold = hasEarlyPrepare()
  // 剪贴板视图的事件在面板启动时就订阅好（ready 之后补发的事件不会先于订阅到达）
  installClipboardEvents()
  const root = createRoot(document.getElementById('root') as HTMLElement)
  const app = (
    <StrictMode>
      <PanelApp />
    </StrictMode>
  )
  // 冷唤出时第一次渲染同步完成（不等调度器的下一个任务）：外壳在模块求值期间就挂载，Boot 里的 prepare 在随后的微任务里
  // 同步提交、测量、panel_ready——早于页面的 load 事件。模块脚本在解析器插入的样式表全部加载之后才执行，此刻测量的布局
  // 已带上样式。预热 / 回收创建的页面照常交给调度器（不阻塞页面加载）
  if (cold) flushSync(() => root.render(app))
  else root.render(app)
  void startAppearanceSync()
})
