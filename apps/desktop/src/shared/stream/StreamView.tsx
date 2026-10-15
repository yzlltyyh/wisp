/**
 * 流式视图。
 *
 * - `format`：plain（原样显示，保留空白，不解析 Markdown）/ mdlite（增量 Markdown-lite）/ edit_split（润色、校对：
 *   正文 + 淡色小字的「改动」，分隔线之后出现「改动」标签或流结束时才切换显示，流式中只切换一次）；
 * - 文字由 Writer 直接写 DOM（不经 React，流式文本不进 store），每个 rAF 合并一次；
 * - 末尾光标用 CSS `::after`，1 Hz 脉冲，结束时淡出，减少动效时静止；容器 `contain: content`；
 * - `maxHeight`：超过后内部滚动，并自动跟随到底部；用户以任何方式上滚（滚轮、拖动滚动条、键盘 PageUp / ↑、
 *   触控板）后停止跟随，滚回底部恢复。判断依据是 scroll 事件里 scrollTop 相对上一次的方向，
 *   程序自己设置 scrollTop（跟随到底部）引起的 scroll 事件不算用户操作；
 * - edit_split 的切分检测是增量的（EditSplitStream）：每次只扫描新到达的文字，整段流的开销是 O(n)；
 * - 通过 `ref` 拿到 `StreamHandle`：`append` / `end` / `reset` / `text` / `body`（复制按钮只复制正文）。
 *
 * 初始内容（`initialText`）在布局阶段同步写入，prepare 路径下也能立即测量高度。
 */
import {
  useCallback,
  useImperativeHandle,
  useLayoutEffect,
  useRef,
  useState,
  type Ref,
} from 'react'
import { useViewActive } from '../hooks/useViewActive'
import { useT } from '../i18n/useT'
import { cx } from '../ui/cx'
import { syncFadeEdges } from '../ui/ScrollArea'
import { EditSplitStream, splitEditOutput, type EditSplit } from './editSplit'
import { URL_BREAK, stripUrlBreaks } from './urlBreaks'
import { StreamWriter } from './writer'

export type StreamFormat = 'plain' | 'mdlite' | 'edit_split'

export interface StreamHandle {
  append(text: string): void
  /** 流结束（光标淡出、合并 DOM、edit_split 做最终切分） */
  end(): void
  /** 清空，可选写入初始文字（不做动画），回到流式状态 */
  reset(text?: string): void
  /** 已收到的全文 */
  text(): string
  /** edit_split 的正文（其他格式等于全文） */
  body(): string
}

export interface StreamViewProps {
  format: StreamFormat
  /** 初始全文（回放、静态内容） */
  initialText?: string
  /** 初始即已结束（静态内容：无光标） */
  ended?: boolean
  /** 流式中显示末尾光标（默认 true） */
  caret?: boolean
  /** 内部滚动的最大高度（px）；不传则随内容增高 */
  maxHeight?: number
  /** edit_split：切分结果变化时回调 */
  onSplit?: (split: EditSplit) => void
  ref?: Ref<StreamHandle>
  className?: string
  'data-testid'?: string
}

/** 「接近底部」的容差（px） */
const FOLLOW_SLACK = 8

