/**
 * 面板拖动：从头部与空白处按下左键开始拖动（`panel_drag_start`），交互元素、可选中的正文、可编辑区、滚动条、
 * 显式标记的区域与 portal 冒泡上来的目标不触发；只认鼠标 / 笔的主键；发起拖动时紧接着的 mousedown 阻止默认行为
 * （不选中文字、焦点留在原处），没有发起时不碰默认行为；固定窗口宿主（enabled = false）不接。
 */
import { fireEvent, render, screen } from '@testing-library/react'
import { createPortal } from 'react-dom'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { setTransport, type Transport } from '../../ipc'
import { canStartPanelDrag, usePanelDrag } from '../usePanelDrag'

let calls: Array<[string, Record<string, unknown>]>

const transport = (): Transport => ({
  kind: 'mock',
  invoke: <T,>(cmd: string, args?: Record<string, unknown>) => {
    calls.push([cmd, args ?? {}])
    return Promise.resolve(undefined as T)
  },
  openStream: () => Promise.reject(new Error('unexpected stream')),
  listen: () => Promise.resolve(() => undefined),
  window: {
    label: 'panel',
    startDragging: () => Promise.resolve(),
    close: () => Promise.resolve(),
  },
  protocolUrl: (p) => p,
})

beforeEach(() => {
  calls = []
  setTransport(transport())
})
afterEach(() => setTransport(transport()))

const drags = () => calls.filter(([c]) => c === 'panel_drag_start')

function Card({ enabled = true, portal = false }: { enabled?: boolean; portal?: boolean }) {
  const drag = usePanelDrag(7, enabled)
  return (
    // oxlint-disable-next-line jsx-a11y/no-static-element-interactions -- 测试用的卡片
    <div data-testid="root" onPointerDown={drag?.onPointerDown} onMouseDown={drag?.onMouseDown}>
      <style>
        {'.sel { user-select: text; -webkit-user-select: text } .off { user-select: none }'}
      </style>
      <div className="head" data-testid="head">
        <span data-testid="title">标题</span>
        <span data-testid="spacer" />
        <button type="button" aria-label="关闭" data-testid="button">
          <svg data-testid="icon" />
        </button>
        <span role="menuitem" tabIndex={-1} data-testid="menuitem">
          菜单项
        </span>
        <a href="#x" data-testid="link">
          链接
        </a>
      </div>
      <div className="ui-stream" data-testid="stream">
        <p data-testid="stream-p">正文</p>
      </div>
      <div className="sel" data-testid="css-selectable">
        <strong data-testid="css-selectable-inner">可选中</strong>
        <span className="off" data-testid="css-unselectable">
          不可选中的角标
        </span>
      </div>
      <textarea data-testid="textarea" defaultValue="识别结果" />
      <input data-testid="input" />
      {/* oxlint-disable-next-line jsx-a11y/no-noninteractive-tabindex -- 与翻译视图状态行的 tooltip 触发点一样可聚焦 */}
      <span tabIndex={0} data-testid="focusable">
        状态
      </span>
      <div data-no-drag data-testid="no-drag">
        <span data-testid="no-drag-inner">x</span>
      </div>
      <footer data-testid="footer">
        <span data-testid="hint">提示</span>
      </footer>
      {portal ? createPortal(<div data-testid="portal">浮层</div>, document.body) : null}
    </div>
  )
}

const press = (testId: string, init: PointerEventInit = {}) =>
  fireEvent.pointerDown(screen.getByTestId(testId), {
    button: 0,
    pointerType: 'mouse',
    ...init,
  })

describe('canStartPanelDrag', () => {
  it('头部、标题、空白、提示条可以开始拖动；交互元素、正文、可编辑区、显式标记的区域不行', () => {
    render(<Card />)
    const root = screen.getByTestId('root')
    const can = (id: string) => canStartPanelDrag(screen.getByTestId(id), root)
    for (const id of ['root', 'head', 'title', 'spacer', 'footer', 'hint', 'css-unselectable']) {
      expect(can(id), id).toBe(true)
    }
    for (const id of [
      'button',
      'icon',
      'menuitem',
      'link',
      'stream',
      'stream-p',
      'css-selectable',
      'css-selectable-inner',
      'textarea',
      'input',
      'focusable',
      'no-drag',
      'no-drag-inner',
    ]) {
      expect(can(id), id).toBe(false)
    }
  })

  it('不在绑定元素子树里的目标（portal 冒泡上来的浮层）不拖动', () => {
    render(<Card portal />)
    expect(canStartPanelDrag(screen.getByTestId('portal'), screen.getByTestId('root'))).toBe(false)
    expect(canStartPanelDrag(null, screen.getByTestId('root'))).toBe(false)
  })

  it('按在可滚动元素的滚动条上不拖动', () => {
    render(<Card />)
    const root = screen.getByTestId('root')
    const footer = screen.getByTestId('footer')
    // 竖直可滚动、经典滚动条 12 px：内容区宽 188，元素宽 200
    Object.defineProperties(footer, {
      scrollHeight: { configurable: true, value: 400 },
      clientHeight: { configurable: true, value: 100 },
      scrollWidth: { configurable: true, value: 188 },
      clientWidth: { configurable: true, value: 188 },
    })
    footer.getBoundingClientRect = () => DOMRect.fromRect({ x: 0, y: 0, width: 200, height: 100 })
    expect(canStartPanelDrag(footer, root, { x: 194, y: 50 })).toBe(false)
    expect(canStartPanelDrag(footer, root, { x: 100, y: 50 })).toBe(true)
  })
})

describe('usePanelDrag', () => {
  it('空白处按下左键：发起 panel_drag_start（带会话 id），紧接着的 mousedown 阻止默认行为', () => {
    render(<Card />)
    press('spacer')
    expect(drags()).toEqual([['panel_drag_start', { session_id: 7 }]])
    const down = fireEvent.mouseDown(screen.getByTestId('spacer'), { button: 0 })
    expect(down).toBe(false)
  })

  it('按钮、正文、文本框上按下不拖动，也不阻止默认行为（焦点、选中照常）', () => {
    render(<Card />)
    for (const id of ['button', 'stream-p', 'textarea', 'css-selectable-inner']) {
      press(id)
      expect(fireEvent.mouseDown(screen.getByTestId(id), { button: 0 }), id).toBe(true)
    }
    expect(drags()).toHaveLength(0)
  })

  it('只认鼠标 / 笔的主键：右键、触摸不拖动；笔可以', () => {
    render(<Card />)
    press('spacer', { button: 2 })
    press('spacer', { pointerType: 'touch' })
    expect(drags()).toHaveLength(0)
    press('spacer', { pointerType: 'pen' })
    expect(drags()).toHaveLength(1)
  })

  it('内层已经处理过这次按下（defaultPrevented）时不拖动', () => {
    render(<Card />)
    const spacer = screen.getByTestId('spacer')
    spacer.addEventListener('pointerdown', (e) => e.preventDefault())
    press('spacer')
    expect(drags()).toHaveLength(0)
  })

  it('固定窗口宿主（enabled = false）：不接', () => {
    render(<Card enabled={false} />)
    press('spacer')
    expect(drags()).toHaveLength(0)
    expect(fireEvent.mouseDown(screen.getByTestId('spacer'), { button: 0 })).toBe(true)
  })
})
