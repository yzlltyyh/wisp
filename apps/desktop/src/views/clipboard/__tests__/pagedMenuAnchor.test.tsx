/**
 * 分页菜单锚在固定矩形上（右键菜单的指针位置，剪贴板的「粘贴为… / 提取」子页要用）：能打开、选择后关闭，
 * 没有触发器元素时焦点还给 `returnFocus`，点外面关闭。
 */
import { act, fireEvent, render, screen } from '@testing-library/react'
import { createRef } from 'react'
import { describe, expect, it, vi } from 'vitest'
import { MotionRoot } from '../../../shared/motion/MotionRoot'
import { PagedMenu, type MenuPage } from '../../../shared/ui'

function page(onPick: () => void): MenuPage {
  return {
    key: 'root',
    label: 'Actions',
    items: [{ id: 'paste', label: 'Paste', onSelect: onPick }],
    'data-testid': 'point-menu',
  }
}

describe('PagedMenu · 矩形锚点', () => {
  it('在指针位置打开；选择后关闭并把焦点还给 returnFocus', () => {
    const onPick = vi.fn<() => void>()
    const onClose = vi.fn<() => void>()
    const back = createRef<HTMLInputElement>()
    render(
      <MotionRoot>
        <input ref={back} data-testid="back" />
        <PagedMenu
          open
          onClose={onClose}
          anchor={{ left: 120, top: 80, width: 0, height: 0 }}
          returnFocus={back}
          page={page(onPick)}
          direction={1}
        />
      </MotionRoot>,
    )
    expect(screen.getByTestId('point-menu').getAttribute('role')).toBe('menu')
    act(() => screen.getByTestId('point-menu-item-paste').click())
    expect(onPick).toHaveBeenCalledTimes(1)
    expect(onClose).toHaveBeenCalledTimes(1)
    expect(document.activeElement).toBe(back.current)
  })

  it('点菜单外面关闭（矩形锚点没有触发器元素可以排除）', () => {
    const onClose = vi.fn<() => void>()
    render(
      <MotionRoot>
        <button type="button" data-testid="outside">
          outside
        </button>
        <PagedMenu
          open
          onClose={onClose}
          anchor={{ left: 10, top: 10, width: 0, height: 0 }}
          page={page(() => {})}
          direction={1}
        />
      </MotionRoot>,
    )
    fireEvent.pointerDown(screen.getByTestId('outside'))
    expect(onClose).toHaveBeenCalledTimes(1)
  })
})
