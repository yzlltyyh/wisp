/**
 * 设置 → OCR → 识别历史。
 *
 * - 列表：缩略图 · 识别结果的前两行 · 时间 / 模式 / 模型；每行右侧 [重新翻译] [复制] [删除]（截图翻译的条目存的是译文，
 *   没有「重新翻译」）。点按一行展开「回看」：
 *   截图 + 全文（可选中），展开 / 收起用 presets.sectionExpand（高度 spring.gentle + 交叉淡入）；
 * - 实时：订阅 `ocr_history:changed`（inserted / removed / cleared）原地增删——新记录在顶部**按高度展开**进来
 *   （presets.sectionExpand：高度 spring.gentle + 交叉淡入），已有的行随排版逐帧下移，卡片下沿与下面的分组一起平滑
 *   移动（新行若一帧占满高度、靠 FLIP 平移已有的行，外框要么用弹簧追、最后一行被裁掉约 0.3 s，要么同步跟随、
 *   卡片下沿一帧跳一整行，所以由插入的行自己展开）；删除 / 裁剪的行
 *   presets.collapse 收起，不整页重拉；设置窗口自己发起的删除 / 清空同样会收到，按 id 去重。「显示更多」追加的行只
 *   淡入（不是实时插入）；读下一页失败时底部轻提示（列表仍在，按钮恢复可点）；
 * - 状态切换（读取中 / 读取失败 / 空 / 列表）：同一个 AnimatePresence 里交叉淡入（popLayout），只有这时外框高度用
 *   AutoHeight 过渡（`swapKey`）；行自己的展开 / 收起 / 插入 / 删除时外框与内容逐帧同高（不用第二条弹簧去追）；
 * - 回看的截图：先按**缩略图的宽高比**占好位（缩略图本身先放大显示），原图在后台按显示尺寸解码进 canvas（不在页面里
 *   保留整张 4K 位图），解码完成后交叉淡入替换缩略图；sectionExpand 量到的就是最终高度，展开结束后下沿不会再跳；
 * - 键盘：整张列表只有当前行在 Tab 序列里（roving tabindex），↑↓ 在行之间移动；Tab 走完当前行的操作就离开列表；
 * - 空状态：开关关闭（说明开启后保存什么、存在哪里）/ 开启但还没有记录；
 * - 关掉开关时有记录：原地出现确认条「已停止记录。已保存的 N 条要一并清空吗？」[保留][清空 N 条]；
 * - 「重新翻译」：`trigger_action(translate_selection, text, origin = ocr_history)`（不新增命令）；「复制」：CopyButton
 *   （`clipboard_copy_text`）；「删除」：`ocr_history_delete`，乐观地先收起这一行，失败时放回并提示。
 *
 * 深链：`WindowTarget::Settings { section: 'ocr', anchor: 'ocr_history' }`（OCR 面板 ⋯ 菜单「识别历史…」）——OcrPage 把
 * 分组滚到可见位置、焦点给分组标题，并短暂高亮这一块（减少动效时不闪）。
 */
import { History, ImageOff, Languages, Trash2 } from 'lucide-react'
import { AnimatePresence, m, useIsPresent } from 'motion/react'
import {
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
} from 'react'
import { useT } from '../../shared/i18n/useT'
import { relativeTime } from '../../shared/i18n/relativeTime'
import { api, on, resolveAssetUrl } from '../../shared/ipc'
import type { OcrHistoryChanged, OcrHistoryItem, OcrMode } from '../../shared/ipc/types'
import { presets, usePreset, type PresetProps } from '../../shared/motion/presets'
import { AutoHeight, Button, CopyButton, Icon, IconButton, Skeleton } from '../../shared/ui'
import { useConfirmKeys } from '../components/Confirm'
import { ipcErrorText } from '../lib/errors'
import { revealGrowing } from '../lib/reveal'
import { notify } from '../store'

/** 每页条数（「显示更多」一次再取这么多） */
export const PAGE = 30
/** 列表行里预览的最大字数（其余在展开后显示；CSS 再按两行截断） */
const PREVIEW_CHARS = 240

/** 把一次变化合进列表（纯函数，单测覆盖）：新记录插在最前、按 id 去重；删除 / 清空原地生效 */
export function applyChange(
  items: readonly OcrHistoryItem[],
  change: OcrHistoryChanged,
): OcrHistoryItem[] {
  switch (change.change) {
    case 'inserted':
      return [change.item, ...items.filter((x) => x.id !== change.item.id)]
    case 'removed': {
      const gone = new Set(change.ids)
      return items.filter((x) => !gone.has(x.id))
    }
    case 'cleared':
      return []
  }
}

