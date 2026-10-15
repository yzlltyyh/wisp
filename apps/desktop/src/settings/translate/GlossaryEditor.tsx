/**
 * 术语表：表格编辑 source / target / note；source 与 target 相同表示保持不译；
 * note 只在这里显示、不发给模型；导入（JSON / CSV / TSV，Rust 弹系统对话框）/ 导出；条目多时可搜索。
 *
 * - 写入：**逐条**——停顿后只把改过的行写出去（`glossary_upsert`，带上这一行改动前的原文；原文清空或删除行 =
 *   `glossary_remove`），不再整表 `glossary_set`：编辑器打开之后别处（其他设备同步、手改文件、导入）加进来的术语
 *   不会被窗口里的旧列表删掉。写入**串行**（同一时刻只有一条在途，按改动的先后发出）；挂载期间登记到 writes.ts
 *   （关窗前提交并等待）。两行原文相同时（例如新加一行来替换旧的那行）术语表里只有一条：其中一行删除、清空或改了
 *   原文，这一条改写成另一行的内容，而不是被删掉；
 * - 外部变化：收到 `glossary:changed`（任何来源的改动都会发）时重新读取；有没写出的改动、写入在途或焦点在某一行里
 *   时先记下，等这些都结束再读，不打断输入。读回的条目按原文对上已有的行（沿用行的 key，不重新挂载、不丢焦点），
 *   内容就是这一行自己写进去的那份时保持原样；
 * - 跟手：行组件 memo、按 key 更新，打字只重渲染当前行；条目超过 VIRTUAL_AFTER 时表格改为内部滚动的虚拟列表；
 * - 搜索时正在编辑的行即使不再匹配也保持可见（不会在输入中途消失、丢焦点）；
 * - 「添加术语」的新行只在第一次出现时滚过去、自动聚焦一次（获得焦点后就清掉标记）：之后删行、过滤、虚拟列表把它
 *   滚出再滚回，都不会再跳过去或抢走搜索框 / 别的行的焦点；
 * - 版式像表格而不是表单：单元格平时没有边框（hairline 分隔），悬停或聚焦时才显示输入框；删除按钮只在行悬停或聚焦时出现；
 *   内部滚动的虚拟列表上下边缘渐隐，提示这里还能滚。
 */
import { useVirtualizer } from '@tanstack/react-virtual'
import { Download, Plus, Search, Upload, X } from 'lucide-react'
import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { useT } from '../../shared/i18n/useT'
import { api, on } from '../../shared/ipc'
import type { GlossaryEntry } from '../../shared/ipc/types'
import {
  Badge,
  Button,
  Icon,
  IconButton,
  SearchField,
  Section,
  Skeleton,
  TextField,
} from '../../shared/ui'
import { ipcErrorText } from '../lib/errors'
import { COMMIT_DELAY_MS } from '../lib/useDraft'
import { registerFlusher, trackWrite } from '../lib/writes'
import { notify } from '../store'

/** 条目超过这个数时显示搜索框 */
const SEARCH_AFTER = 8
/** 条目超过这个数时改为内部滚动的虚拟列表 */
const VIRTUAL_AFTER = 60
/** 行高（与 .st-glossary-row 一致）与虚拟列表可见的行数 */
const ROW_HEIGHT = 40
const VISIBLE_ROWS = 10

export interface Row extends GlossaryEntry {
  /** 本地行 id（React key；新增的空行还没有 source） */
  key: number
}

let rowSeq = 0

/** 一行要写进术语表的样子；原文为空时为 null（不写入） */
export function toEntry(row: GlossaryEntry): GlossaryEntry | null {
  const source = row.source.trim()
  if (!source) return null
  return {
    source,
    // 译文留空 = 与原文相同 = 保持不译
    target: row.target.trim() || source,
    note: row.note?.trim() || null,
  }
}

function sameEntry(a: GlossaryEntry, b: GlossaryEntry): boolean {
  return a.source === b.source && a.target === b.target && (a.note ?? null) === (b.note ?? null)
}

/**
 * 把读回的条目对上已有的行：按原文找到原来的行沿用它的 key（不重新挂载、焦点与虚拟列表的位置不变）；内容就是这一行
 * 写进去的那份（`saved`）时保留行的原样（例如译文留空的写法）；还没写过的新行（原文为空）留在末尾。
 * `saved` 随之改为读回的内容。没有任何变化时返回 `prev` 本身
 */
