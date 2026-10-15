/**
 * Space 全文预览：占满列表区（列表淡出、保留滚动位置），presets.previewIn 进入与退出（视图的
 * AnimatePresence 播放退出，与列表淡入重叠，没有空白帧）。
 * 键盘（Esc / Space 返回、↑↓ 切换条目、PageUp / PageDown 滚动、Tab / ⇧Tab 在预览的按钮之间移动焦点）由视图的
 * overlay 作用域处理。↑↓ 换条目时外壳不重播进入动效，但滚动位置、等宽开关、「第一处命中滚到中间」都按新条目重新开始。
 */
import { ExternalLink, FolderOpen, Globe, ImageDown, WholeWord, X } from 'lucide-react'
import { m, useIsPresent } from 'motion/react'
import {
  Fragment,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
  type Ref,
} from 'react'
import type { ClipHit, ClipItemDetail, OsKind } from '../../../shared/ipc/types'
import { formatNumber } from '../../../shared/i18n/format'
import { useT, type TFunction } from '../../../shared/i18n/useT'
import { usePreset } from '../../../shared/motion/presets'
import { Button, Icon, IconButton, ScrollArea } from '../../../shared/ui'
import { ClipLeading } from '../ClipRow'
import { fullTime } from '../format'
import { findMatches, formatHex, hostOf, PREVIEW_RENDER_CHARS, SEARCH_PREFIX_CHARS } from '../model'
import type { ClipActions } from '../useClipboard'
import { ColorInfo, FileList, ImageView, metaParts, toRgba, useDetail } from './parts'

export interface FullPreviewProps {
  hit: ClipHit
  /** 搜索框里的词：全文里高亮 */
  query: string
  /** 列表区高度（预览占满它） */
  height: number
  now: number
  os: OsKind
  actions: ClipActions
  onClose(): void
  /** 内容滚动容器（PageUp / PageDown） */
  scrollRef?: Ref<HTMLDivElement>
}

/** 预览头部（类型 + 元信息两行） */
export const PREVIEW_HEAD_HEIGHT = 52
/** 预览正文的下内边距（clipboard.css 的 .clip-preview-body） */
const PREVIEW_BODY_PAD_BOTTOM = 12

/**
 * 内容短的预览（文件、链接、颜色）在剩余区域里垂直居中（视觉中心略偏上，见 .clip-preview-center），不再只占上面一截、
 * 下面留出大片空白；内容放不下时照常从顶部开始、在框内滚动（min-height，不是固定高度）
 */
function Centered({ bodyHeight, children }: { bodyHeight: number; children: ReactNode }) {
  return (
    <div
      className="clip-preview-center"
      style={{ minHeight: Math.max(0, bodyHeight - PREVIEW_BODY_PAD_BOTTOM) }}
    >
      {children}
    </div>
  )
}

export function FullPreview({
  hit,
  query,
  height,
  now,
  os,
  actions,
  onClose,
  scrollRef,
}: FullPreviewProps) {
  const t = useT()
  const it = hit.item
  const detail = useDetail(actions, it.id, true)
  const motion = usePreset('previewIn')
  const present = useIsPresent()
  // 等宽开关跟随条目：换条目时回到该类型的默认值（代码等宽，其余比例字体）
  const [monoState, setMonoState] = useState({ id: it.id, mono: it.kind === 'code' })
  if (monoState.id !== it.id) setMonoState({ id: it.id, mono: it.kind === 'code' })
  const mono = monoState.id === it.id ? monoState.mono : it.kind === 'code'
  const textual = it.kind === 'text' || it.kind === 'rich_text' || it.kind === 'code'
  const files = detail?.files ?? it.files ?? []
  const sub = [it.source?.name, fullTime(it.last_used_at, now)].filter(Boolean).join(' · ')
  const bodyHeight = Math.max(0, height - PREVIEW_HEAD_HEIGHT)

  return (
    <m.section
      className="clip-preview"
      aria-label={t('clipboard.a11y.preview')}
      data-kind={it.kind}
      data-exiting={!present || undefined}
      data-testid="clip-preview"
      style={{ height }}
      {...motion}
    >
      <header className="clip-preview-head">
        <span className="clip-preview-icon">
          <ClipLeading hit={hit} thumb={{ width: 40, height: 32 }} />
        </span>
        <span className="clip-preview-title">
          <span className="clip-preview-kind" data-testid="clip-preview-title">
            {previewTitle(it, files.length, detail?.files != null, t)}
          </span>
          <span className="clip-preview-sub">{sub}</span>
        </span>
        {textual ? (
          <IconButton
            icon={WholeWord}
            label={t('clipboard.action.monospace')}
            active={mono}
            aria-pressed={mono}
            onClick={() => setMonoState({ id: it.id, mono: !mono })}
            data-testid="clip-preview-mono"
          />
        ) : null}
        <IconButton
          icon={X}
          label={t('clipboard.a11y.close_preview')}
          shortcut="Space"
          onClick={onClose}
          data-testid="clip-preview-close"
        />
      </header>
      <ScrollArea
        // 换条目：新的滚动容器（滚动位置从头开始）
        key={it.id}
        ref={scrollRef}
        className="clip-preview-body"
        maxHeight={bodyHeight}
        data-testid="clip-preview-body"
      >
        <PreviewBody
          hit={hit}
          detail={detail}
          query={query}
          mono={mono}
          bodyHeight={bodyHeight}
          os={os}
          t={t}
          actions={actions}
        />
      </ScrollArea>
    </m.section>
  )
}

