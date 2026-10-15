/**
 * 模型 id 的分类（与 `crates/wisp-llm/src/vision.rs` 的 `NON_CHAT` / `bare_id` 同一套规则，units 测试逐条比对 Rust 源码）。
 *
 * 嵌入、语音、图像生成、审核等模型不能用来翻译或识别：`/models` 常把它们排在前面（Ollama 的 `nomic-embed-text`、
 * OpenAI 的 `text-embedding-3-small`），首次拉取全部启用后会被当成默认模型，第一次翻译就失败。
 * 默认模型的自动选择、翻译 / OCR 的候选列表与面板的模型菜单都跳过它们。
 */

/** 明显不是对话模型的 id（与 Rust `NON_CHAT` 逐字一致；改一边要同步另一边） */
const NON_CHAT =
  /(?:^|[-_:])(?:tts|transcribe|whisper|realtime|audio|embed|embedding|rerank|reranker|moderation|live|image-gen|dall-e|imagen|veo|sora|gpt-image|flash-image|pro-image|computer-use)(?:[^a-z]|$)|^(?:babbage|davinci|text-embedding|omni-moderation)/

/** 小写并去掉到最后一个 `/` 为止的 provider 前缀（`openai/gpt-5` → `gpt-5`） */
export function bareModelId(id: string): string {
  const lower = id.trim().toLowerCase()
  const i = lower.lastIndexOf('/')
  return i < 0 ? lower : lower.slice(i + 1)
}

/** 是否明显不是对话模型（嵌入、语音、图像生成、审核……） */
export function isNonChatModel(id: string): boolean {
  return NON_CHAT.test(bareModelId(id))
}

/** 能用来翻译 / 识别的模型：已启用且是对话模型 */
export function isUsableModel(m: { id: string; enabled: boolean }): boolean {
  return m.enabled && !isNonChatModel(m.id)
}
