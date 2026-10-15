import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import tailwindcss from '@tailwindcss/vite'
import react from '@vitejs/plugin-react'
import { defineConfig, loadEnv, type Plugin, type UserConfig } from 'vite'

/** 前端入口：四个 HTML，交给 Rolldown 自动拆共享 chunk，不手动切 vendor */
const entries = {
  panel: fileURLToPath(new URL('./panel.html', import.meta.url)),
  settings: fileURLToPath(new URL('./settings.html', import.meta.url)),
  pinned: fileURLToPath(new URL('./pinned.html', import.meta.url)),
  overlay: fileURLToPath(new URL('./overlay.html', import.meta.url)),
}

// `tauri dev` 在移动端 / 远程调试时会注入 TAURI_DEV_HOST；桌面端为空
const devHost = process.env.TAURI_DEV_HOST

/**
 * 开发期 CSP。桌面端 `tauri dev` 直接加载 http://localhost:1420，Tauri 的 dev 代理只在移动端启用，
 * `tauri.conf.json` 的 devCsp / csp 在开发期**都不生效**——内联脚本、data: 字体、blob: worker 之类的违规只会在构建产物里暴露。
 * 因此在 `tauri dev`（Tauri CLI 会给 beforeDevCommand 注入 TAURI_ENV_PLATFORM）下，把 devCsp 以
 * `<meta http-equiv="Content-Security-Policy">` 注入每个 HTML 入口的 <head> 最前面：
 * - script-src 额外只放行 @vitejs/plugin-react 的 Fast Refresh 前导内联脚本（按内容的 sha256，其他内联脚本照样拦截）；
 * - meta 不支持 frame-ancestors（浏览器会告警并忽略），注入前去掉；
 * - 浏览器 mock（`pnpm dev:mock` / 单独 `vite`）与 vitest 不注入。
 * 构建产物仍以 tauri.conf.json 的 csp 为准；用构建产物跑 WISP_SMOKE 冒烟时再收集一次 WebView 控制台的 CSP 违规。
 */
function devCspPlugin(): Plugin {
  const conf = JSON.parse(
    readFileSync(fileURLToPath(new URL('./src-tauri/tauri.conf.json', import.meta.url)), 'utf8'),
  ) as { app: { security: { devCsp: string } } }
  const preamble = react.preambleCode.replace('__BASE__', '/')
  const preambleHash = `'sha256-${createHash('sha256').update(preamble).digest('base64')}'`
  const csp = conf.app.security.devCsp
    .split(';')
    .map((d) => d.trim())
    .filter((d) => d !== '' && !d.startsWith('frame-ancestors'))
    .map((d) => (d.startsWith('script-src ') ? `${d} ${preambleHash}` : d))
    .join('; ')
  return {
    name: 'wisp:dev-csp',
    apply: 'serve',
    transformIndexHtml: {
      // post：在 Fast Refresh 前导与 /@vite/client 都注入之后再 head-prepend，保证 meta 是 <head> 的第一个元素
      // （meta CSP 只约束它之后的内容）
      order: 'post',
      handler: () => [
        {
          tag: 'meta',
          attrs: { 'http-equiv': 'Content-Security-Policy', content: csp },
          injectTo: 'head-prepend',
        },
      ],
    },
  }
}

export default defineConfig(({ command, mode }): UserConfig => {
  const env = loadEnv(mode, process.cwd(), 'VITE_')
  const isTest = mode === 'test' || process.env.VITEST !== undefined
  // 浏览器 mock 的编译期开关：dev server、vitest、`--mode mock`，或显式 VITE_MOCK=1。
  // 用 define 常量而不是运行时判断，生产包里 mock 的动态 import 会被整段删除。
  const mockEnabled =
    command === 'serve' ||
    isTest ||
    mode === 'mock' ||
    env.VITE_MOCK === '1' ||
    process.env.VITE_MOCK === '1'
  const tauriDebug = process.env.TAURI_ENV_DEBUG === 'true'
  const tauriDev = command === 'serve' && !isTest && process.env.TAURI_ENV_PLATFORM !== undefined

  return {
    plugins: [...(tauriDev ? [devCspPlugin()] : []), react(), ...(isTest ? [] : [tailwindcss()])],
    define: {
      __WISP_MOCK__: JSON.stringify(mockEnabled),
    },
    // Tauri CLI 依赖 Rust 的报错输出，不清屏
    clearScreen: false,
    envPrefix: ['VITE_', 'TAURI_ENV_'],
    server: {
      port: 1420,
      strictPort: true,
      host: devHost || false,
      hmr: devHost ? { protocol: 'ws', host: devHost, port: 1421 } : undefined,
      watch: { ignored: ['**/src-tauri/**'] },
    },
    preview: { port: 1420, strictPort: true },
    build: {
      // WKWebView（macOS 14+ / Safari 17）、WebView2（Chromium 常青）、WebKitGTK 4.1；Tailwind 4 要求 Safari 16.4+
      target: ['es2022', 'safari16.4', 'chrome111'],
      outDir: 'dist',
      emptyOutDir: true,
      assetsInlineLimit: 4096,
      reportCompressedSize: false,
      // 目标 WebView（WKWebView / Safari 17+、WebView2、WebKitGTK 2.42+）都原生支持 <link rel="modulepreload">，
      // 不需要约 1 KB 的 polyfill（每个入口都会带上，Overlay 的体积预算只有 15 KB）
      modulePreload: { polyfill: false },
      minify: tauriDebug ? false : 'oxc',
      sourcemap: tauriDebug,
      rolldownOptions: {
        input: entries,
      },
    },
  }
})
