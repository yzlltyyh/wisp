/**
 * i18n：zh-CN 与 en-US 键集合一致、占位符一致；插值 / 复数 / 运行期 key / 伪本地化；
 * 相对时间；切换语言后组件即时重渲染。错误卡片文案覆盖全部 ErrorCode 与 Rust 下发的 hint / platform key。
 */
import { act, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { getAppearance, setAppearance } from '../../hooks/useAppearance'
import type { ErrorCode } from '../../ipc/types'
import { formatBytes, formatDuration, formatTokens } from '../format'
import { hasKey, interpolate, pseudoLocalize, t, tKey } from '../i18n'
import { CORE_MESSAGES, loadCoreMessages } from '../messages'
import { relativeTime } from '../relativeTime'
import { useT } from '../useT'

loadCoreMessages()
const original = getAppearance()
afterEach(() => setAppearance(original))
const setLang = (uiLanguage: 'zh-CN' | 'en-US') => setAppearance({ ...getAppearance(), uiLanguage })

function flatten(tree: object, prefix = ''): Map<string, string> {
  const out = new Map<string, string>()
  for (const [k, v] of Object.entries(tree)) {
    const path = prefix ? `${prefix}.${k}` : k
    if (typeof v === 'string') out.set(path, v)
    else for (const [kk, vv] of flatten(v as object, path)) out.set(kk, vv)
  }
  return out
}

/**
 * 扫描 Rust 源码（crates/ 与 src-tauri/src）中的 `"errors.hint.<key>"` / `"errors.platform.<key>"` 字面量。
 * vitest 的根目录是 apps/desktop；前端 tsconfig 不带 node 类型，用 Node 22+ 的 `process.getBuiltinModule` 取 fs。
 */
interface NodeFs {
  readFileSync(path: string, encoding: 'utf8'): string
  readdirSync(path: string, options: { recursive: true; encoding: 'utf8' }): string[]
}
function rustErrorKeys(): Set<string> {
  const proc = (
    globalThis as unknown as {
      process: { getBuiltinModule(name: 'node:fs'): NodeFs; cwd(): string }
    }
  ).process
  const fs = proc.getBuiltinModule('node:fs')
  const roots = [`${proc.cwd()}/../../crates`, `${proc.cwd()}/src-tauri/src`]
  const out = new Set<string>()
  for (const root of roots) {
    for (const file of fs.readdirSync(root, { recursive: true, encoding: 'utf8' })) {
      if (!file.endsWith('.rs') || file.includes('target/')) continue
      const text = fs.readFileSync(`${root}/${file}`, 'utf8')
      for (const m of text.matchAll(/"(errors\.(?:hint|platform)\.[a-z0-9_]+)"/g)) {
        const key = m[1] as string
        // `starts_with("errors.platform.wayland_")` 这类前缀断言不是 key
        if (!key.endsWith('_')) out.add(key)
      }
    }
  }
  return out
}

const PLURAL = /_(zero|one|two|few|many|other)$/
const base = (k: string) => k.replace(PLURAL, '')
const placeholders = (s: string) => [...s.matchAll(/\{(\w+)\}/g)].map((m) => m[1]).sort()

describe('文案表', () => {
  for (const [ns, m] of Object.entries(CORE_MESSAGES)) {
    it(`${ns}：zh-CN / en-US 键集合一致（复数变体归并），占位符一致，没有空串`, () => {
      const zh = flatten(m['zh-CN'])
      const en = flatten(m['en-US'])
      expect(new Set([...en.keys()].map(base))).toEqual(new Set([...zh.keys()].map(base)))
      for (const [k, v] of en) {
        const zk = zh.has(k) ? k : `${base(k)}_other`
        const zv = zh.get(zk)
        expect(zv, `${ns}.${k}`).toBeDefined()
        expect(placeholders(v), `${ns}.${k}`).toEqual(placeholders(zv ?? ''))
      }
      // 空串只允许出现在「已取消」的说明（cancelled 不出卡片）
      const empty = [...en, ...zh].filter(
        ([k, v]) => v.trim() === '' && !k.endsWith('cancelled.detail'),
      )
      expect(empty).toEqual([])
    })
  }

  it('错误卡片覆盖全部 ErrorCode', () => {
    const codes: ErrorCode[] = [
      'auth',
      'not_found',
      'no_vision',
      'image_too_large',
      'param_unsupported',
      'too_long',
      'quota',
      'rate_limited',
      'upstream',
      'content_filter',
      'network',
      'timeout',
      'cancelled',
      'protocol',
      'not_configured',
      'keychain',
      'internal',
    ]
    for (const c of codes) expect(hasKey(`errors.code.${c}.title`), c).toBe(true)
  })

  it('Rust 源码里出现的 errors.hint.* / errors.platform.* 字面量都有中英文文案（防止新增 key 漏翻译）', () => {
    const keys = rustErrorKeys()
    // 扫描本身要找得到东西（路径或写法变了时不会悄悄变成空集）
    expect(keys.size).toBeGreaterThan(20)
    expect(keys).toContain('errors.hint.check_key')
    expect(keys).toContain('errors.platform.wayland_selection')
    const missing: string[] = []
    for (const lang of ['zh-CN', 'en-US'] as const) {
      const table = flatten(CORE_MESSAGES.errors[lang])
      for (const k of keys)
        if (!table.get(k.slice('errors.'.length))?.trim()) missing.push(`${lang} ${k}`)
    }
    expect(missing).toEqual([])
  })

  it('Rust 下发的 key 都有文案（wisp-llm hint、wisp-platform reason）', () => {
    for (const k of [
      'errors.hint.check_key',
      'errors.hint.region',
      'errors.hint.check_model',
      'errors.hint.try_v1',
      'errors.hint.pick_vision_model',
      'errors.hint.shrink_image',
      'errors.hint.shorten_text',
      'errors.hint.check_billing',
      'errors.hint.retry_later',
      'errors.hint.retry',
      'errors.hint.switch_model',
      'errors.hint.check_network',
      'errors.hint.adjust_timeout',
      'errors.hint.check_protocol',
      'errors.hint.check_settings',
      'errors.platform.wayland_selection',
      'errors.platform.x11_missing_xtest',
      'errors.platform.hooks_not_compiled',
      'errors.platform.no_permission_model',
      'hotkeys.ext.need_input_monitoring',
      'hotkeys.conflict.static_app',
    ]) {
      expect(hasKey(k), k).toBe(true)
    }
  })
})

describe('运行时', () => {
  it('插值、缺参数时保留占位符', () => {
    expect(interpolate('{a} + {b}', { a: 1 })).toBe('1 + {b}')
    setLang('zh-CN')
    expect(t('errors.code.rate_limited.detail', { n: 12 })).toBe('12 秒后可重试')
  })

  it('复数：en-US 区分 one / other，zh-CN 只有 other', () => {
    setLang('en-US')
    expect(t('common.time.days', { count: 1 })).toBe('1 day ago')
    expect(t('common.time.days', { count: 3 })).toBe('3 days ago')
    expect(formatTokens(1)).toBe('1 token')
    expect(formatTokens(1234)).toBe('1,234 tokens')
    setLang('zh-CN')
    expect(t('common.time.days', { count: 1 })).toBe('1 天前')
  })

  it('运行期 key：找不到时返回 fallback', () => {
    expect(tKey('errors.hint.nope')).toBeNull()
    expect(tKey('errors.hint.nope', undefined, '兜底')).toBe('兜底')
    setLang('en-US')
    expect(tKey('errors.hint.try_v1')).toBe('Try adding or removing /v1 in the endpoint')
  })

  it('运行期 key：给了 fallback 而参数没给全（文案里还剩占位符）时同样返回 fallback，不把「{action}」显示出来', () => {
    setLang('zh-CN')
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    expect(tKey('hotkeys.conflict.self_duplicate', {}, '无法使用')).toBe('无法使用')
    expect(tKey('hotkeys.conflict.self_duplicate', { action: '剪贴板' }, '无法使用')).toBe(
      '已用于「剪贴板」',
    )
    // 没有 fallback：照旧返回（调用方自己处理，例如 ipcErrorText 去掉占位符）
    expect(tKey('hotkeys.conflict.self_duplicate')).toContain('{action}')
    warn.mockRestore()
  })

  it('伪本地化：重音字母 + 全角括号，占位符不变', () => {
    expect(pseudoLocalize('Copy {n}')).toMatch(/^［Çóp\S* \{n\}·+］$/)
    expect(pseudoLocalize('{esc} to cancel · {backspace} to clear')).toMatch(
      /^［\{esc\} \S+ çáñçél · \{backspace\} \S+ çléár·+］$/,
    )
    // Trans 按 \{(\w+)\} 找节点占位符：伪本地化之后仍能全部找到
    const names = [...pseudoLocalize('Press {key} again to delete {name}').matchAll(/\{(\w+)\}/g)]
    expect(names.map((m) => m[1])).toEqual(['key', 'name'])
  })

  it('相对时间：刚刚 / N 分钟 / N 小时 / N 天 / 日期', () => {
    setLang('zh-CN')
    const now = new Date(2026, 9, 10, 12, 0, 0).getTime()
    expect(relativeTime(now - 20_000, now)).toBe('刚刚')
    expect(relativeTime(now - 5 * 60_000, now)).toBe('5 分钟前')
    expect(relativeTime(now - 3 * 3_600_000, now, 'short')).toBe('3 小时')
    expect(relativeTime(now - 2 * 86_400_000, now)).toBe('2 天前')
    expect(relativeTime(new Date(2026, 8, 1).getTime(), now)).toMatch(/9月1日/)
    expect(relativeTime(new Date(2025, 8, 1).getTime(), now)).toMatch(/2025/)
    setLang('en-US')
    expect(relativeTime(now - 5 * 60_000, now, 'short')).toBe('5m')
  })

  it('耗时与字节', () => {
    setLang('en-US')
    expect(formatDuration(850)).toBe('850 ms')
    expect(formatDuration(1240)).toBe('1.2 s')
    expect(formatBytes(1536)).toBe('1.5 KB')
    expect(formatBytes(18_874_368)).toBe('18 MB')
  })

  it('切换语言后组件即时重渲染（appearance:changed）', () => {
    setLang('zh-CN')
    function Label() {
      const tt = useT()
      return <span data-testid="label">{tt('common.action.copy')}</span>
    }
    render(<Label />)
    expect(screen.getByTestId('label').textContent).toBe('复制')
    act(() => setLang('en-US'))
    expect(screen.getByTestId('label').textContent).toBe('Copy')
  })
})

describe('keepQuoted：引用的按钮名整体不拆行', () => {
  it('全角括号 / 引号里的短片段包进 .ui-nobr，文本内容逐字不变；英文与超长片段不处理', async () => {
    const { keepQuoted } = await import('../keepQuoted')
    const text = '在系统设置里打开开关后，点「重新启动 Wisp」，之后按 ⌥S 截图。'
    const { container } = render(<p>{keepQuoted(text)}</p>)
    expect(container.textContent).toBe(text)
    const spans = container.querySelectorAll('.ui-nobr')
    expect([...spans].map((s) => s.textContent)).toEqual(['「重新启动 Wisp」'])
    // 多个片段、圆括号
    const two = render(<p>{keepQuoted('打开「辅助功能」（隐私与安全性）后重试')}</p>).container
    expect([...two.querySelectorAll('.ui-nobr')].map((s) => s.textContent)).toEqual([
      '「辅助功能」',
      '（隐私与安全性）',
    ])
    expect(keepQuoted('Click "Relaunch Wisp" (Privacy)')).toBe('Click "Relaunch Wisp" (Privacy)')
    const long = `「${'很长的引用'.repeat(4)}」`
    expect(keepQuoted(long)).toBe(long)
    const node = <b>x</b>
    expect(keepQuoted(node)).toBe(node)
  })
})