/**
 * 预览标题：文件 →「3 个文件」（不再重复「文件 ·」）；颜色 →「颜色 · #3B82F6」；其余 →「类型 · 字数 · 行数」等
 */
function previewTitle(
  it: ClipHit['item'],
  fileCount: number,
  exactFiles: boolean,
  t: TFunction,
): string {
  if (it.kind === 'files') {
    return exactFiles
      ? t('clipboard.meta.files', { count: fileCount, n: formatNumber(fileCount) })
      : (metaParts(it, t)[0] ?? t('clipboard.kind.files'))
  }
  if (it.kind === 'color' && it.color) {
    const rgba = toRgba(it.color)
    return [t('clipboard.kind.color'), rgba ? formatHex(rgba) : it.color].join(' · ')
  }
  return [t(`clipboard.kind.${it.kind}`), ...metaParts(it, t)].join(' · ')
}

function PreviewBody({
  hit,
  detail,
  query,
  mono,
  bodyHeight,
  os,
  t,
  actions,
}: {
  hit: ClipHit
  detail: ClipItemDetail | null | undefined
  query: string
  mono: boolean
  bodyHeight: number
  os: OsKind
  t: TFunction
  actions: ClipActions
}) {
  const it = hit.item
  const box = useRef<HTMLDivElement>(null)
  const [width, setWidth] = useState(0)
  // 图片按列表区宽度等比缩放：用户按下 Space 之后读一次布局
  useLayoutEffect(() => {
    if (it.kind === 'image' && box.current) setWidth(box.current.clientWidth)
  }, [it.kind])

  switch (it.kind) {
    case 'image':
      return (
        <div ref={box} className="clip-preview-media">
          {width > 0 ? (
            <ImageView item={it} detail={detail} maxWidth={width} maxHeight={bodyHeight - 64} />
          ) : null}
          <div className="clip-preview-actions">
            <Button
              size="sm"
              variant="secondary"
              icon={ImageDown}
              onClick={() => actions.saveImage(it.id)}
            >
              {t('clipboard.action.save_image')}
            </Button>
          </div>
        </div>
      )
    case 'files': {
      const files = detail?.files ?? it.files ?? []
      return (
        <Centered bodyHeight={bodyHeight}>
          <div className="clip-preview-files">
            <FileList files={files} total={files.length} t={t} showPath />
            <div className="clip-preview-actions">
              <Button
                size="sm"
                variant="secondary"
                icon={FolderOpen}
                disabled={files.length > 0 && files.every((f) => !f.exists)}
                onClick={() => actions.reveal(it.id)}
              >
                {t(`clipboard.action.reveal.${os}`)}
              </Button>
            </div>
          </div>
        </Centered>
      )
    }
    case 'color':
      return it.color ? (
        <Centered bodyHeight={bodyHeight}>
          <ColorInfo value={it.color} t={t} copy />
        </Centered>
      ) : null
    case 'link': {
      const link = it.link ?? detail?.text ?? it.preview
      const host = hostOf(link)
      // Wisp 不替用户联网取网页标题 / favicon（隐私）：域名放大作标题，完整 URL 在下面（可选中；在 / ? & # = 之后优先
      // 折行，两种引擎都不会在路径段中间断开）
      return (
        <Centered bodyHeight={bodyHeight}>
          <div className="clip-preview-link">
            <div className="clip-preview-site">
              <span className="clip-preview-globe" aria-hidden="true">
                <Icon icon={Globe} size={18} />
              </span>
              <p className="clip-preview-host">{host ?? link}</p>
            </div>
            <p className="clip-preview-url" data-testid="clip-preview-url">
              <Marked text={link} query={query} breakUrl />
            </p>
            <div className="clip-preview-actions">
              <Button
                size="sm"
                variant="secondary"
                icon={ExternalLink}
                onClick={() => actions.openLink(it.id)}
              >
                {t('clipboard.action.open_link')}
              </Button>
            </div>
          </div>
        </Centered>
      )
    }
    default:
      return <TextBody key={it.id} hit={hit} detail={detail} query={query} mono={mono} t={t} />
  }
}