export function reconcileRows(
  prev: Row[] | null,
  entries: readonly GlossaryEntry[],
  saved: Map<number, GlossaryEntry>,
): Row[] {
  const pool = new Map<string, Row[]>()
  for (const r of prev ?? []) {
    const source = r.source.trim()
    if (source) pool.set(source, [...(pool.get(source) ?? []), r])
  }
  const next: Row[] = entries.map((e) => {
    const reuse = pool.get(e.source)?.shift()
    if (!reuse) return { ...e, key: ++rowSeq }
    const mine = saved.get(reuse.key)
    return mine && sameEntry(mine, e) ? reuse : { ...e, key: reuse.key }
  })
  for (const r of prev ?? []) if (!r.source.trim()) next.push(r)
  saved.clear()
  for (const [i, e] of entries.entries()) {
    const row = next[i]
    if (row) saved.set(row.key, e)
  }
  if (prev && prev.length === next.length && prev.every((r, i) => r === next[i])) return prev
  return next
}

type Op =
  | { kind: 'upsert'; entry: GlossaryEntry; previous: string | null }
  | { kind: 'remove'; source: string }

/** 串行写入：按改动的先后逐条发出，同一时刻只有一条在途 */
const ops: Op[] = []
let writing: Promise<void> | null = null
/** 写入全部结束时通知（`failed`：其间有写入失败，本地记下的「已写入」不可信，需要重新读取） */
const idleListeners = new Set<(failed: boolean) => void>()

function enqueue(op: Op): void {
  ops.push(op)
  if (writing) return
  let failed = false
  writing = trackWrite(
    (async () => {
      while (ops.length > 0) {
        const next = ops.shift() as Op
        try {
          // oxlint-disable-next-line no-await-in-loop -- 有意串行
          await (next.kind === 'upsert'
            ? api.glossaryUpsert(next.entry, next.previous)
            : api.glossaryRemove(next.source))
        } catch (error) {
          failed = true
          notify(
            ipcErrorText(error, next.kind === 'upsert' ? 'glossary_upsert' : 'glossary_remove'),
            'danger',
          )
        }
      }
    })().finally(() => {
      writing = null
      for (const l of idleListeners) l(failed)
    }),
  )
}

/** 等待串行写入全部结束（导出前、测试） */
export async function settleGlossaryWrites(): Promise<void> {
  for (let current = writing; current; current = writing) {
    // oxlint-disable-next-line no-await-in-loop -- 有意串行：等完这一批再看有没有接着排进来的
    await current
  }
}