export function StreamView({
  format,
  initialText = '',
  ended = false,
  caret = true,
  maxHeight,
  onSplit,
  ref,
  className,
  'data-testid': testId = 'stream-view',
}: StreamViewProps) {
  const t = useT()
  const root = useRef<HTMLDivElement>(null)
  const bodyEl = useRef<HTMLDivElement>(null)
  const changesEl = useRef<HTMLDivElement>(null)
  const body = useRef<StreamWriter | null>(null)
  const changes = useRef<StreamWriter | null>(null)
  const full = useRef('')
  const split = useRef<EditSplit | null>(null)
  /** edit_split 的增量切分检测与当前显示所依据的切分点版本 */
  const tracker = useRef(new EditSplitStream())
  const splitVersion = useRef(0)
  const follow = useRef(true)
  /** 上一次看到的 scrollTop（判断滚动方向）与程序最近一次设置的 scrollTop（区分用户滚动） */
  const lastTop = useRef(0)
  const programmaticTop = useRef<number | null>(null)
  /** 正在流式输出（只有这时才自动跟随到底部；初始内容、静态内容停在顶部） */
  const streaming = useRef(false)
  const [hasChanges, setHasChanges] = useState(false)
  const onSplitRef = useRef(onSplit)
  useLayoutEffect(() => {
    onSplitRef.current = onSplit
  })

  /**
   * 是否内部滚动（`maxHeight` 已给出）。放在 ref 里：写入器只在挂载 / 格式变化时创建，它的 onFlush 必须读到
   * 最新值——视图可以在内容到达上限后才给出 maxHeight（或反过来去掉），跟随到底部不能停在创建时的闭包上
   */
  const scrollable = useRef(maxHeight !== undefined)
  const stick = useCallback(() => {
    const el = root.current
    if (!el || !scrollable.current) return
    if (streaming.current && follow.current) {
      const bottom = el.scrollHeight - el.clientHeight
      if (bottom > el.scrollTop) {
        el.scrollTop = bottom
        // 浏览器可能取整：记下实际落点，对应的 scroll 事件不当作用户滚动
        programmaticTop.current = el.scrollTop
        lastTop.current = el.scrollTop
      }
    }
    syncFadeEdges(el)
  }, [])
  // 上限变化（含从「随内容增高」切换为内部滚动）：立即按当前跟随状态贴底并同步边缘渐隐，不等下一次写入。
  // 所在视图还在切换的暂存层里时不读布局（不在挂载的提交里强制排版暂存视图），显示的那次提交里再做
  const active = useViewActive()
  useLayoutEffect(() => {
    scrollable.current = maxHeight !== undefined
    if (active) stick()
  }, [maxHeight, stick, active])

  const onScroll = (el: HTMLDivElement) => {
    const top = el.scrollTop
    const atBottom = top + el.clientHeight >= el.scrollHeight - FOLLOW_SLACK
    if (programmaticTop.current !== null && Math.abs(top - programmaticTop.current) < 1) {
      programmaticTop.current = null
    } else if (atBottom) {
      follow.current = true
    } else if (top < lastTop.current) {
      // 用户往上滚（滚轮、滚动条、键盘、触控板都会走到这里）：停止跟随，直到滚回底部
      follow.current = false
    }
    lastTop.current = top
    syncFadeEdges(el)
  }

  // 写入器：挂载 / 格式变化时（重新）创建，初始内容同步写入（onFlush 是稳定的 stick，读 ref）
  useLayoutEffect(() => {
    const host = bodyEl.current
    const changesHost = changesEl.current
    if (!host || !changesHost) return
    const bodyWriter = new StreamWriter(host, {
      format: format === 'mdlite' ? 'mdlite' : 'plain',
      caret,
      onFlush: stick,
    })
    const changesWriter = new StreamWriter(changesHost, {
      format: 'plain',
      caret: false,
      onFlush: stick,
    })
    body.current = bodyWriter
    changes.current = changesWriter
    full.current = ''
    split.current = null
    tracker.current = new EditSplitStream()
    splitVersion.current = 0
    setHasChanges(false)
    bodyWriter.reset('')
    if (initialText) apply(initialText, false)
    if (ended) finish()
    return () => {
      bodyWriter.destroy()
      changesWriter.destroy()
      body.current = null
      changes.current = null
    }
    // 只在格式变化时重建；initialText / ended 是初始值
    // oxlint-disable-next-line react/exhaustive-deps
  }, [format])

  /** 写入新到达的文字（edit_split：检测到「改动」后，后续文字进改动区） */
  function apply(text: string, animate: boolean): void {
    const b = body.current
    const c = changes.current
    if (!b || !c) return
    full.current += text
    if (format !== 'edit_split') {
      if (animate) b.append(text)
      else b.reset(full.current)
      return
    }
    const tr = tracker.current
    tr.push(text)
    if (split.current) {
      if (tr.version !== splitVersion.current || !tr.labeled) {
        // 切分点变了（改动区里又出现了分隔线）：全量重算一次，结束时 finish() 再以全文为准
        const next = splitEditOutput(full.current)
        c.reset(next.changes ?? '')
        split.current = next
        splitVersion.current = tr.version
        return
      }
      // 同一切分点：改动只会在末尾增长，只追加增量
      const latest = tr.changes()
      if (latest.length > c.text.length) {
        if (animate) c.append(latest.slice(c.text.length))
        else c.reset(latest)
      }
      split.current = { body: split.current.body, changes: latest }
      return
    }
    if (tr.labeled) {
      // 只切换一次：正文定格为切分出的正文，之后的文字进改动区
      const s: EditSplit = { body: tr.body(), changes: tr.changes() }
      split.current = s
      splitVersion.current = tr.version
      b.reset(s.body)
      b.setCaret(false)
      c.reset(s.changes ?? '')
      c.setCaret(caret)
      setHasChanges(true)
      onSplitRef.current?.(s)
      return
    }
    if (animate) b.append(text)
    else b.reset(full.current)
  }

  function finish(): void {
    const b = body.current
    const c = changes.current
    if (!b || !c) return
    if (format === 'edit_split') {
      const final = splitEditOutput(full.current)
      if (final.body !== b.text) b.reset(final.body)
      if ((final.changes ?? '') !== c.text) c.reset(final.changes ?? '')
      setHasChanges(final.changes !== null)
      if (!split.current || split.current.changes !== final.changes) onSplitRef.current?.(final)
      split.current = final
      c.end()
    }
    b.end()
  }

  useImperativeHandle(
    ref,
    () => ({
      append: (text) => {
        streaming.current = true
        apply(text, true)
      },
      end: () => {
        finish()
        streaming.current = false
      },
      reset: (text = '') => {
        streaming.current = false
        full.current = ''
        split.current = null
        tracker.current = new EditSplitStream()
        splitVersion.current = 0
        follow.current = true
        programmaticTop.current = null
        lastTop.current = 0
        setHasChanges(false)
        body.current?.reset('')
        body.current?.setCaret(caret)
        changes.current?.reset('')
        if (text) apply(text, false)
      },
      text: () => full.current,
      body: () => (format === 'edit_split' ? splitEditOutput(full.current).body : full.current),
    }),
    // apply / finish 读的都是 ref
    // oxlint-disable-next-line react/exhaustive-deps
    [format, caret],
  )

  return (
    <div
      ref={root}
      className={cx('ui-stream', maxHeight !== undefined && 'ui-scroll', className)}
      style={maxHeight !== undefined ? { maxHeight } : undefined}
      data-format={format}
      data-testid={testId}
      onWheel={(e) => {
        // 滚轮上滚立即停止跟随（不等 scroll 事件，避免同一帧的 flush 又把它拉回底部）
        if (e.deltaY < 0) follow.current = false
      }}
      onScroll={(e) => onScroll(e.currentTarget)}
      // 系统的复制（右键菜单、未被视图接管的 ⌘C）：去掉 URL 的折行点（urlBreaks.ts），复制出去的与原文一致
      onCopy={(e) => {
        const text = window.getSelection()?.toString() ?? ''
        if (!text.includes(URL_BREAK)) return
        e.clipboardData.setData('text/plain', stripUrlBreaks(text))
        e.preventDefault()
      }}
    >
      <div
        ref={bodyEl}
        className={cx('ui-stream-body', format === 'mdlite' && 'ui-md')}
        data-format={format === 'mdlite' ? 'mdlite' : 'plain'}
        data-testid={`${testId}-body`}
      />
      <div
        className="ui-stream-changes"
        hidden={!hasChanges}
        aria-label={t('panel.stream.changes')}
        data-testid={`${testId}-changes`}
      >
        <div ref={changesEl} className="ui-stream-body" data-format="plain" />
      </div>
    </div>
  )
}