interface HistoryState {
  items: OcrHistoryItem[]
  /** 经 `inserted` 事件实时插入的记录（进入时按高度展开；分页读到的只淡入） */
  live: ReadonlySet<number>
  /** 首次读取完成（之前显示骨架） */
  loaded: boolean
  /** 服务端可能还有更多（上一页取满了） */
  more: boolean
  loading: boolean
  failed: boolean
}

/** 识别历史的数据：分页读取 + 订阅变化事件 */
export function useOcrHistory() {
  const [state, setState] = useState<HistoryState>({
    items: [],
    live: new Set(),
    loaded: false,
    more: false,
    // 挂载时就开始读第一页
    loading: true,
    failed: false,
  })
  const latest = useRef(state)
  useLayoutEffect(() => {
    latest.current = state
  })

  /** 读一页（调用方先把 loading 置上：挂载时由初始状态，之后在事件处理里） */
  const fetchPage = useCallback((offset: number, alive: () => boolean = () => true) => {
    void api.ocrHistoryList(offset, PAGE).then(
      (page) => {
        // 防御：命令只会返回数组；测试替身等返回空值时按空页处理
        const list = Array.isArray(page) ? page : []
        if (alive()) setState((s) => withPage(s, list, offset))
      },
      (error: unknown) => {
        if (!alive()) return
        if (offset > 0) {
          // 「显示更多」失败：列表还在（failed 状态只用于第一页），按钮恢复可点，底部轻提示说明这次没读到
          setState((s) => ({ ...s, loading: false }))
          notify(ipcErrorText(error, 'ocr_history_list'), 'danger')
          return
        }
        setState((s) => ({ ...s, loaded: true, loading: false, failed: true }))
      },
    )
  }, [])

  useEffect(() => {
    let disposed = false
    fetchPage(0, () => !disposed)
    let unlisten: (() => void) | null = null
    void on('ocr_history:changed', (change) => {
      setState((s) => ({
        ...s,
        items: applyChange(s.items, change),
        live: change.change === 'inserted' ? new Set(s.live).add(change.item.id) : s.live,
      }))
    }).then((un) => {
      if (disposed) un()
      else unlisten = un
    })
    return () => {
      disposed = true
      unlisten?.()
    }
  }, [fetchPage])

  const loadMore = useCallback(() => {
    const s = latest.current
    if (s.loading || !s.more) return
    setState((cur) => ({ ...cur, loading: true, failed: false }))
    fetchPage(s.items.length)
  }, [fetchPage])
  const retry = useCallback(() => {
    setState((cur) => ({ ...cur, loading: true, failed: false }))
    fetchPage(0)
  }, [fetchPage])

  /** 乐观删除：先从列表里去掉；失败时放回原位 */
  const remove = useCallback(async (item: OcrHistoryItem) => {
    const index = latest.current.items.findIndex((x) => x.id === item.id)
    setState((s) => ({ ...s, items: s.items.filter((x) => x.id !== item.id) }))
    try {
      await api.ocrHistoryDelete(item.id)
      return true
    } catch (error) {
      setState((s) => {
        if (s.items.some((x) => x.id === item.id)) return s
        const items = [...s.items]
        items.splice(Math.max(0, Math.min(index, items.length)), 0, item)
        return { ...s, items }
      })
      notify(ipcErrorText(error, 'ocr_history_delete'), 'danger')
      return false
    }
  }, [])

  const clear = useCallback(async () => {
    const before = latest.current.items
    setState((s) => ({ ...s, items: [], more: false }))
    try {
      await api.ocrHistoryClear()
      return true
    } catch (error) {
      setState((s) => (s.items.length === 0 ? { ...s, items: before } : s))
      notify(ipcErrorText(error, 'ocr_history_clear'), 'danger')
      return false
    }
  }, [])

  return { ...state, loadMore, remove, clear, retry }
}

/** 读到一页之后的状态：读取期间到达的新记录已经插在前面，按 id 去重后接上 */
function withPage(s: HistoryState, page: readonly OcrHistoryItem[], offset: number): HistoryState {
  const seen = new Set(s.items.map((x) => x.id))
  const items =
    offset === 0 ? mergeFirst(page, s.items) : [...s.items, ...page.filter((x) => !seen.has(x.id))]
  return {
    items,
    live: s.live,
    loaded: true,
    more: page.length === PAGE,
    loading: false,
    failed: false,
  }
}

