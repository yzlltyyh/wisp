/**
 * Tab 焦点循环（每次 Tab 都由外壳接管）：按文档顺序移动、首尾回绕、跳过禁用 / inert / aria-hidden、焦点在外面时
 * 进入、焦点在序列外的元素上时从它的位置接着走
 */
import { afterEach, describe, expect, it } from 'vitest'
import { cycleFocusWithin } from '../focusLoop'

afterEach(() => {
  document.body.replaceChildren()
})

function setup(html: string): HTMLElement {
  const root = document.createElement('div')
  root.innerHTML = html
  document.body.append(root)
  // jsdom 没有布局：给每个元素一个布局盒
  for (const el of root.querySelectorAll<HTMLElement>('*'))
    el.getClientRects = () => [new DOMRect(0, 0, 10, 10)] as unknown as DOMRectList
  return root
}

describe('cycleFocusWithin', () => {
  it('每次都接管：按文档顺序经过全部控件（按钮也在内）；最后一个之后回到第一个；Shift+Tab 反之', () => {
    const root = setup(
      '<button id="a">a</button><button id="b" disabled>b</button><div inert><button id="c">c</button></div>' +
        '<span aria-hidden="true"><button id="d">d</button></span><textarea id="e"></textarea><button id="f">f</button>',
    )
    const el = (id: string) => root.querySelector<HTMLElement>(`#${id}`)!
    el('f').focus()
    expect(cycleFocusWithin(root, false)).toBe(true)
    expect(document.activeElement).toBe(el('a'))
    expect(cycleFocusWithin(root, false)).toBe(true)
    expect(document.activeElement).toBe(el('e'))
    expect(cycleFocusWithin(root, false)).toBe(true)
    expect(document.activeElement).toBe(el('f'))
    expect(cycleFocusWithin(root, true)).toBe(true)
    expect(document.activeElement).toBe(el('e'))
    expect(cycleFocusWithin(root, true)).toBe(true)
    expect(cycleFocusWithin(root, true)).toBe(true)
    expect(document.activeElement).toBe(el('f'))
  })

  it('焦点在序列外的元素（tabindex = -1）上：从它的位置接着走', () => {
    const root = setup(
      '<button id="a">a</button><h3 id="t" tabindex="-1">t</h3><button id="z">z</button>',
    )
    root.querySelector<HTMLElement>('#t')!.focus()
    expect(cycleFocusWithin(root, false)).toBe(true)
    expect(document.activeElement?.id).toBe('z')
    root.querySelector<HTMLElement>('#t')!.focus()
    expect(cycleFocusWithin(root, true)).toBe(true)
    expect(document.activeElement?.id).toBe('a')
  })

  it('焦点在外面（body）时进入第一个 / 最后一个；没有可聚焦的控件时不接管', () => {
    const root = setup('<button id="a">a</button><button id="z">z</button>')
    ;(document.activeElement as HTMLElement | null)?.blur()
    expect(cycleFocusWithin(root, true)).toBe(true)
    expect(document.activeElement?.id).toBe('z')
    expect(cycleFocusWithin(setup('<p>text</p>'), false)).toBe(false)
    expect(cycleFocusWithin(null, false)).toBe(false)
  })
})
