/**
 * capability 比 Tauri 的 core:default 更窄：只放行事件监听与窗口拖动 / 关闭。前端误用其他 Tauri 核心 API
 * （window 的 getter / setter、app、path、webview……）只会在真机里被 ACL 拒绝，mock 与 vitest 里照常通过，
 * 所以这里静态扫描全部前端源码，把 `@tauri-apps/*` 的使用钉死在传输层：
 * - `shared/ipc/tauri.ts`：core、event，以及惰性 / 仅类型的 window（`WindowFacade` 的拖动、关闭）；
 * - `shared/ipc/runtime.ts`：core（`isTauri`）；
 * - 其他文件（含 mock、测试）一律不得引用 `@tauri-apps/*`，插件的 JS 包也不得引用。
 * 新增用途时先改 capabilities/default.json，再改这里的白名单。
 */
import { describe, expect, it } from 'vitest'

/** 全部前端源码（`/src/...` → 文本）；用 Vite 的 glob + ?raw（以项目根为基准的绝对模式），不依赖 node API */
const SOURCES = import.meta.glob<string>(['/src/**/*.{ts,tsx,mts}', '!/src/generated/**'], {
  query: '?raw',
  import: 'default',
  eager: true,
})

const ALLOWED: Record<string, readonly string[]> = {
  'shared/ipc/tauri.ts': [
    '@tauri-apps/api/core',
    '@tauri-apps/api/event',
    '@tauri-apps/api/window',
  ],
  'shared/ipc/runtime.ts': ['@tauri-apps/api/core'],
}

const SELF = 'shared/ipc/__tests__/tauri-imports.test.ts'

/** 静态 import / export from / 动态 import() / vi.mock() 里的模块说明符 */
const SPECIFIER =
  /(?:\bfrom\s*|\bimport\s*\(\s*|\bimport\s+|\bvi\.(?:mock|doMock)\s*\(\s*)(['"])(@tauri-apps\/[^'"]+)\1/g

/** `/src/shared/ipc/tauri.ts` → `shared/ipc/tauri.ts` */
function srcPath(key: string): string {
  return key.replace(/^\/src\//, '')
}

function tauriImports(): Map<string, string[]> {
  const found = new Map<string, string[]>()
  for (const [key, text] of Object.entries(SOURCES)) {
    const rel = srcPath(key)
    if (rel === SELF) continue
    const specs = [...text.matchAll(SPECIFIER)].map((m) => m[2] as string)
    if (specs.length > 0) found.set(rel, specs)
  }
  return found
}

describe('@tauri-apps/* 只在传输层使用', () => {
  it('扫描到了全部源码', () => {
    const files = Object.keys(SOURCES).map(srcPath)
    expect(files).toContain('shared/ipc/tauri.ts')
    expect(files).toContain('entries/overlay.ts')
    expect(files.length).toBeGreaterThan(50)
  })

  it('不越过白名单', () => {
    const violations: string[] = []
    for (const [file, specs] of tauriImports()) {
      const allowed = ALLOWED[file] ?? []
      for (const spec of specs) {
        if (!allowed.includes(spec)) violations.push(`${file}: ${spec}`)
      }
    }
    expect(violations).toEqual([])
  })

  it('window 模块在 tauri.ts 里只能惰性或仅类型导入（Overlay 入口只打进 core + event）', () => {
    const text = SOURCES['/src/shared/ipc/tauri.ts'] ?? ''
    expect(text).toContain('@tauri-apps/api/core')
    // 每条静态 import 语句：是否 `import type`、模块说明符
    const statements = [...text.matchAll(/^\s*import\s+(type\s+)?[^'"]*?from\s*['"]([^'"]+)['"]/gm)]
    const windowImports = statements.filter((m) => m[2] === '@tauri-apps/api/window')
    expect(windowImports.length).toBeGreaterThan(0)
    for (const m of windowImports) expect(m[1], m[0]).toBeTruthy()
  })

  it('扫描器能识别各种写法', () => {
    const sample = [
      "import { a } from '@tauri-apps/api/app'",
      'export * from "@tauri-apps/api/path"',
      "const m = await import('@tauri-apps/api/webview')",
      "import '@tauri-apps/plugin-os'",
      "vi.mock('@tauri-apps/api/core')",
    ].join('\n')
    expect([...sample.matchAll(SPECIFIER)].map((m) => m[2])).toEqual([
      '@tauri-apps/api/app',
      '@tauri-apps/api/path',
      '@tauri-apps/api/webview',
      '@tauri-apps/plugin-os',
      '@tauri-apps/api/core',
    ])
  })
})
