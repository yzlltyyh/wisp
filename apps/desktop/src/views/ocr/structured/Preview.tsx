/**
 * 结构化识别结果的预览（懒加载，见 ../structuredModule.ts）：普通 Markdown 交给 Markdown-lite 渲染（与译文区同一套样式），
 * 表格画成真正的表格（表头、对齐、合并单元格；横向放不下时在表格内左右滚动），公式以等宽源码显示并带「复制公式」。
 * 只读、可以选中复制；要修改时切回源码（可编辑的文本框）。全部用 React 元素与 textContent 生成，不插入任何 HTML。
 */
import './structured.css'
import { Sigma } from 'lucide-react'
import { memo, useLayoutEffect, useMemo, useRef, type Ref, type UIEvent } from 'react'
import { useT } from '../../../shared/i18n/useT'
import { parseInline, parseMdLite, type Inline } from '../../../shared/markdown/mdlite'
import { renderBlocks } from '../../../shared/markdown/render'
import { CopyButton, Icon } from '../../../shared/ui'
import { parseStructured, type Cell } from './parse'

/** 一段普通 Markdown：沿用 Markdown-lite 的 DOM 渲染（静态） */
function MdSegment({ text }: { text: string }) {
  const host = useRef<HTMLDivElement>(null)
  useLayoutEffect(() => {
    if (host.current) renderBlocks(parseMdLite(text), host.current)
  }, [text])
  return <div ref={host} className="ui-md ocr-preview-md" />
}

/** 单元格里的行内样式（粗体、斜体、行内代码） */
function Runs({ text }: { text: string }) {
  const runs: Inline[] = parseInline(text)
  return (
    <>
      {runs.map((r, i) => {
        // 行内片段没有稳定 id：按位置区分（内容变化时整段重新解析）
        const key = `${i}:${r.text}`
        if (r.code) return <code key={key}>{r.text}</code>
        if (r.bold) return <strong key={key}>{r.text}</strong>
        if (r.italic) return <em key={key}>{r.text}</em>
        return <span key={key}>{r.text}</span>
      })}
    </>
  )
}

function Table({ rows }: { rows: readonly Cell[][] }) {
  const t = useT()
  // 连续的表头行放进 thead（HTML 表格可能有多行表头）
  let headRows = 0
  while (headRows < rows.length && rows[headRows]?.every((c) => c.header)) headRows++
  const row = (cells: readonly Cell[], r: number) => (
    // oxlint-disable-next-line react/no-array-index-key -- 表格的行与单元格只按位置区分
    <tr key={r}>
      {cells.map((c, k) => {
        const Tag = c.header ? 'th' : 'td'
        return (
          <Tag
            // oxlint-disable-next-line react/no-array-index-key -- 同上
            key={k}
            colSpan={c.colSpan > 1 ? c.colSpan : undefined}
            rowSpan={c.rowSpan > 1 ? c.rowSpan : undefined}
            style={c.align ? { textAlign: c.align } : undefined}
          >
            <Runs text={c.text} />
          </Tag>
        )
      })}
    </tr>
  )
  return (
    <section className="ocr-table-wrap" aria-label={t('ocr.preview.table')}>
      <table className="ocr-table">
        {headRows > 0 ? <thead>{rows.slice(0, headRows).map(row)}</thead> : null}
        <tbody>{rows.slice(headRows).map((cells, r) => row(cells, r + headRows))}</tbody>
      </table>
    </section>
  )
}

function MathBlock({ tex }: { tex: string }) {
  const t = useT()
  return (
    <figure className="ocr-math">
      <figcaption className="ocr-math-head">
        <Icon icon={Sigma} size={12} />
        <span className="ocr-math-label">{t('ocr.preview.formula')}</span>
        <CopyButton text={tex} label={t('ocr.preview.copy_formula')} data-testid="ocr-math-copy" />
      </figcaption>
      <pre className="ocr-math-tex">
        <code>{tex}</code>
      </pre>
    </figure>
  )
}

export interface StructuredPreviewProps {
  text: string
  label: string
  className?: string
  /** 滚动容器（视图据此同步边缘渐隐） */
  scrollRef?: Ref<HTMLDivElement>
  onScroll?: (e: UIEvent<HTMLDivElement>) => void
  'data-testid'?: string
}

/** 结构化结果的只读预览 */
export const StructuredPreview = memo(function StructuredPreview({
  text,
  label,
  className,
  scrollRef,
  onScroll,
  'data-testid': testId,
}: StructuredPreviewProps) {
  const blocks = useMemo(() => parseStructured(text), [text])
  return (
    <div
      ref={scrollRef}
      onScroll={onScroll}
      className={className ? `ocr-preview ${className}` : 'ocr-preview'}
      role="document"
      aria-label={label}
      data-testid={testId}
    >
      {blocks.map((b, i) =>
        b.kind === 'md' ? (
          // oxlint-disable-next-line react/no-array-index-key -- 块只按位置区分（内容变化时整体重新解析）
          <MdSegment key={i} text={b.text} />
        ) : b.kind === 'table' ? (
          // oxlint-disable-next-line react/no-array-index-key -- 同上
          <Table key={i} rows={b.rows} />
        ) : (
          // oxlint-disable-next-line react/no-array-index-key -- 同上
          <MathBlock key={i} tex={b.tex} />
        ),
      )}
    </div>
  )
})