/** 第一页与读取期间已经收到的新记录合并（新记录在前，按 id 去重） */
function mergeFirst(
  page: readonly OcrHistoryItem[],
  early: readonly OcrHistoryItem[],
): OcrHistoryItem[] {
  const ids = new Set(page.map((x) => x.id))
  return [...early.filter((x) => !ids.has(x.id)), ...page]
}

export type OcrHistoryData = ReturnType<typeof useOcrHistory>

function modeLabel(t: ReturnType<typeof useT>, mode: OcrMode): string {
  return t(`common.ocr_mode.${mode}`)
}

/** 回看里截图的最大高度（px，与 .st-ocrh-shot 一致） */
const SHOT_MAX_HEIGHT = 240
/** 缩略图还没加载完、量不到宽高比时的占位比例 */
const SHOT_FALLBACK_RATIO = 4 / 3

/** 列表本体（放在识别历史分组的卡片里，开关行之后） */
export function OcrHistoryList({
  enabled,
  limit,
  history,
}: {
  enabled: boolean
  limit: number
  history: OcrHistoryData
}) {
  const t = useT()
  const swap = usePreset('footerIn')
  const swapOut = presets.menuOut
  const { items, loaded, failed } = history
  // 同一个 AnimatePresence 里的四种状态：交叉淡入（popLayout：退场的一方脱离排版叠在原位）；只有换状态时外框高度
  // 由 AutoHeight 过渡，列表里行的展开 / 收起 / 插入 / 删除时外框同步跟随
  const view = !loaded
    ? 'loading'
    : failed && items.length === 0
      ? 'failed'
      : items.length === 0
        ? 'empty'
        : 'list'
  return (
    <AutoHeight className="st-ocrh-box" swapKey={view} data-testid="ocr-history-box">
      <AnimatePresence initial={false} mode="popLayout">
        {view === 'loading' ? (
          <m.div key="loading" className="st-ocrh-loading" aria-busy="true" exit={swapOut}>
            <Skeleton lines={2} />
          </m.div>
        ) : view === 'failed' ? (
          <m.div
            key="failed"
            className="st-ocrh-empty"
            role="alert"
            data-testid="ocr-history-failed"
            {...swap}
            exit={swapOut}
          >
            <span className="st-ocrh-empty-icon" aria-hidden="true">
              <Icon icon={History} size={18} />
            </span>
            <span className="st-ocrh-empty-text">
              <b>{t('settings.ocr.history_load_failed')}</b>
            </span>
            <Button size="sm" variant="ghost" onClick={history.retry}>
              {t('common.action.retry')}
            </Button>
          </m.div>
        ) : view === 'empty' ? (
          <m.div
            key="empty"
            className="st-ocrh-empty"
            data-testid="ocr-history-empty"
            {...swap}
            exit={swapOut}
          >
            <span className="st-ocrh-empty-icon" aria-hidden="true">
              <Icon icon={History} size={18} />
            </span>
            <span className="st-ocrh-empty-text">
              <b>
                {t(enabled ? 'settings.ocr.history_empty_on' : 'settings.ocr.history_empty_off')}
              </b>
              <span>
                {enabled
                  ? t('settings.ocr.history_empty_on_desc')
                  : t('settings.ocr.history_empty_off_desc', { n: limit })}
              </span>
            </span>
          </m.div>
        ) : (
          <m.div key="list" {...swap} exit={swapOut}>
            <HistoryRows history={history} />
          </m.div>
        )}
      </AnimatePresence>
    </AutoHeight>
  )
}

