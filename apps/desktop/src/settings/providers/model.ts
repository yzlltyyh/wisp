/**
 * 服务与模型的纯函数：模板分组、显示名、host、默认模型的候选项（OCR 只列启用且 vision 为 Yes / Unknown 的模型）、
 * 新建模型条目、按 id 替换 Provider。不依赖 React，便于单测。
 */
import { tKey } from '../../shared/i18n/i18n'
import { isNonChatModel, isUsableModel } from '../../shared/models'
import type {
  ModelEntry,
  ModelRef,
  Protocol,
  Provider,
  ProviderTemplate,
  Settings,
} from '../../shared/ipc/types'

/** 引导页的大卡片：OpenAI 默认 Responses（表单里可切换为 Chat）、Anthropic、Gemini；第四张是「其他服务」 */
export const FEATURED_TEMPLATES = ['openai-responses', 'anthropic', 'gemini'] as const

/** OpenAI 的两个模板（同一张卡片，表单里切换协议） */
export const OPENAI_TEMPLATES: readonly string[] = ['openai', 'openai-responses']

/** 属于引导大卡片的模板（「其他服务」列出其余的） */
export function isFeaturedTemplate(id: string): boolean {
  return OPENAI_TEMPLATES.includes(id) || (FEATURED_TEMPLATES as readonly string[]).includes(id)
}

/** 设置里服务列表为空时的快捷按钮 */
export const QUICK_TEMPLATES: readonly string[] = [...FEATURED_TEMPLATES, 'deepseek']

export const PROTOCOLS: readonly Protocol[] = [
  'openai_chat',
  'openai_responses',
  'anthropic_messages',
  'gemini',
]

/**
 * Provider 是否需要 API Key——与 Rust 的 `orchestrator::plan::provider_needs_key` 同一规则（两端各算一遍，规则改动时两边
 * 同步，用例与 Rust 的 `keyless_providers` 对齐）：有模板时按模板声明的 `key_required`；没有模板（或模板表里查不到）时
 * 回环地址（localhost / *.localhost / 127.x / ::1）不需要。设置页「未设置 Key」的提示、编辑器的 Key 是否必填都按它判断，
 * 不再把「没有模板」一律当成需要（工作正常的本地服务不会挂着橙色警告）
 */
export function providerNeedsKey(
  provider: Pick<Provider, 'base_url' | 'template_id'>,
  template: Pick<ProviderTemplate, 'key_required'> | null | undefined,
): boolean {
  if (provider.template_id && template) return template.key_required
  return !isLoopbackUrl(provider.base_url)
}

