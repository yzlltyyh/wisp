/**
 * 悬浮剪贴板入口：挂载 FloatingApp（承载剪贴板视图）。独立入口：固定窗口（翻译 / OCR）不必解析剪贴板视图的代码，
 * 悬浮窗也不带翻译 / OCR 视图。
 */
import '../shared/styles/tailwind.css'
import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { startAppearanceSync, whenReady } from '../shared/ipc'
import { FloatingApp } from '../floating'

whenReady(() => {
  createRoot(document.getElementById('root') as HTMLElement).render(
    <StrictMode>
      <FloatingApp />
    </StrictMode>,
  )
  void startAppearanceSync()
})
