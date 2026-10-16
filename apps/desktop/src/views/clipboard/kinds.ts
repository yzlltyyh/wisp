/**
 * 条目类型的特征表：哪些类型能当作纯文本处理、哪些有可以排版的文字正文。操作的可用条件、WebView 代写、预览的等宽
 * 开关与悬停卡片都从这里判断，不在各处各写一份类型列表。`satisfies Record<ClipKind, …>`：新增类型时编译期必须补上。
 */
import type { ClipItemMeta, ClipKind } from '../../shared/ipc/types'

interface KindTraits {
  /** 内容可以表示成纯文本：纯文本粘贴、WebView 代写文本、送翻译（颜色除外） */
  readonly textual: boolean
  /** 有成段的文字正文：全文预览的等宽开关、悬停卡片取正文开头 */
  readonly body: boolean
  /**
   * 可以拖到别的应用里（HTML5 拖拽：WebView 只把 text/plain、text/uri-list、text/html 带出去；图片与文件要原生拖出，
   * 暂不提供）
   */
  readonly drag: boolean
}

const KIND_TRAITS = {
  text: { textual: true, body: true, drag: true },
  rich_text: { textual: true, body: true, drag: true },
  code: { textual: true, body: true, drag: true },
  link: { textual: true, body: false, drag: true },
  color: { textual: true, body: false, drag: true },
  image: { textual: false, body: false, drag: false },
  files: { textual: false, body: false, drag: false },
} as const satisfies Record<ClipKind, KindTraits>

export function isTextual(kind: ClipKind): boolean {
  return KIND_TRAITS[kind].textual
}

export function hasTextBody(kind: ClipKind): boolean {
  return KIND_TRAITS[kind].body
}

export function isDraggable(kind: ClipKind): boolean {
  return KIND_TRAITS[kind].drag
}

/** 文件条目里的文件都已不存在（记录还在）：整行置灰，「在 Finder 中显示」不可用 */
export function allFilesMissing(item: ClipItemMeta): boolean {
  const files = item.files
  return item.kind === 'files' && !!files && files.length > 0 && files.every((f) => !f.exists)
}