/** 列表行 + 「显示更多」：实时插入按高度展开、roving tabindex */
function HistoryRows({ history }: { history: OcrHistoryData }) {
  const t = useT()
  const [open, setOpen] = useState<number | null>(null)
  const rowIn = usePreset('sectionExpand')
  const pageIn = usePreset('footerIn')
  const collapse = usePreset('collapse')
  const { items, live, more, loading } = history
  const listRef = useRef<HTMLUListElement>(null)
  /** 当前行（Tab 序列里唯一的一行）；null = 第一行 */
  const [current, setCurrent] = useState<number | null>(null)
  const currentId =
    current !== null && items.some((x) => x.id === current) ? current : (items[0]?.id ?? null)

  // roving tabindex：只有当前行的按钮在 Tab 序列里（30 行 × 4 个停顿太长）
  useLayoutEffect(() => {
    const list = listRef.current
    if (!list) return
    for (const li of list.querySelectorAll<HTMLElement>(':scope > li[data-id]')) {
      const tabbable = Number(li.dataset.id) === currentId && li.dataset.leaving === undefined
      for (const b of li.querySelectorAll<HTMLElement>('button')) {
        if (tabbable) b.removeAttribute('tabindex')
        else b.tabIndex = -1
      }
    }
  })

  /** ↑↓ 在行之间移动（焦点给那一行的主按钮）；Home / End 到首尾 */
  const onKeyDown = (e: ReactKeyboardEvent<HTMLUListElement>) => {
    if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(e.key)) return
    const rows = [
      ...(listRef.current?.querySelectorAll<HTMLElement>(
        ':scope > li[data-id]:not([data-leaving]) .st-ocrh-main',
      ) ?? []),
    ]
    const at = rows.findIndex((b) => b.closest('li')?.contains(document.activeElement))
    if (at < 0 || rows.length === 0) return
    e.preventDefault()
    const target =
      e.key === 'Home'
        ? rows[0]
        : e.key === 'End'
          ? rows[rows.length - 1]
          : rows[Math.max(0, Math.min(rows.length - 1, at + (e.key === 'ArrowDown' ? 1 : -1)))]
    target?.focus()
  }

  return (
    <div className="st-ocrh" data-testid="ocr-history-list">
      {/* oxlint-disable-next-line jsx-a11y/no-noninteractive-element-interactions -- ↑↓ 只是在行之间移动焦点 */}
      <ul
        ref={listRef}
        className="st-ocrh-list"
        aria-label={t('settings.ocr.history')}
        onKeyDown={onKeyDown}
        onFocus={(e) => {
          const id = Number(e.target.closest<HTMLElement>('li[data-id]')?.dataset.id)
          if (Number.isFinite(id) && id !== currentId) setCurrent(id)
        }}
      >
        <AnimatePresence initial={false}>
          {items.map((item) => (
            <HistoryItem
              key={item.id}
              item={item}
              enter={live.has(item.id) ? rowIn : pageIn}
              exit={collapse.exit}
              open={open === item.id}
              onToggle={() => setOpen((cur) => (cur === item.id ? null : item.id))}
              onDelete={() => {
                if (open === item.id) setOpen(null)
                void history.remove(item).then((ok) => {
                  if (ok) notify(t('settings.ocr.history_deleted'), 'success')
                })
              }}
            />
          ))}
        </AnimatePresence>
      </ul>
      {more ? (
        <div className="st-ocrh-more">
          <Button
            size="sm"
            variant="ghost"
            disabled={loading}
            onClick={history.loadMore}
            data-testid="ocr-history-more"
          >
            {t('settings.ocr.history_more')}
          </Button>
        </div>
      ) : null}
    </div>
  )
}

/** 一行（列表项）：进入方式在挂载时定下（实时插入按高度展开 / 分页淡入）；退场时收起并标记，键盘导航与 Tab 序列跳过它 */
function HistoryItem({
  item,
  enter,
  exit,
  ...row
}: {
  item: OcrHistoryItem
  enter: PresetProps
  exit: PresetProps['exit']
  open: boolean
  onToggle: () => void
  onDelete: () => void
}) {
  const present = useIsPresent()
  const [motion] = useState(enter)
  return (
    <m.li
      className="st-ocrh-item st-clip"
      data-id={item.id}
      data-leaving={present ? undefined : ''}
      data-testid={present ? `ocr-history-item-${item.id}` : undefined}
      initial={motion.initial}
      animate={motion.animate}
      exit={exit}
    >
      <HistoryRow item={item} {...row} />
    </m.li>
  )
}