function TextBody({
  hit,
  detail,
  query,
  mono,
  t,
}: {
  hit: ClipHit
  detail: ClipItemDetail | null | undefined
  query: string
  mono: boolean
  t: TFunction
}) {
  const it = hit.item
  const full = detail?.text ?? null
  const text = full ?? it.preview
  const shown = text.length > PREVIEW_RENDER_CHARS ? text.slice(0, PREVIEW_RENDER_CHARS) : text
  const notes: string[] = []
  if ((it.char_count ?? 0) > SEARCH_PREFIX_CHARS) {
    notes.push(t('clipboard.meta.search_prefix', { n: formatNumber(SEARCH_PREFIX_CHARS) }))
  }
  if (shown.length < text.length) {
    notes.push(t('clipboard.meta.rendered_prefix', { n: formatNumber(PREVIEW_RENDER_CHARS) }))
  }
  if (it.truncated) notes.push(t('clipboard.meta.truncated'))
  if (detail?.has_html || detail?.has_rtf) notes.push(t('clipboard.meta.formats'))
  const pre = useRef<HTMLPreElement>(null)
  const code = it.kind === 'code'
  // 有搜索词时把第一处命中滚到中间（只在正文到达后执行一次）
  const scrolled = useRef(false)
  useLayoutEffect(() => {
    if (scrolled.current || full === null || !query.trim()) return
    scrolled.current = true
    pre.current?.querySelector('mark')?.scrollIntoView({ block: 'center' })
  }, [full, query])
  // 代码横向溢出：还能往哪边滚，哪边就渐隐（正文到达 / 换了正文时同步一次，之后随横向滚动同步）
  useLayoutEffect(() => {
    if (code && pre.current) syncFadeX(pre.current)
    // shown：正文变了（到达 / 换条目）要重新量
    // oxlint-disable-next-line react/exhaustive-effect-dependencies
  }, [code, shown])
  return (
    <>
      {notes.length > 0 ? (
        <p className="clip-preview-notes" data-testid="clip-preview-notes">
          {notes.map((n) => (
            <span key={n} className="clip-preview-note">
              {n}
            </span>
          ))}
        </p>
      ) : null}
      <pre
        ref={pre}
        className="clip-preview-text"
        data-mono={mono || undefined}
        // 代码不折行（横向滚动），缩进结构保持对齐
        data-code={code || undefined}
        onScroll={code ? (e) => syncFadeX(e.currentTarget) : undefined}
        data-loading={full === null && detail === undefined ? '' : undefined}
        data-testid="clip-preview-text"
      >
        <Marked text={shown} query={query} />
      </pre>
    </>
  )
}

/** 横向可滚动的代码：左右两边各自还有没有内容（`data-fade-x`：left / right / both，没有时去掉） */
function syncFadeX(el: HTMLElement): void {
  const left = el.scrollLeft > 1
  const right = el.scrollLeft + el.clientWidth < el.scrollWidth - 1
  const next = left && right ? 'both' : left ? 'left' : right ? 'right' : null
  if ((el.dataset.fadeX ?? null) === next) return
  if (next) el.dataset.fadeX = next
  else delete el.dataset.fadeX
}

/** URL 的折行点：/ ? & # = 之后（`scheme://` 里的两个斜杠除外） */
const URL_BREAK = /(?<!:\/?)[/?&#=](?=.)/g

/** 在 URL 的分隔符之后插入 `<wbr>`：只给出折行机会，不改变文本内容（复制、读屏都不受影响） */
function withBreaks(text: string, key: number): ReactNode[] {
  const out: ReactNode[] = []
  let last = 0
  for (const hit of text.matchAll(URL_BREAK)) {
    const end = (hit.index ?? 0) + 1
    out.push(text.slice(last, end), <wbr key={`${key}-${end}`} />)
    last = end
  }
  if (last < text.length) out.push(text.slice(last))
  return out
}

/** 文本里高亮搜索词（大小写不敏感，最多 500 处）；`breakUrl` 时在 URL 分隔符之后给出折行机会 */
function Marked({
  text,
  query,
  breakUrl = false,
}: {
  text: string
  query: string
  breakUrl?: boolean
}) {
  const parts = useMemo(() => {
    const plain = (s: string, key: number): ReactNode[] => (breakUrl ? withBreaks(s, key) : [s])
    const ranges = findMatches(text, query)
    if (ranges.length === 0) return plain(text, 0)
    const out: ReactNode[] = []
    let at = 0
    for (const r of ranges) {
      if (r.start > at) out.push(...plain(text.slice(at, r.start), at))
      out.push(
        <mark key={r.start} className="ui-mark">
          {text.slice(r.start, r.end)}
        </mark>,
      )
      at = r.end
    }
    if (at < text.length) out.push(...plain(text.slice(at), at))
    return out
  }, [text, query, breakUrl])
  return (
    <>
      {parts.map((p, i) => (
        // 片段顺序固定，用下标即可
        // oxlint-disable-next-line react/no-array-index-key
        <Fragment key={i}>{p}</Fragment>
      ))}
    </>
  )
}
