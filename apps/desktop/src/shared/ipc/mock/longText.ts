/**
 * mock 里的长文本闸门（与 Rust `translate_start` 同一规则）：`confirm_long` 为
 * false、task 为 translate / polish / proofread、原文超过阈值时不发请求，`StartAck.long_text` 带回估算。
 * 估算按 Rust `plan::long_text` 的口径近似（CJK 每字 1 token、拉丁每 4 个字符 1 token；没有速度样本时不给耗时）
 */
import type { LongTextInfo, StartAck, TranslateReq } from '../types'

const GATED = new Set(['translate', 'polish', 'proofread'])

export function longTextGate(req: TranslateReq, threshold: number): LongTextInfo | null {
  if (req.confirm_long || !GATED.has(req.task)) return null
  let chars = 0
  let cjk = 0
  for (const ch of req.text) {
    chars++
    if (/[぀-ヿ㐀-鿿가-힯]/u.test(ch)) cjk++
  }
  if (chars <= threshold) return null
  return {
    chars,
    est_tokens: Math.max(1, Math.round(cjk + (chars - cjk) / 4)),
    est_seconds: null,
  }
}

/** 闸门拦下时的 StartAck：没有请求，`request_id` 为空串 */
export function longTextAck(info: LongTextInfo, width: number | null): StartAck {
  return { request_id: '', width, long_text: info }
}