function HistoryRow({
  item,
  open,
  onToggle,
  onDelete,
}: {
  item: OcrHistoryItem
  open: boolean
  onToggle: () => void
  onDelete: () => void
}) {
  const t = useT()
  const expand = usePreset('sectionExpand')
  const collapse = usePreset('collapse')
  const detailId = useId()
  const head = useRef<HTMLDivElement>(null)
  const detail = useRef<HTMLDivElement>(null)
  /** 用户刚点开：展开过程中跟随滚动，露出正在长高的下沿（与 Disclosure 一致） */
  const opening = useRef(false)
  /** 缩略图的宽高比（回看时按它给截图占位） */
  const [ratio, setRatio] = useState<number | null>(null)
  const preview = item.text.slice(0, PREVIEW_CHARS).trim()
  const thumb = item.thumb_url ? resolveAssetUrl(item.thumb_url) : null
  const image = item.image_url ? resolveAssetUrl(item.image_url) : null
  const when = relativeTime(item.created_at)
  const meta = [when, modeLabel(t, item.mode), item.model].filter(Boolean).join(' · ')
  const translate = () => {
    void api.triggerAction('translate_selection', 'ocr_history', item.text).then(
      () => notify(t('settings.ocr.history_sent'), 'success'),
      (error: unknown) => notify(ipcErrorText(error, 'trigger_action'), 'danger'),
    )
  }
  return (
    <div className="st-ocrh-row" data-open={open || undefined}>
      <div ref={head} className="st-ocrh-head">
        {/* 无障碍名称：预览 · 时间 · 模式 · 模型；展开 / 收起由 aria-expanded 表达（不用原生 title：会读两遍、样式也不一致） */}
        <button
          type="button"
          className="st-ocrh-main"
          aria-expanded={open}
          aria-controls={detailId}
          aria-label={`${preview || t('settings.ocr.history_open')} · ${meta}`}
          onClick={() => {
            opening.current = !open
            onToggle()
          }}
        >
          <span className="st-ocrh-thumb" aria-hidden="true">
            {thumb ? (
              <img
                src={thumb}
                alt=""
                draggable={false}
                decoding="async"
                data-releasable=""
                onLoad={(e) => {
                  const img = e.currentTarget
                  if (img.naturalWidth > 0 && img.naturalHeight > 0)
                    setRatio(img.naturalWidth / img.naturalHeight)
                }}
              />
            ) : (
              <Icon icon={ImageOff} size={14} />
            )}
          </span>
          <span className="st-ocrh-text">
            {/* 展开时行内的两行预览淡出、高度收起（presets.collapse），回看区的全文是唯一的一份——同一段文字不在同屏
                出现两遍；收起时按 sectionExpand 长回来 */}
            <AnimatePresence initial={false}>
              {open ? null : (
                <m.span
                  key="preview"
                  className="st-ocrh-preview"
                  data-testid={`ocr-history-preview-${item.id}`}
                  initial={expand.initial}
                  animate={expand.animate}
                  exit={collapse.exit}
                >
                  {preview}
                </m.span>
              )}
            </AnimatePresence>
            <span className="st-ocrh-meta">
              <time dateTime={new Date(item.created_at).toISOString()}>{when}</time>
              <span aria-hidden="true"> · </span>
              {modeLabel(t, item.mode)}
              {item.model ? (
                <>
                  <span aria-hidden="true"> · </span>
                  {item.model}
                </>
              ) : null}
            </span>
          </span>
        </button>
        <span className="st-ocrh-actions">
          {/* 截图翻译的条目（含两次调用的直接翻译）存的是译文：再送去翻译只会把译文翻回去，不提供。
              放在最左：缺了它的行里「复制」「删除」仍与其他行上下对齐 */}
          {item.mode === 'translate' ? null : (
            <IconButton
              icon={Languages}
              label={t('settings.ocr.history_translate')}
              onClick={translate}
              data-testid={`ocr-history-translate-${item.id}`}
            />
          )}
          <CopyButton
            text={item.text}
            label={t('settings.ocr.history_copy')}
            data-testid={`ocr-history-copy-${item.id}`}
          />
          <IconButton
            icon={Trash2}
            label={t('settings.ocr.history_delete')}
            onClick={onDelete}
            className="st-ocrh-delete"
            data-testid={`ocr-history-delete-${item.id}`}
          />
        </span>
      </div>
      <AnimatePresence initial={false}>
        {open ? (
          <m.div
            key="detail"
            ref={detail}
            id={detailId}
            className="st-ocrh-detail st-clip"
            {...expand}
            onUpdate={(latest) => {
              if (opening.current) revealGrowing(detail.current, latest.height, head.current)
            }}
            onAnimationComplete={() => {
              opening.current = false
            }}
          >
            <div className="st-ocrh-detail-body">
              {thumb || image ? (
                <HistoryShot
                  thumb={thumb}
                  image={image}
                  ratio={ratio ?? SHOT_FALLBACK_RATIO}
                  alt={t('settings.ocr.history_image')}
                  id={item.id}
                />
              ) : (
                <p className="st-ocrh-noimage">{t('settings.ocr.history_no_image')}</p>
              )}
              <pre className="st-ocrh-full" data-testid={`ocr-history-text-${item.id}`}>
                {item.text}
              </pre>
            </div>
          </m.div>
        ) : null}
      </AnimatePresence>
    </div>
  )
}

