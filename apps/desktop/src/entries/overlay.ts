/**
 * 截图 Overlay 入口（只加载令牌的首帧子集）：原生 TS，没有 React / Motion / Tailwind 运行时，
 * 首帧只用 tokens-overlay.css 的变量（暗化色 + 时长 / 曲线；完整的 tokens.css 随交互模块 select.ts 动态注入）；
 * IPC 只从窄门面 `shared/ipc/overlay` 导入（core / event 两个传输模块与 6 条 Overlay 命令），不碰 `shared/ipc` 桶，
 * 否则会预加载与 panel 共享的整个命令 chunk（目标 ≤ 15 KB，统计口径见 scripts/check-size.mjs）。
 */
import '../shared/styles/tokens-overlay.css'
import { whenReady } from '../shared/ipc/overlay'
import { mountOverlay } from '../overlay'

whenReady(() => {
  mountOverlay(document.getElementById('root') as HTMLElement)
})
