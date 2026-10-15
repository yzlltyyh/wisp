/**
 * 固定窗口入口：挂载 PinnedApp（复用翻译 / OCR 视图组件）。
 */
import '../shared/styles/tailwind.css'
import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { startAppearanceSync, whenReady } from '../shared/ipc'
import { PinnedApp } from '../pinned'

whenReady(() => {
  createRoot(document.getElementById('root') as HTMLElement).render(
    <StrictMode>
      <PinnedApp />
    </StrictMode>,
  )
  void startAppearanceSync()
})