export function GlossaryEditor() {
  const t = useT()
  const [rows, setRows] = useState<Row[] | null>(null)
  const [query, setQuery] = useState('')
  const [busy, setBusy] = useState<'import' | 'export' | null>(null)
  const [focusKey, setFocusKey] = useState<number | null>(null)
  /** 正在编辑（焦点在其中）的行：搜索过滤时保留 */
  const [editingKey, setEditingKey] = useState<number | null>(null)
  const rowsRef = useRef<Row[] | null>(null)
  const editingRef = useRef<number | null>(null)
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null)
  /** 改过、还没写出去的行 */
  const dirty = useRef(new Set<number>())
  /** 每一行在术语表里的样子（最近一次读回或写出的内容）：决定写 upsert 还是 remove、带哪个原来的原文 */
  const saved = useRef(new Map<number, GlossaryEntry>())
  /** 收到了外部变化、但当时不便重新读取（有未写出的改动 / 写入在途 / 正在编辑某一行） */
  const stale = useRef(false)
  useLayoutEffect(() => {
    rowsRef.current = rows
    editingRef.current = editingKey
  })

  /**
   * 一行放弃了它写过的原文（删除、清空或改了原文），而另一行此刻的原文也是它：术语表里同一原文只有一条，Rust 已经把
   * 那一行合并进了这一条。改为写成那一行的内容，而不是删掉这一条或随改名带走——否则用户刚加的那一行会跟着消失
   */
  const handOver = useCallback((key: number, source: string): boolean => {
    const other = (rowsRef.current ?? []).find(
      (r) => r.key !== key && toEntry(r)?.source === source,
    )
    const entry = other ? toEntry(other) : null
    if (!other || !entry) return false
    enqueue({ kind: 'upsert', entry, previous: null })
    saved.current.set(other.key, entry)
    return true
  }, [])

  /** 立即写出改过的行（失焦、删除行、导入导出前、关窗前） */
  const flush = useCallback(() => {
    if (timer.current !== null) clearTimeout(timer.current)
    timer.current = null
    const current = rowsRef.current
    if (dirty.current.size === 0 || !current) return
    for (const row of current) {
      if (!dirty.current.has(row.key)) continue
      const entry = toEntry(row)
      const before = saved.current.get(row.key) ?? null
      if (!entry) {
        // 原文清空：等同于删掉这一条
        if (before && !handOver(row.key, before.source))
          enqueue({ kind: 'remove', source: before.source })
        saved.current.delete(row.key)
      } else if (!before || !sameEntry(before, entry)) {
        // 改了原文、而原来的原文此刻属于另一行：那一条交给那一行，这一行作为新条目写入（不带原来的原文，
        // 否则 Rust 会把那一条改名带走）
        const handed =
          before !== null && before.source !== entry.source && handOver(row.key, before.source)
        enqueue({ kind: 'upsert', entry, previous: handed ? null : (before?.source ?? null) })
        saved.current.set(row.key, entry)
      }
    }
    dirty.current.clear()
  }, [handOver])

  const schedule = useCallback(
    (key: number) => {
      dirty.current.add(key)
      if (timer.current !== null) clearTimeout(timer.current)
      timer.current = setTimeout(flush, COMMIT_DELAY_MS)
    },
    [flush],
  )

  /** 现在能不能用读回的内容替换行：没有未写出的改动、没有在途写入、焦点不在某一行里 */
  const quiet = useCallback(
    () =>
      dirty.current.size === 0 &&
      timer.current === null &&
      writing === null &&
      editingRef.current === null,
    [],
  )

  /** 重新读取（外部变化、导入之后）；不便替换时记下，等安静下来再读 */
  const refresh = useCallback(() => {
    if (!quiet()) {
      stale.current = true
      return
    }
    stale.current = false
    void api
      .glossaryGet()
      .then((entries) => {
        if (!quiet()) {
          stale.current = true
          return
        }
        setRows(reconcileRows(rowsRef.current, entries, saved.current))
      })
      .catch((error: unknown) => notify(ipcErrorText(error, 'glossary_get'), 'danger'))
  }, [quiet])

  useEffect(() => {
    let alive = true
    api
      .glossaryGet()
      .then((entries) => {
        if (alive) setRows(reconcileRows(rowsRef.current, entries, saved.current))
      })
      .catch((error: unknown) => {
        if (!alive) return
        setRows([])
        notify(ipcErrorText(error, 'glossary_get'), 'danger')
      })
    return () => {
      alive = false
    }
  }, [])

  // 别处改了术语表（其他设备同步、手改文件、导入）：重新读取；自己的写入全部结束后补上之前记下的那次
  useEffect(() => {
    const onIdle = (failed: boolean) => {
      if (failed || stale.current) refresh()
    }
    idleListeners.add(onIdle)
    let unlisten: (() => void) | null = null
    let disposed = false
    void on('glossary:changed', () => refresh()).then((un) => {
      if (disposed) un()
      else unlisten = un
    })
    return () => {
      disposed = true
      idleListeners.delete(onIdle)
      unlisten?.()
    }
  }, [refresh])

  // 卸载（切换分组 / 关闭编辑）时把未写的改动写出去；挂载期间可被统一提交（关窗前）
  useEffect(() => {
    const unregister = registerFlusher(flush)
    return () => {
      unregister()
      flush()
    }
  }, [flush])

  // 行的回调都是稳定的：打字只重渲染当前行
  const edit = useCallback(
    (key: number, patch: Partial<GlossaryEntry>) => {
      setRows((prev) => prev && prev.map((r) => (r.key === key ? { ...r, ...patch } : r)))
      schedule(key)
    },
    [schedule],
  )
  const remove = useCallback(
    (key: number) => {
      const next = (rowsRef.current ?? []).filter((r) => r.key !== key)
      rowsRef.current = next
      setRows(next)
      dirty.current.delete(key)
      // 删除立即写出（连同之前还没写的改动）
      flush()
      const before = saved.current.get(key)
      saved.current.delete(key)
      if (before && !handOver(key, before.source))
        enqueue({ kind: 'remove', source: before.source })
    },
    [flush, handOver],
  )
  const add = () => {
    const row: Row = { source: '', target: '', note: null, key: ++rowSeq }
    setFocusKey(row.key)
    setQuery('')
    setRows((prev) => (prev ? [...prev, row] : [row]))
  }
  const onRowFocus = useCallback((key: number) => {
    editingRef.current = key
    setEditingKey(key)
    // 新行已经拿到焦点：清掉「自动聚焦」标记，它之后重新挂载（过滤、虚拟列表滚出滚回）时不再抢焦点
    setFocusKey((k) => (k === key ? null : k))
  }, [])
  const onRowBlur = useCallback(
    (key: number) => {
      if (editingRef.current === key) editingRef.current = null
      setEditingKey((k) => (k === key ? null : k))
      flush()
      // 编辑期间错过的外部变化：没有写入要发时现在补上（有写入时等它们结束）
      if (stale.current) refresh()
    },
    [flush, refresh],
  )

  const importFile = async () => {
    setBusy('import')
    flush()
    try {
      const count = await api.glossaryImport()
      if (count > 0) {
        refresh()
        notify(t('settings.glossary.imported', { count, n: count }), 'success')
      }
    } catch (error) {
      notify(ipcErrorText(error, 'glossary_import'), 'danger')
    } finally {
      setBusy(null)
    }
  }
  const exportFile = async () => {
    setBusy('export')
    flush()
    try {
      await settleGlossaryWrites()
      const path = await api.glossaryExport()
      if (path) notify(t('settings.glossary.exported', { path }), 'success', { sticky: true })
    } catch (error) {
      notify(ipcErrorText(error, 'glossary_export'), 'danger')
    } finally {
      setBusy(null)
    }
  }

  const visible = useMemo(() => {
    if (!rows) return []
    const q = query.trim().toLowerCase()
    if (!q) return rows
    return rows.filter(
      (r) =>
        r.key === editingKey ||
        r.source.toLowerCase().includes(q) ||
        r.target.toLowerCase().includes(q) ||
        (r.note ?? '').toLowerCase().includes(q),
    )
  }, [rows, query, editingKey])

  const virtual = (rows?.length ?? 0) > VIRTUAL_AFTER

  return (
    <Section
      title={
        <span className="st-label-with-badge">
          {t('settings.glossary.title')}
          {rows && rows.length > 0 ? <Badge tone="neutral">{rows.length}</Badge> : null}
        </span>
      }
      description={t('settings.glossary.description')}
      action={
        <span className="st-section-actions">
          <Button
            size="sm"
            variant="ghost"
            icon={Upload}
            loading={busy === 'import'}
            onClick={() => void importFile()}
          >
            {t('settings.glossary.import')}
          </Button>
          <Button
            size="sm"
            variant="ghost"
            icon={Download}
            loading={busy === 'export'}
            disabled={!rows || rows.length === 0}
            onClick={() => void exportFile()}
          >
            {t('settings.glossary.export')}
          </Button>
        </span>
      }
      data-testid="glossary"
    >
      {rows === null ? (
        <div className="st-card-pad">
          <Skeleton lines={2} />
        </div>
      ) : (
        <div className="st-glossary">
          {rows.length > SEARCH_AFTER ? (
            <div className="st-glossary-search">
              <SearchField
                value={query}
                onChange={setQuery}
                placeholder={t('settings.glossary.search')}
              />
            </div>
          ) : null}
          {rows.length > 0 ? (
            <div className="st-glossary-head" aria-hidden="true">
              <span>{t('settings.glossary.source')}</span>
              <span>{t('settings.glossary.target')}</span>
              <span>{t('settings.glossary.note')}</span>
              <span />
            </div>
          ) : (
            <div className="st-card-empty" data-testid="glossary-empty">
              {t('settings.glossary.empty')}
            </div>
          )}
          {virtual ? (
            <VirtualRows
              rows={visible}
              focusKey={focusKey}
              onEdit={edit}
              onRemove={remove}
              onFocusRow={onRowFocus}
              onBlurRow={onRowBlur}
            />
          ) : (
            visible.map((r) => (
              <GlossaryRow
                key={r.key}
                row={r}
                focusOnMount={focusKey === r.key}
                onEdit={edit}
                onRemove={remove}
                onFocusRow={onRowFocus}
                onBlurRow={onRowBlur}
              />
            ))
          )}
          {rows.length > 0 && visible.length === 0 ? (
            <div className="st-card-empty">
              <Icon icon={Search} size={14} /> {t('common.state.no_results')}
            </div>
          ) : null}
          <div className="st-glossary-foot">
            <Button size="sm" variant="ghost" icon={Plus} onClick={add} data-testid="glossary-add">
              {t('settings.glossary.add')}
            </Button>
          </div>
        </div>
      )}
    </Section>
  )
}

