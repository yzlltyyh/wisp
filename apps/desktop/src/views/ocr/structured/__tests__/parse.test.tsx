/**
 * 结构化结果的预览：切块（Markdown / 表格 / 公式）、管道表格与 HTML 表格（合并单元格、不插入 HTML）、公式等宽显示、
 * 行内公式与金额、围栏代码块不解析、图片引用去掉；以及面板里「有没有结构」的轻量判定。
 */
import { render, screen, within } from '@testing-library/react'
import { describe, expect, it } from 'vitest'
import { registerOcrMessages } from '../../i18n'
import { hasStructure } from '../../model'
import { htmlTable, parseStructured, splitRow } from '../parse'
import { StructuredPreview } from '../Preview'

registerOcrMessages()

describe('切块', () => {
  it('管道表格：表头、对齐、数据行；单元格里转义的 | 与行内代码里的 | 不切开；行宽不齐时补空', () => {
    const blocks = parseStructured(
      [
        '## 标题',
        '',
        '| 名称 | 值 |',
        '| :--- | ---: |',
        '| a \\| b | `x|y` |',
        '| 只有一格 |',
      ].join('\n'),
    )
    expect(blocks.map((b) => b.kind)).toEqual(['md', 'table'])
    const table = blocks[1]
    if (table?.kind !== 'table') throw new Error('not a table')
    expect(table.rows[0]?.map((c) => [c.text, c.header, c.align])).toEqual([
      ['名称', true, 'left'],
      ['值', true, 'right'],
    ])
    expect(table.rows[1]?.map((c) => c.text)).toEqual(['a | b', '`x|y`'])
    expect(table.rows[2]?.map((c) => c.text)).toEqual(['只有一格', ''])
    expect(splitRow('|a|b|')).toEqual(['a', 'b'])
  })

  it('HTML 表格：去掉外层的 div / html / body，保留合并单元格（上限 50），只取纯文本', () => {
    const src =
      '<div style="text-align: center;"><html><body><table border=1><tr><td rowspan=2>平台</td>' +
      '<td colspan="9999">安装包</td></tr><tr><td><b>大小</b><img src="x" onerror="alert(1)"></td></tr>' +
      '</table></body></html></div>'
    const blocks = parseStructured(`前言\n${src}\n结尾`)
    expect(blocks.map((b) => b.kind)).toEqual(['md', 'table', 'md'])
    const rows = htmlTable(src)
    expect(rows?.[0]?.map((c) => [c.text, c.rowSpan, c.colSpan])).toEqual([
      ['平台', 2, 1],
      ['安装包', 1, 50],
    ])
    expect(rows?.[1]?.[0]?.text).toBe('大小')
  })

  it('公式块：$$…$$（跨行或同一行）与 \\[…\\] 保留源码', () => {
    const blocks = parseStructured('a\n$$\nx^2 + y^2\n$$\n\\[ \\frac{1}{2} \\]\n$$E=mc^2$$')
    expect(blocks).toEqual([
      { kind: 'md', text: 'a' },
      { kind: 'math', tex: 'x^2 + y^2' },
      { kind: 'math', tex: '\\frac{1}{2}' },
      { kind: 'math', tex: 'E=mc^2' },
    ])
  })

  it('行内公式按行内代码等宽显示；金额不受影响；围栏代码块里的表格不解析；图片引用去掉', () => {
    const [md] = parseStructured('按 $\\alpha_1$ 计算，价格 $5 和 $10。![图](imgs/a.jpg)')
    expect(md).toEqual({ kind: 'md', text: '按 `$\\alpha_1$` 计算，价格 $5 和 $10。' })
    const fenced = parseStructured('```\n| a | b |\n| --- | --- |\n```')
    expect(fenced.map((b) => b.kind)).toEqual(['md'])
  })

  it('面板里的判定：有管道表格 / HTML 表格 / 公式块才算有结构；分隔线与普通竖线不算', () => {
    expect(hasStructure('| a | b |\n| --- | --- |\n| 1 | 2 |')).toBe(true)
    expect(hasStructure('a\n|---|\n')).toBe(true)
    expect(hasStructure('<table><tr><td>1</td></tr></table>')).toBe(true)
    expect(hasStructure('$$\nx\n$$')).toBe(true)
    expect(hasStructure('标题\n---\n正文 a | b')).toBe(false)
    expect(hasStructure('价格 $5')).toBe(false)
  })
})

describe('渲染', () => {
  it('表格画成真正的 <table>（表头在 thead、合并单元格带 rowspan / colspan），公式等宽并可复制', () => {
    render(
      <StructuredPreview
        text={
          '| 指标 | 值 |\n| --- | --- |\n| **p95** | 96 ms |\n\n<table><tr><td rowspan=2>平台</td><td>大小</td></tr><tr><td>9.8 MB</td></tr></table>\n\n$$\nT = \\mu\n$$'
        }
        label="识别出的文字"
        data-testid="preview"
      />,
    )
    const preview = screen.getByTestId('preview')
    const tables = preview.querySelectorAll('table')
    expect(tables).toHaveLength(2)
    expect(
      within(tables[0] as HTMLElement)
        .getAllByRole('columnheader')
        .map((x) => x.textContent),
    ).toEqual(['指标', '值'])
    expect(tables[0]?.querySelector('tbody strong')?.textContent).toBe('p95')
    expect(tables[1]?.querySelector('td')?.getAttribute('rowspan')).toBe('2')
    expect(preview.querySelector('.ocr-math-tex')?.textContent).toBe('T = \\mu')
    expect(screen.getByTestId('ocr-math-copy')).toHaveAccessibleName(/复制公式|Copy formula/)
    // 不插入任何 HTML：没有 script / img / 带事件属性的元素
    expect(preview.querySelector('img, script, [onerror]')).toBeNull()
  })
})