/**
 * 回看的截图：外框按宽高比定好尺寸（最高 240 px），先放大显示缩略图；原图在后台按外框的显示尺寸（× 设备像素比）解码
 * 进 canvas——`createImageBitmap` 的 resize 选项让引擎直接按目标尺寸解码，页面里不留整张原图的位图（4K 截图 30 多
 * MiB）；解码完成后 canvas 交叉淡入盖住缩略图（--dur-chunk）。不支持 resize 选项的引擎退回整张解码、画完即 close()
 */
function HistoryShot({
  thumb,
  image,
  ratio,
  alt,
  id,
}: {
  thumb: string | null
  image: string | null
  ratio: number
  alt: string
  id: number
}) {
  const canvas = useRef<HTMLCanvasElement>(null)
  const [sharp, setSharp] = useState(false)
  useEffect(() => {
    const el = canvas.current
    if (!thumb || !image || !el || typeof createImageBitmap !== 'function') return
    const ac = new AbortController()
    let alive = true
    void (async () => {
      const blob = await (await fetch(image, { signal: ac.signal })).blob()
      const scale = window.devicePixelRatio || 1
      const width = Math.max(1, Math.round(el.clientWidth * scale))
      const height = Math.max(1, Math.round(el.clientHeight * scale))
      let bitmap: ImageBitmap
      try {
        bitmap = await createImageBitmap(blob, {
          resizeWidth: width,
          resizeHeight: height,
          resizeQuality: 'high',
        })
      } catch {
        bitmap = await createImageBitmap(blob)
      }
      if (!alive) return bitmap.close()
      el.width = width
      el.height = height
      el.getContext('2d')?.drawImage(bitmap, 0, 0, width, height)
      bitmap.close()
      setSharp(true)
    })().catch(() => undefined)
    return () => {
      alive = false
      ac.abort()
    }
  }, [thumb, image])
  return (
    <span
      className="st-ocrh-shot"
      data-sharp={sharp || undefined}
      data-testid={`ocr-history-image-${id}`}
      style={{
        aspectRatio: String(ratio),
        width: `min(100%, ${Math.round(SHOT_MAX_HEIGHT * ratio)}px)`,
      }}
    >
      {/* 读屏读缩略图的替代文本；清晰的原图画在上面的 canvas 里（装饰，读屏跳过）。没有缩略图时直接显示原图 */}
      <img src={thumb ?? image ?? ''} alt={alt} draggable={false} data-releasable="" />
      {thumb && image ? <canvas ref={canvas} aria-hidden="true" /> : null}
    </span>
  )
}

/**
 * 关掉开关时有已保存的记录：原地出现的确认条「已停止记录。已保存的 N 条要一并清空吗？」[保留][清空 N 条]——开关此刻
 * 已经关了，危险按钮只做「清空」这一件事（不再写「关闭并清空」）。
 * 打开时焦点给「保留」（安全的一侧），Esc = 保留
 */
export function TurnOffPrompt({
  count,
  onKeep,
  onClear,
}: {
  count: number
  onKeep: () => void
  onClear: () => void
}) {
  const t = useT()
  const fade = usePreset('confirmIn')
  const keep = useRef<HTMLButtonElement>(null)
  const container = useRef<HTMLDivElement>(null)
  useConfirmKeys(true, onKeep, { cancel: keep, container })
  return (
    <m.div className="st-clip" {...fade}>
      {/* 不加 role=alert：打开时焦点已经移到「保留」，读屏随焦点读出这一条，再播报一次就重复了 */}
      <div
        ref={container}
        className="st-confirm-row ui-confirm"
        data-tone="danger"
        data-testid="ocr-history-off-prompt"
      >
        <span className="ui-confirm-text">
          {t('settings.ocr.history_off_question', { count, n: count })}
        </span>
        <span className="ui-confirm-actions">
          <Button ref={keep} size="sm" variant="ghost" onClick={onKeep}>
            {t('settings.ocr.history_off_keep')}
          </Button>
          <Button size="sm" variant="danger" onClick={onClear} data-testid="ocr-history-off-clear">
            {t('settings.ocr.history_off_clear', { count, n: count })}
          </Button>
        </span>
      </div>
    </m.div>
  )
}