interface RowCallbacks {
  onEdit: (key: number, patch: Partial<GlossaryEntry>) => void
  onRemove: (key: number) => void
  onFocusRow: (key: number) => void
  onBlurRow: (key: number) => void
}

/** 条目很多时：内部滚动、只渲染可见的行（行高固定） */
function VirtualRows({
  rows,
  focusKey,
  ...callbacks
}: { rows: readonly Row[]; focusKey: number | null } & RowCallbacks) {
  const scroller = useRef<HTMLDivElement>(null)
  const [edge, setEdge] = useState('none')
  // oxlint-disable-next-line react/incompatible-library -- 本项目不用 React Compiler；与 shared/ui/VirtualList 相同
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scroller.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 6,
    initialRect: { width: 560, height: VISIBLE_ROWS * ROW_HEIGHT },
  })
  // 新加的空行在末尾：只在它第一次出现时滚过去一次（它挂载时自动获得焦点）；之后行号变化（删行、过滤）不再跟着跳
  const scrolledFor = useRef<number | null>(null)
  const focusIndex = focusKey === null ? -1 : rows.findIndex((r) => r.key === focusKey)
  useEffect(() => {
    if (focusKey === null || focusIndex < 0 || scrolledFor.current === focusKey) return
    scrolledFor.current = focusKey
    virtualizer.scrollToIndex(focusIndex, { align: 'end' })
  }, [focusKey, focusIndex, virtualizer])
  // 上下边缘渐隐：还能往上 / 往下滚时显示（只在状态变化时重渲染）
  const updateEdge = () => {
    const el = scroller.current
    if (!el) return
    const top = el.scrollTop > 1
    const bottom = el.scrollTop + el.clientHeight < el.scrollHeight - 1
    const next = top && bottom ? 'both' : top ? 'top' : bottom ? 'bottom' : 'none'
    setEdge((prev) => (prev === next ? prev : next))
  }
  useLayoutEffect(updateEdge)
  return (
    <div className="st-glossary-viewport" data-edge={edge}>
      <div
        ref={scroller}
        className="st-glossary-scroll"
        style={{ height: Math.min(rows.length, VISIBLE_ROWS) * ROW_HEIGHT }}
        onScroll={updateEdge}
      >
        <div className="st-glossary-inner" style={{ height: virtualizer.getTotalSize() }}>
          {virtualizer.getVirtualItems().map((item) => {
            const r = rows[item.index]
            if (!r) return null
            return (
              <GlossaryRow
                key={r.key}
                row={r}
                focusOnMount={focusKey === r.key}
                offset={item.start}
                {...callbacks}
              />
            )
          })}
        </div>
      </div>
    </div>
  )
}

