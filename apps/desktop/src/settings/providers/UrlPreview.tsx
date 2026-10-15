/**
 * 最终请求 URL 的实时预览：输入停顿 150 ms 后调用 Rust 的 `llm_preview_url`（规范化规则只有 wisp-llm 一份）；
 * 显示「POST https://…/chat/completions」；输入被改写时（去掉末尾的接口路径等，`warning`）先说明这一点，以 # 结尾时说明
 * 「不追加版本段」，自动补了 /v1 时说明补全；Gemini 显示当前模型或
 * {model} 占位；URL 里还有 `{WorkspaceId}` 这类占位时提示替换。
 */
import { CornerDownRight, Info } from 'lucide-react'
import { useEffect, useState } from 'react'
import { useT, useTKey } from '../../shared/i18n/useT'
import { api } from '../../shared/ipc'
import type { Protocol, UrlPreview as Preview } from '../../shared/ipc/types'
import { Icon } from '../../shared/ui'
import { placeholderRange } from './model'

/** 编辑时 150 ms 去抖（不是动效时长） */
const PREVIEW_DEBOUNCE_MS = 150

export function useUrlPreview(
  protocol: Protocol,
  baseUrl: string,
  modelId: string | null,
): Preview | null {
  const [preview, setPreview] = useState<Preview | null>(null)
  useEffect(() => {
    let alive = true
    const timer = setTimeout(() => {
      if (!baseUrl.trim()) {
        setPreview(null)
        return
      }
      api
        .llmPreviewUrl(protocol, baseUrl, modelId)
        .then((p) => {
          if (alive) setPreview(p)
        })
        .catch(() => {
          if (alive) setPreview(null)
        })
    }, PREVIEW_DEBOUNCE_MS)
    return () => {
      alive = false
      clearTimeout(timer)
    }
  }, [protocol, baseUrl, modelId])
  return preview
}

/** 把 URL 按「/」切开：每段不折行，只在斜杠之后可以换行（复制出来的文字不变） */
function urlSegments(url: string) {
  const parts = url.split('/')
  return parts.map((part, i) => (
    // oxlint-disable-next-line react/no-array-index-key -- 片段按位置排列，没有稳定 id
    <span key={i}>
      <span className="st-url-seg">{i < parts.length - 1 ? `${part}/` : part}</span>
      {i < parts.length - 1 ? <wbr /> : null}
    </span>
  ))
}

export function UrlPreview({
  protocol,
  baseUrl,
  modelId,
}: {
  protocol: Protocol
  baseUrl: string
  modelId: string | null
}) {
  const t = useT()
  const tKey = useTKey()
  const preview = useUrlPreview(protocol, baseUrl, modelId)
  const placeholder = placeholderRange(baseUrl)
  return (
    <div className="st-url" data-testid="url-preview" aria-live="polite">
      {preview ? (
        <span className="st-url-line">
          <Icon icon={CornerDownRight} size={12} className="st-url-arrow" />
          <span className="st-url-method">{preview.method}</span>
          <span className="st-url-text" data-testid="url-preview-text" title={preview.url}>
            {urlSegments(preview.url)}
          </span>
        </span>
      ) : (
        <span className="st-url-line st-url-empty">{t('settings.providers.url_empty')}</span>
      )}
      {placeholder ? (
        <span className="st-url-note" data-tone="warning">
          <Icon icon={Info} size={12} />
          {t('settings.providers.url_placeholder', {
            name: baseUrl.slice(placeholder[0], placeholder[1]),
          })}
        </span>
      ) : preview?.warning ? (
        // 输入被改写过的地方（去掉了末尾的接口路径、查询串附加到末尾、丢弃了中间的 # 片段）：Rust 下发 i18n key
        <span className="st-url-note" data-tone="warning" data-testid="url-preview-warning">
          <Icon icon={Info} size={12} />
          {tKey(preview.warning, undefined, t('settings.providers.url_appended'))}
        </span>
      ) : preview?.forced ? (
        <span className="st-url-note">
          <Icon icon={Info} size={12} />
          {t('settings.providers.url_forced')}
        </span>
      ) : preview?.version_appended ? (
        <span className="st-url-note">
          <Icon icon={Info} size={12} />
          {t('settings.providers.url_appended')}
        </span>
      ) : null}
    </div>
  )
}
