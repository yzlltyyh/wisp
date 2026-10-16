/**
 * 悬停预览卡片：画在面板内部、覆盖在列表上（左右内缩与行高亮同为 6 px）；
 * 放得下就在行的下方，否则在上方，两边都放不下时贴着空间更大的一侧并截断。出现 / 消失按 presets.popover（由列表的
 * AnimatePresence 播放退出）。卡片接收指针：指针在卡片上慢下来时卡片保持；正常速度扫过时由列表收起卡片、选中下面的
 * 行（ClipList）；点卡片等于点它对应的行。退出动画期间不接收指针。
 *
 * 文本只显示前 3 行：单行的条目直接用元数据里的预览（≤ 200 字，3 行放得下），多行的才取正文，而且只取开头
 * （`textHead`，最多 2000 字，每行再截到 300 字）——排版的工作量有上限。超过 LARGE_TEXT_CHARS 的多行正文不取
 * （`clipboard_get` 只能取全文，整段经 IPC 传过来再解析会在主线程上卡一下），同样用元数据里的预览。
 */
import { m, useIsPresent } from 'motion/react'
import { useStore } from 'zustand'
import { useLayoutEffect, useRef, useState } from 'react'
import { resolveAssetUrl } from '../../../shared/ipc'
import type { ClipHit } from '../../../shared/ipc/types'
import { relativeTime } from '../../../shared/i18n/relativeTime'
import { useT } from '../../../shared/i18n/useT'
import { usePreset } from '../../../shared/motion/presets'
import { AppIcon, Kbd } from '../../../shared/ui'
import type { ClipRunner } from '../actions'
import { blankLabel } from '../ClipRow'
import { hasTextBody } from '../kinds'
import { firstLines, hostOf, isBlank, LARGE_TEXT_CHARS } from '../model'
import type { ClipStore } from '../store'
import type { ClipCommands } from '../useClipboard'
import { EntityChips } from './Chips'
import { ColorInfo, FileList, ImageView, metaParts, useDetail, useHead } from './parts'

export interface HoverCardProps {
  hit: ClipHit
  /** 行在列表区里的上下边（px） */
  anchorTop: number
  anchorBottom: number
  /** 列表区高度 */
  areaHeight: number
  now: number
  cmd: ClipCommands
  /** 角落里「Space 打开预览」的按键来自注册表 */
  runner: ClipRunner
  /** 识别出的片段（芯片）读 store 里的检视结果 */
  store: ClipStore
}

/** 卡片与行、与列表区边缘的间距 */
const GAP = 4
const EDGE = 6
/** 图片在卡片里的最大高度 */
const IMAGE_MAX = 200
/** 文本卡片显示的行数（前 3 行） */
const TEXT_LINES = 3