const GlossaryRow = memo(function GlossaryRow({
  row,
  focusOnMount,
  offset,
  onEdit,
  onRemove,
  onFocusRow,
  onBlurRow,
}: { row: Row; focusOnMount: boolean; offset?: number } & RowCallbacks) {
  const t = useT()
  const { key } = row
  return (
    <div
      className="st-glossary-row"
      data-virtual={offset !== undefined || undefined}
      style={offset !== undefined ? { transform: `translateY(${offset}px)` } : undefined}
      data-testid="glossary-row"
      onFocus={() => onFocusRow(key)}
      onBlur={(e) => {
        // 焦点在同一行的输入框之间移动不算离开
        if (!e.currentTarget.contains(e.relatedTarget as Node | null)) onBlurRow(key)
      }}
    >
      <TextField
        value={row.source}
        onChange={(v) => onEdit(key, { source: v })}
        placeholder={t('settings.glossary.source_placeholder')}
        aria-label={t('settings.glossary.source')}
        // oxlint-disable-next-line jsx-a11y/no-autofocus -- 用户刚点了「添加术语」：新行直接输入
        autoFocus={focusOnMount}
        data-testid="glossary-source"
      />
      <TextField
        value={row.target}
        onChange={(v) => onEdit(key, { target: v })}
        placeholder={row.source ? row.source : t('settings.glossary.target_placeholder')}
        aria-label={t('settings.glossary.target')}
        data-testid="glossary-target"
      />
      <TextField
        value={row.note ?? ''}
        onChange={(v) => onEdit(key, { note: v })}
        placeholder={t('settings.glossary.note_placeholder')}
        aria-label={t('settings.glossary.note')}
      />
      <IconButton
        icon={X}
        size="sm"
        label={t('common.action.remove')}
        className="st-glossary-remove"
        onClick={() => onRemove(key)}
      />
    </div>
  )
})
