/**
 * 文案一致性检查（与 scripts/check-i18n.mjs 的键对齐检查互补，随 `pnpm test` 在 CI 上跑）：
 * - **同一枚举值只有一个显示名**：OCR 模式（lines / paragraphs / markdown / translate）的名字只在 `common.ocr_mode`；
 *   面板菜单、头部、设置的分段控件、识别历史都引用它。其他带这几个键的对象只能是说明文字（白名单）；
 * - **英文按钮与标签用 sentence case**（Fluent / GNOME 规范，macOS 也可接受）：句中（每个分句的第一个词之后）不出现
 *   首字母大写的普通词；专有名词（产品、平台、系统设置里的面板名、按钮上的系统文字）在白名单里；
 * - **不出现开发术语**：用户文案里不说「chip」；
 * - **快捷键只有一种写法**：文案里不手写 `Ctrl + Alt + A` 之类的组合，用 `{combo}` 占位由 formatHotkey 生成；
 * - **术语只有一种叫法**：剪贴板开关一律叫「记录 / recording」（不说「监听」/ listening / watch）；
 *   换模型一律叫「切换模型 / Switch model」（不说「换模型」「换个模型」）；
 *   划词的浮标英文叫 floating button（不是钓鱼用的 bobber）；「提升译文」英文统一为 Improve translation / Improved
 *   （不说 Refine）。
 */
import { describe, expect, it } from 'vitest'

interface NodeFs {
  readFileSync(path: string, encoding: 'utf8'): string
  readdirSync(path: string): string[]
}
const proc = (
  globalThis as unknown as { process: { getBuiltinModule(name: 'node:fs'): NodeFs; cwd(): string } }
).process
const fs = proc.getBuiltinModule('node:fs')
const ROOT = `${proc.cwd()}/src/locales`
const LANGS = ['zh-CN', 'en-US'] as const

type Json = string | { [key: string]: Json }

function load(lang: string): Map<string, Json> {
  const out = new Map<string, Json>()
  for (const file of fs.readdirSync(`${ROOT}/${lang}`)) {
    if (!file.endsWith('.json')) continue
    out.set(
      file.slice(0, -5),
      JSON.parse(fs.readFileSync(`${ROOT}/${lang}/${file}`, 'utf8')) as Json,
    )
  }
  return out
}

/** 全部字符串：[键路径, 值] */
function strings(tree: Map<string, Json>): Array<[string, string]> {
  const out: Array<[string, string]> = []
  const walk = (node: Json, path: string) => {
    if (typeof node === 'string') out.push([path, node])
    else for (const [k, v] of Object.entries(node)) walk(v, `${path}.${k}`)
  }
  for (const [ns, node] of tree) walk(node, ns)
  return out
}

/** 全部对象：[键路径, 对象] */
function objects(tree: Map<string, Json>): Array<[string, Record<string, Json>]> {
  const out: Array<[string, Record<string, Json>]> = []
  const walk = (node: Json, path: string) => {
    if (typeof node === 'string') return
    out.push([path, node])
    for (const [k, v] of Object.entries(node)) walk(v, `${path}.${k}`)
  }
  for (const [ns, node] of tree) walk(node, ns)
  return out
}

const OCR_MODES = ['lines', 'paragraphs', 'markdown']
/** 可以按 OCR 模式分键、但写的是说明（不是名字）的对象 */
const OCR_MODE_DESCRIPTIONS = new Set(['ocr.mode_hint', 'settings.ocr.mode_desc'])

/**
 * 英文里可以在句中首字母大写的词：产品 / 公司 / 平台名、缩写、语言名、系统设置里的面板与按钮名（照系统的写法）、
 * Wisp 设置里的分组名（引用时照导航上的写法）
 */
const PROPER = new Set(
  `Wisp OCR API ID IDs URL OK macOS Windows Linux Wayland X11 XTest XFixes XInput2 AltGr Finder File Explorer Quick Look
  System Settings Keychain Credential Manager Secret Service Accessibility Screen Recording Input Monitoring Privacy
  Security Keyboard Shortcuts Applications Allow Retry Authorization MIT License Mainland China Chat Completions Responses
  Messages OpenAI Anthropic Google Gemini Qwen Alibaba Cloud Model Studio Zhipu GLM Volcengine Ark Doubao Moonshot Kimi
  LM SiliconFlow DeepSeek Ollama OpenRouter English Chinese Japanese Korean Simplified Traditional Tauri Ctrl Alt Shift
  Win Super Cmd Command Option Control Fn Esc Enter Tab Space Return Delete Backspace Right Left QQ iTerm2 Terminal Meta
  Secure Input Bundle Models Translation Natural Base About General Hotkeys Appearance`.split(
    /\s+/,
  ),
)

