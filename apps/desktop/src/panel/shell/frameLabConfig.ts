/**
 * 面板帧模式实验台的开关（开发者工具）。只有 debug 构建的 Rust 在设置了 `WISP_FRAME_LAB` 时才会
 * 注入 `window.__WISP_FRAME_LAB__`（见 `src-tauri/src/windows/frame_lab.rs`）；生产构建永远是 null，
 * 实验台组件按需动态加载，不进入首屏 chunk。
 */
import { boot } from '../../shared/ipc'

export interface FrameLabConfig {
  /** 卡片着色由谁画：native = 原生着色层（产品采用的做法）；css = CSS 画（对照组） */
  readonly fill: 'native' | 'css'
  /** 在卡片底边画 2 px 品红标记，供录屏逐帧检测底边空白条 */
  readonly marker: boolean
}

declare global {
  interface Window {
    __WISP_FRAME_LAB__?: FrameLabConfig
  }
}

export const frameLab: FrameLabConfig | null =
  boot.dev && typeof window !== 'undefined' ? (window.__WISP_FRAME_LAB__ ?? null) : null