/** Base URL 指向回环地址（`plan::is_loopback` 的镜像：去掉 scheme、userinfo、端口与路径后比较 host） */
export function isLoopbackUrl(baseUrl: string): boolean {
  const trimmed = baseUrl.trim()
  const at = trimmed.indexOf('://')
  const rest = at >= 0 ? trimmed.slice(at + 3) : trimmed
  let host = rest.split(/[/?#]/)[0] ?? ''
  const userinfo = host.lastIndexOf('@')
  if (userinfo >= 0) host = host.slice(userinfo + 1)
  host = host.startsWith('[') ? (host.slice(1).split(']')[0] ?? '') : (host.split(':')[0] ?? '')
  host = host.toLowerCase()
  return (
    host === 'localhost' || host.endsWith('.localhost') || host.startsWith('127.') || host === '::1'
  )
}

export function templateName(tpl: Pick<ProviderTemplate, 'name_key' | 'provider'>): string {
  return tKey(tpl.name_key, undefined, tpl.provider.name)
}

/**
 * 新草稿的名称：模板预填的名字由 Rust 写死（部分是中文），按界面语言换成模板的本地化名称；用户改过的名字不动
 */
export function localizedDraft(draft: Provider, tpl: ProviderTemplate): Provider {
  return draft.name === tpl.provider.name ? { ...draft, name: templateName(tpl) } : draft
}

/**
 * 编辑器标题下的副标题：模板名 · host；服务名就是模板名时只显示 host（不重复）
 */
export function providerSubtitle(
  display: string,
  tpl: ProviderTemplate | null,
  host: string,
): string {
  const name = tpl ? templateName(tpl) : null
  return [name && name !== display ? name : null, host].filter(Boolean).join(' · ')
}

/** 服务显示名：用户起的名字，没有时退回 id（Provider.name 缺省为空串） */
export function providerName(p: Pick<Provider, 'name' | 'id'>): string {
  return p.name.trim() || p.id
}

/** base_url 的 host（列表副标题）；解析失败时原样截断 */
export function hostOf(baseUrl: string): string {
  const raw = baseUrl.trim().replace(/#$/, '')
  if (!raw) return ''
  try {
    const url = new URL(/^[a-z][a-z0-9+.-]*:\/\//i.test(raw) ? raw : `https://${raw}`)
    return url.port ? `${url.hostname}:${url.port}` : url.hostname
  } catch {
    return raw.slice(0, 48)
  }
}

/**
 * 服务名的两字母头像（没有品牌标志时的退路，见 logos.tsx 的 ProviderLogo）：两个字母，避免 OpenAI / Ollama / OpenRouter
 * 都是「O」。
 * - 去掉括号里的补充说明（「Ollama（本机）」→「Ollama」）；
 * - 以汉字开头：取第一个汉字（一个汉字的视觉宽度相当于两个字母）；
 * - 西文：取大写的词首与驼峰（「Google Gemini」→ GG、「DeepSeek」→ DS、「OpenAI」→ OA、「LM Studio」→ LM）；
 *   不够两个时取首字母 + 第二个字母（「Ollama」→ Ol、「Groq」→ Gr）。
 */
export function monogram(name: string): string {
  const base = name.replace(/[(（][^)）]*[)）]/g, ' ').trim() || name.trim()
  const chars = Array.from(base)
  const first = chars[0]
  if (!first) return '?'
  if (/\p{Script=Han}/u.test(first)) return first
  const caps: string[] = []
  for (const word of base.split(/[^\p{L}\p{N}]+/u)) {
    const letters = Array.from(word)
    letters.forEach((ch, i) => {
      // 词首（第一个词总算；之后的词首字母大写才算）与词内的驼峰大写
      const isUpper = ch !== ch.toLowerCase()
      if (
        (i === 0 && (caps.length === 0 || isUpper)) ||
        (i > 0 && isUpper && letters[i - 1] === letters[i - 1]?.toLowerCase())
      )
        caps.push(ch.toUpperCase())
    })
  }
  if (caps.length >= 2) return caps.slice(0, 2).join('')
  const second = chars.slice(1).find((ch) => /[\p{L}\p{N}]/u.test(ch)) ?? ''
  return `${first.toUpperCase()}${second.toLowerCase()}`
}

/**
 * 新建服务的名称与已有的重名时加编号（「DeepSeek」已存在 →「DeepSeek 2」），列表里不会出现两行一模一样的服务
 */
export function uniqueName(
  name: string,
  existing: readonly Pick<Provider, 'name' | 'id'>[],
): string {
  const taken = new Set(existing.map((p) => providerName(p)))
  if (!taken.has(name)) return name
  for (let n = 2; ; n++) {
    const candidate = `${name} ${n}`
    if (!taken.has(candidate)) return candidate
  }
}

export function refKey(ref: ModelRef | null): string | null {
  return ref ? `${ref.provider_id}\u0000${ref.model_id}` : null
}

export function parseRefKey(key: string): ModelRef {
  const i = key.indexOf('\u0000')
  return { provider_id: key.slice(0, i), model_id: key.slice(i + 1) }
}

export interface ModelOption {
  value: string
  label: string
  hint: string
  vision: ModelEntry['vision']
}

/** 默认翻译模型的候选：全部 Provider 中已启用的对话模型（嵌入 / 语音 / 图像生成等不能翻译，shared/models.ts） */
export function translateOptions(providers: readonly Provider[]): ModelOption[] {
  return providers.flatMap((p) =>
    p.models.filter(isUsableModel).map((m) => ({
      value: refKey({ provider_id: p.id, model_id: m.id }) as string,
      label: m.label?.trim() || m.id,
      hint: providerName(p),
      vision: m.vision,
    })),
  )
}

/** 默认 OCR 模型的候选：已启用且 vision 为 Yes / Unknown */
export function ocrOptions(providers: readonly Provider[]): ModelOption[] {
  return translateOptions(providers).filter((o) => o.vision !== 'no')
}

/** 默认值指向的模型是否还在候选里（被删除 / 停用 / 改成不支持图片时提示） */
export function defaultIsValid(ref: ModelRef | null, options: readonly ModelOption[]): boolean {
  return ref === null || options.some((o) => o.value === refKey(ref))
}

export function newModelEntry(id: string): ModelEntry {
  return {
    id,
    label: null,
    vision: 'unknown',
    vision_source: null,
    reasoning: false,
    enabled: true,
    missing_upstream: false,
    deprecated: false,
    context_tokens: null,
    caps: {
      temperature: null,
      max_tokens_field: null,
      thinking_param: null,
      effort_param: null,
      stream_options: null,
      include_obfuscation: null,
      anthropic_thinking_disabled: null,
      anthropic_efforts: null,
    },
    reasoning_effort: null,
  }
}

/** 在当前设置的 providers 里按 id 替换（不存在时追加）；数组整体替换是 Merge Patch 的语义 */
export function replaceProvider(settings: Settings, next: Provider): Provider[] {
  const list = settings.providers
  const i = list.findIndex((p) => p.id === next.id)
  if (i < 0) return [...list, next]
  return list.map((p, k) => (k === i ? next : p))
}

/**
 * 新服务保存后自动补上缺失的默认模型（已有默认值时不动），只在已启用的**对话**模型里选（嵌入、语音、图像生成等跳过）：
 * - 翻译：优先取这次测试连接实际探测成功的模型（`preferred`，即 `ProviderTestReport.model_used`），否则取第一个；
 * - OCR：`preferred` 确认支持图片时取它，否则取第一个确认支持图片的，再退回第一个 vision 为 Unknown 的。
 * 返回需要写入的 defaults 补丁（没有变化时为 null）。
 */
export function fillDefaults(
  settings: Settings,
  provider: Provider,
  preferred: string | null = null,
): Settings['defaults'] | null {
  const usable = provider.models.filter(isUsableModel)
  const probed = preferred === null ? undefined : usable.find((m) => m.id === preferred)
  const next = { ...settings.defaults }
  let changed = false
  const first = probed ?? usable[0]
  if (!next.translate && first) {
    next.translate = { provider_id: provider.id, model_id: first.id }
    changed = true
  }
  if (!next.ocr) {
    const vision =
      (probed?.vision === 'yes' ? probed : undefined) ??
      usable.find((m) => m.vision === 'yes') ??
      usable.find((m) => m.vision === 'unknown')
    if (vision) {
      next.ocr = { provider_id: provider.id, model_id: vision.id }
      changed = true
    }
  }
  return changed ? next : null
}

/**
 * 第一次拿到模型列表（服务原来一个模型都没有）时：嵌入 / 语音 / 图像生成等非对话模型默认不启用
 * （Rust 的合并规则是「首次拉取 ≤ 20 个全部启用」），之后的刷新原样采用 Rust 的合并结果
 */
export function initialModels(
  previous: readonly ModelEntry[],
  fetched: ModelEntry[],
): ModelEntry[] {
  if (previous.length > 0) return fetched
  return fetched.map((m) => (m.enabled && isNonChatModel(m.id) ? { ...m, enabled: false } : m))
}

/** `{WorkspaceId}` 一类需要用户替换的占位片段（通义的业务空间端点） */
export function placeholderRange(url: string): [number, number] | null {
  const m = /\{[A-Za-z]+\}/.exec(url)
  return m ? [m.index, m.index + m[0].length] : null
}