export function HoverCard({
  hit,
  anchorTop,
  anchorBottom,
  areaHeight,
  now,
  cmd,
  runner,
  store,
}: HoverCardProps) {
  const t = useT()
  const it = hit.item
  const ref = useRef<HTMLDivElement>(null)
  const [pos, setPos] = useState<{ top: number; maxHeight: number; side: 'top' | 'bottom' }>({
    top: anchorBottom + GAP,
    maxHeight: areaHeight,
    side: 'bottom',
  })
  const present = useIsPresent()
  // 图片要原图（大图）；多行文本要正文开头（单行的预览已经够 3 行；很长的正文不取，见文件头）；其余类型用元数据就够
  const multiline =
    hasTextBody(it.kind) && (it.line_count ?? 1) > 1 && (it.char_count ?? 0) <= LARGE_TEXT_CHARS
  const detail = useDetail(cmd, it.id, it.kind === 'image')
  const head = useHead(cmd, it.id, multiline)
  const previewKey = runner.shortcut('preview')
  // 芯片的个数只用来在它们出现时重新定位（芯片本身由 EntityChips 读 store）
  const chips = useStore(store, (s) =>
    s.inspect?.id === it.id ? s.inspect.result.entities.length : 0,
  )
  const motion = usePreset('popover')

  // 定位：先按自然高度量一次，再决定放在行的上方还是下方
  useLayoutEffect(() => {
    const el = ref.current
    if (!el) return
    const h = el.scrollHeight
    const below = areaHeight - EDGE - (anchorBottom + GAP)
    const above = anchorTop - GAP - EDGE
    let next: typeof pos
    if (h <= below) next = { top: anchorBottom + GAP, maxHeight: below, side: 'bottom' }
    else if (h <= above) next = { top: anchorTop - GAP - h, maxHeight: above, side: 'top' }
    else if (below >= above) next = { top: anchorBottom + GAP, maxHeight: below, side: 'bottom' }
    else next = { top: EDGE, maxHeight: above, side: 'top' }
    setPos((p) =>
      p.top === next.top && p.maxHeight === next.maxHeight && p.side === next.side ? p : next,
    )
    // 详情（正文 / 原图）、芯片到达后内容高度会变，需要重新定位
    // oxlint-disable-next-line react/exhaustive-effect-dependencies
  }, [anchorTop, anchorBottom, areaHeight, detail, head, chips])

  const meta = metaParts(it, t)
  return (
    <m.div
      ref={ref}
      className="clip-hover"
      role="tooltip"
      aria-label={t('clipboard.a11y.preview')}
      data-side={pos.side}
      data-kind={it.kind}
      data-exiting={!present || undefined}
      data-testid="clip-hover"
      style={{
        top: pos.top,
        maxHeight: pos.maxHeight,
        transformOrigin: pos.side === 'bottom' ? 'center top' : 'center bottom',
      }}
      {...motion}
    >
      <div className="clip-hover-body">
        <HoverBody
          hit={hit}
          detail={detail}
          head={head}
          loading={multiline && head === undefined}
        />
      </div>
      <EntityChips store={store} runner={runner} id={it.id} />
      <div className="clip-hover-meta">
        <AppIcon
          src={it.source?.icon_url ? resolveAssetUrl(it.source.icon_url) : null}
          name={it.source?.name ?? t('clipboard.a11y.unknown_app')}
        />
        <span className="clip-hover-meta-text">
          {[it.source?.name, relativeTime(it.last_used_at, now), ...meta]
            .filter(Boolean)
            .join(' · ')}
        </span>
        {previewKey ? (
          <span className="clip-hover-key">
            <Kbd combo={previewKey} variant="cap" />
          </span>
        ) : null}
      </div>
    </m.div>
  )
}

function HoverBody({
  hit,
  detail,
  head,
  loading,
}: {
  hit: ClipHit
  detail: ReturnType<typeof useDetail>
  head: string | null | undefined
  loading: boolean
}) {
  const t = useT()
  const item = hit.item
  switch (item.kind) {
    case 'image':
      return <ImageView item={item} detail={detail} maxWidth={360} maxHeight={IMAGE_MAX} />
    case 'files':
      // 元数据最多带前 5 个文件，总数未知：不显示「还有 N 个」（元信息里写「5+ 个文件」）
      return <FileList files={item.files ?? []} t={t} />
    case 'color':
      return item.color ? <ColorInfo value={item.color} /> : null
    case 'link': {
      const link = item.link ?? item.preview
      const host = hostOf(link)
      return (
        <div className="clip-hover-link">
          {host ? <span className="clip-hover-host">{host}</span> : null}
          <span className="clip-hover-url">{link}</span>
        </div>
      )
    }
    default: {
      if (isBlank(item.preview) && !head?.trim()) {
        return <div className="clip-hover-text clip-blank">{blankLabel(item, t)}</div>
      }
      // 单行：元数据里的预览（≤ 200 字）；截断了的在末尾补 …
      const single =
        (item.char_count ?? 0) > item.preview.length && !item.preview.endsWith('…')
          ? `${item.preview}…`
          : item.preview
      const { text } = firstLines(head ?? single, TEXT_LINES)
      return (
        <div
          className="clip-hover-text"
          data-mono={item.kind === 'code' || undefined}
          data-loading={loading || undefined}
        >
          {text}
        </div>
      )
    }
  }
}
