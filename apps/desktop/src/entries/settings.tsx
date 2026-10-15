/**
 * 设置 / 引导入口：`boot.label === 'onboarding'` 时挂载 OnboardingApp，否则 SettingsApp
 * （不依赖 URL query）；两者互相 lazy()，各自只加载自己的代码。
 */
import '../shared/styles/tailwind.css'
import { lazy, StrictMode, Suspense } from 'react'
import { createRoot } from 'react-dom/client'
import { boot, startAppearanceSync, whenReady } from '../shared/ipc'

const SettingsApp = lazy(() => import('../settings').then((m) => ({ default: m.SettingsApp })))
const OnboardingApp = lazy(() =>
  import('../onboarding').then((m) => ({ default: m.OnboardingApp })),
)

whenReady(() => {
  const App = boot.label === 'onboarding' ? OnboardingApp : SettingsApp
  createRoot(document.getElementById('root') as HTMLElement).render(
    <StrictMode>
      <Suspense fallback={null}>
        <App />
      </Suspense>
    </StrictMode>,
  )
  void startAppearanceSync()
})
