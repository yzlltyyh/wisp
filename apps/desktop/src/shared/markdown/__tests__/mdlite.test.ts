/**
 * Markdown-lite 解析：行内定界符、乐观渲染、块级结构。
 */
import { describe, expect, it } from 'vitest'
import { parseInline, parseMdLite, type Inline } from '../mdlite'

const runs = (src: string, optimistic = false) =>
  parseInline(src, optimistic).map((r: Inline) => [
    r.text,
    `${r.bold ? 'b' : ''}${r.italic ? 'i' : ''}${r.code ? 'c' : ''}`,
  ])

describe('parseInline', () => {
  it('粗体、斜体、行内代码', () => {
    expect(runs('a **b** c')).toEqual([
      ['a ', ''],
      ['b', 'b'],
      [' c', ''],
    ])
    expect(runs('*x* and _y_')).toEqual([
      ['x', 'i'],
      [' and ', ''],
      ['y', 'i'],
    ])
    expect(runs('use `a*b*c` here')).toEqual([
      ['use ', ''],
      ['a*b*c', 'c'],
      [' here', ''],
    ])
    expect(runs('***both***')).toEqual([['both', 'bi']])
  })

  it('单词内的下划线、两侧是空白的星号原样显示', () => {
    expect(runs('snake_case_name')).toEqual([['snake_case_name', '']])
    expect(runs('5 * 3 = 15')).toEqual([['5 * 3 = 15', '']])
    expect(runs('\\*not italic\\*')).toEqual([['*not italic*', '']])
  })

  it('未闭合的定界符：最终渲染原样显示，流式中按已闭合乐观渲染', () => {
    expect(runs('a **bol')).toEqual([['a **bol', '']])
    expect(runs('a **bol', true)).toEqual([
      ['a ', ''],
      ['bol', 'b'],
    ])
    expect(runs('x `co', true)).toEqual([
      ['x ', ''],
      ['co', 'c'],
    ])
    expect(runs('x `co')).toEqual([['x `co', '']])
  })

  it('自然风格里的反引号成对时才是代码（natural 中单个反引号原样显示）', () => {
    expect(runs("it's `fine")).toEqual([["it's `fine", '']])
  })
})

describe('parseMdLite', () => {
  it('标题、段落、列表、代码块、分隔线、表格', () => {
    const blocks = parseMdLite(
      [
        '# Title',
        '',
        'para line 1',
        'para line 2',
        '',
        '- a',
        '- b',
        '',
        '1. one',
        '2. two',
        '',
        '```ts',
        'const x = 1',
        '',
        'const y = 2',
        '```',
        '',
        '---',
        '| a | b |',
        '|---|---|',
      ].join('\n'),
    )
    expect(blocks.map((b) => b.kind)).toEqual([
      'heading',
      'paragraph',
      'list',
      'list',
      'code',
      'rule',
      'table',
    ])
    const code = blocks[4]
    expect(code).toMatchObject({ kind: 'code', lang: 'ts', text: 'const x = 1\n\nconst y = 2' })
    expect(blocks[1]).toMatchObject({ kind: 'paragraph' })
  })

  it('空行之后同类型的列表项继续同一个列表', () => {
    const blocks = parseMdLite('- a\n\n- b\n\ntext')
    expect(blocks.map((b) => b.kind)).toEqual(['list', 'paragraph'])
    expect(blocks[0]).toMatchObject({ kind: 'list', items: [expect.anything(), expect.anything()] })
  })

  it('setext 标题', () => {
    expect(parseMdLite('Title\n===')).toMatchObject([{ kind: 'heading', level: 1 }])
    expect(parseMdLite('Sub\n---')).toMatchObject([{ kind: 'heading', level: 2 }])
    expect(parseMdLite('a\n\n---')).toMatchObject([{ kind: 'paragraph' }, { kind: 'rule' }])
  })

  it('未闭合的围栏按代码块显示（看到围栏行就切换样式）', () => {
    expect(parseMdLite('```py\nprint(1)')).toMatchObject([
      { kind: 'code', lang: 'py', text: 'print(1)' },
    ])
  })

  it('有序列表的起始序号', () => {
    expect(parseMdLite('3. c\n4. d')).toMatchObject([{ kind: 'list', ordered: true, start: 3 }])
  })
})