describe.each(LANGS)('文案一致性 · %s', (lang) => {
  const tree = load(lang)

  it('OCR 模式只有一组显示名（common.ocr_mode），其余按模式分键的只能是说明', () => {
    const names = tree.get('common') as Record<string, Record<string, string>>
    expect(Object.keys(names.ocr_mode ?? {})).toEqual([...OCR_MODES, 'translate'])
    const dup = objects(tree)
      .filter(([path, o]) => OCR_MODES.every((m) => m in o) && path !== 'common.ocr_mode')
      .map(([path]) => path)
      .filter((path) => !OCR_MODE_DESCRIPTIONS.has(path))
    expect(dup).toEqual([])
  })

  it('不出现开发术语「chip」', () => {
    expect(strings(tree).filter(([, v]) => /\bchips?\b/i.test(v))).toEqual([])
  })

  it('剪贴板开关一律叫「记录 / recording」：不说「监听」/ listening / watch', () => {
    // 英文的 watch 只在说剪贴板时禁止（「watch the translation stream in」是正常用法）
    const clip = (path: string, v: string) => path.includes('clipboard') || /clipboard/i.test(v)
    const bad =
      lang === 'zh-CN'
        ? strings(tree).filter(([, v]) => /监听|监视/.test(v))
        : strings(tree).filter(
            ([path, v]) =>
              /\blisten(?:ing|s|ed)?\b/i.test(v) ||
              (clip(path, v) && /\bwatch(?:ed|es|ing)?\b/i.test(v)),
          )
    expect(bad).toEqual([])
  })

  it('换模型这个动作只有一种叫法：「切换模型」/「Switch model」（按钮、菜单与说明句同词）', () => {
    const bad =
      lang === 'zh-CN'
        ? // 「换模型」「换个模型」「换一个模型」「换用…模型」都不用；「切换到支持图片的模型」这种说明照常
          strings(tree).filter(([, v]) => /(?<!切)换(?:个|一个|用)?[^，。；、\s]{0,8}模型/.test(v))
        : strings(tree).filter(([, v]) =>
            /\b(?:another|a different|change(?:s|d)? (?:the )?)model\b|\bswitch(?:ing)? models\b/i.test(
              v,
            ),
          )
    expect(bad).toEqual([])
    // 按钮与菜单项本身
    const flat = new Map(strings(tree))
    const want = lang === 'zh-CN' ? '切换模型' : 'Switch model'
    for (const key of [
      'common.action.switch_model',
      'errors.action.switch_model',
      'ocr.action.switch_model',
      'translate.more.switch_model',
      'translate.a11y.model_menu',
    ])
      expect(flat.get(key), key).toBe(want)
  })

  it('不手写快捷键组合（用 {combo} 占位，由 formatHotkey 生成）', () => {
    const combo = /\b(?:Ctrl|Alt|Shift|Win|Super|Cmd)\s*\+\s*(?:[A-Z0-9]\b|Ctrl|Alt|Shift)/
    expect(strings(tree).filter(([, v]) => combo.test(v))).toEqual([])
  })
})

describe('英文术语', () => {
  it('划词浮标叫 floating button（不是 bobber）；「提升译文」叫 Improve translation / Improved（不是 Refine）', () => {
    const en = strings(load('en-US'))
    expect(en.filter(([, v]) => /\bbobbers?\b/i.test(v))).toEqual([])
    expect(en.filter(([, v]) => /\brefine[ds]?\b/i.test(v))).toEqual([])
    const values = new Map(en)
    expect(values.get('panel.task.refine')).toBe('Improve translation')
    expect(values.get('translate.more.refine')).toBe('Improve translation')
    expect(values.get('translate.status.refined')).toBe('Improved')
  })
})

describe('英文按钮与标签：sentence case', () => {
  it('句中没有首字母大写的普通词（专有名词见白名单）', () => {
    const bad: string[] = []
    for (const [path, value] of strings(load('en-US'))) {
      const text = value.replace(/\{[^}]*\}/g, '').replace(/[’']s\b/g, '')
      // 每个分句的第一个词照常大写：按句读、冒号、破折号、圆点、引号、斜杠、› 切开
      for (const seg of text.split(/[·—–:;(.?!/“”"…›\n]|, /)) {
        const words = seg.match(/[A-Za-z][A-Za-z0-9_-]*/g) ?? []
        for (const w of words.slice(1)) {
          if (/^[A-Z][a-z]/.test(w) && !PROPER.has(w)) bad.push(`${path}: ${w} ← “${value}”`)
        }
      }
    }
    expect(bad).toEqual([])
  })
})
