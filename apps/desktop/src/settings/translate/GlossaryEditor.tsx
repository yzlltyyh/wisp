/**
 * 术语表：表格编辑 source / target / note；source 与 target 相同表示保持不译；
 * note 只在这里显示、不发给模型；导入（JSON / CSV / TSV，Rust 弹系统对话框）/ 导出；条目多时可搜索。
 *
 * - 写入：停顿后整体 `glossary_set`（空 source 的行不写入）；**串行**——在途时只记住最新一份，返回后再发，
 *   旧内容不会后到覆盖新内容；挂载期间登记到 writes.ts（关窗前提交并等待）；
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
import { api } from '../../shared/ipc'
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

interface Row extends GlossaryEntry {
  /** 本地行 id（React key；新增的空行还没有 source） */
  key: number
}

let rowSeq = 0
const toRows = (entries: readonly GlossaryEntry[]): Row[] =>
  entries.map((e) => ({ ...e, key: ++rowSeq }))

function toEntries(rows: readonly Row[]): GlossaryEntry[] {
  return rows
    .filter((r) => r.source.trim())
    .map((r) => ({
      source: r.source.trim(),
      // 译文留空 = 与原文相同 = 保持不译
      target: r.target.trim() || r.source.trim(),
      note: r.note?.trim() || null,
    }))
}

/** 串行写入：同一时刻只有一份 glossary_set 在途，期间的改动只保留最新的一份 */
let writing: Promise<void> | null = null
let nextEntries: GlossaryEntry[] | null = null

function writeGlossary(entries: GlossaryEntry[]): void {
  nextEntries = entries
  if (writing) return
  writing = trackWrite(
    (async () => {
      while (nextEntries) {
        const batch = nextEntries
        nextEntries = null
        try {
          // oxlint-disable-next-line no-await-in-loop -- 有意串行
          await api.glossarySet(batch)
        } catch (error) {
          notify(ipcErrorText(error, 'glossary_set'), 'danger')
        }
      }
    })().finally(() => {
      writing = null
    }),
  )
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
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const dirty = useRef(false)
  useLayoutEffect(() => {
    rowsRef.current = rows
  })

  /** 立即写出未写的改动（失焦、删除行、导入导出前、关窗前） */
  const flush = useCallback(() => {
    if (timer.current !== null) clearTimeout(timer.current)
    timer.current = null
    if (!dirty.current || !rowsRef.current) return
    dirty.current = false
    writeGlossary(toEntries(rowsRef.current))
  }, [])

  const schedule = useCallback(() => {
    dirty.current = true
    if (timer.current !== null) clearTimeout(timer.current)
    timer.current = setTimeout(flush, COMMIT_DELAY_MS)
  }, [flush])

  const reload = () =>
    api
      .glossaryGet()
      .then((entries) => setRows(toRows(entries)))
      .catch((error: unknown) => {
        setRows([])
        notify(ipcErrorText(error, 'glossary_get'), 'danger')
      })

  useEffect(() => {
    let alive = true
    api
      .glossaryGet()
      .then((entries) => alive && setRows(toRows(entries)))
      .catch((error: unknown) => {
        if (!alive) return
        setRows([])
        notify(ipcErrorText(error, 'glossary_get'), 'danger')
      })
    return () => {
      alive = false
    }
  }, [])

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
      schedule()
    },
    [schedule],
  )
  const remove = useCallback((key: number) => {
    const next = (rowsRef.current ?? []).filter((r) => r.key !== key)
    rowsRef.current = next
    setRows(next)
    // 删除立即写出（连同之前还没写的改动）
    if (timer.current !== null) clearTimeout(timer.current)
    timer.current = null
    dirty.current = false
    writeGlossary(toEntries(next))
  }, [])
  const add = () => {
    const row: Row = { source: '', target: '', note: null, key: ++rowSeq }
    setFocusKey(row.key)
    setQuery('')
    setRows((prev) => (prev ? [...prev, row] : [row]))
  }
  const onRowFocus = useCallback((key: number) => {
    setEditingKey(key)
    // 新行已经拿到焦点：清掉「自动聚焦」标记，它之后重新挂载（过滤、虚拟列表滚出滚回）时不再抢焦点
    setFocusKey((k) => (k === key ? null : k))
  }, [])
  const onRowBlur = useCallback(
    (key: number) => {
      setEditingKey((k) => (k === key ? null : k))
      flush()
    },
    [flush],
  )

  const importFile = async () => {
    setBusy('import')
    flush()
    try {
      const count = await api.glossaryImport()
      if (count > 0) {
        await reload()
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
      if (writing) await writing
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
