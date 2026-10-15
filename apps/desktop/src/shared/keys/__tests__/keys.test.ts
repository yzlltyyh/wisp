/**
 * 快捷键显示（与 Rust hotkeys::display 共用 fixture）与键盘路由。
 */
import { afterEach, describe, expect, it, vi } from 'vitest'
import cases from '../__fixtures__/format-cases.json'
import { canonicalCombo, comboFromEvent, matchesEvent, parseCombo } from '../combo'
import { formatHotkey, hotkeyParts, hotkeySpeech } from '../formatHotkey'
import { activeScopes, dispatch, registerScope } from '../keyRouter'

describe('formatHotkey（共用 fixture）', () => {
  for (const c of cases) {
    it(`${c.combo}`, () => {
      expect(formatHotkey(c.combo, 'macos')).toBe(c.macos)
      expect(formatHotkey(c.combo, 'windows')).toBe(c.windows)
      expect(formatHotkey(c.combo, 'linux')).toBe(c.linux)
    })
  }

  it('fixture 覆盖足够多的组合（三端各 ≥ 10）', () => {
    expect(cases.length).toBeGreaterThanOrEqual(10)
  })

  it('Windows / Linux 界面上没有 ⌘ / ⌥ / ⌃ / ⇧', () => {
    for (const c of cases) expect(c.windows + c.linux).not.toMatch(/[⌘⌥⌃⇧]/)
  })

  it('Mod 按平台展开；hotkeyParts 逐键帽', () => {
    expect(formatHotkey('Mod+KeyC', 'macos')).toBe('⌘C')
    expect(formatHotkey('Mod+KeyC', 'windows')).toBe('Ctrl+C')
    expect(hotkeyParts('Mod+Shift+Digit1', 'macos')).toEqual(['⇧', '⌘', '1'])
    expect(hotkeyParts('Mod+Shift+Digit1', 'linux')).toEqual(['Ctrl', 'Shift', '1'])
  })

  it('朗读文本：macOS 用键名（Option / Command / Return），Windows / Linux 用键帽文字', () => {
    expect(hotkeySpeech('Alt+KeyD', 'macos')).toBe('Option D')
    expect(hotkeySpeech('Control+Alt+Shift+Super+Enter', 'macos')).toBe(
      'Control Option Shift Command Return',
    )
    expect(hotkeySpeech('Alt+Backspace', 'macos')).toBe('Option Delete')
    expect(hotkeySpeech('Control+Alt+KeyD', 'windows')).toBe('Ctrl Alt D')
    expect(hotkeySpeech('Super+Alt+KeyD', 'linux')).toBe('Super Alt D')
  })
})

describe('combo', () => {
  it('别名与顺序规范化', () => {
    expect(canonicalCombo('shift+cmd+v', 'macos')).toBe('Shift+Super+KeyV')
    expect(canonicalCombo('Option+Ctrl+d', 'macos')).toBe('Control+Alt+KeyD')
    expect(canonicalCombo('CmdOrCtrl+,', 'windows')).toBe('Control+Comma')
    expect(parseCombo('Esc', 'linux').code).toBe('Escape')
  })

  it('comboFromEvent：单独的修饰键返回 null；⌥ 产生的特殊字符不影响（用 code）', () => {
    const mod = new KeyboardEvent('keydown', { key: 'Alt', code: 'AltLeft', altKey: true })
    expect(comboFromEvent(mod, 'macos')).toBeNull()
    const e = new KeyboardEvent('keydown', { key: '∂', code: 'KeyD', altKey: true, shiftKey: true })
    expect(comboFromEvent(e, 'macos')).toBe('Alt+Shift+KeyD')
  })

  it('应用内快捷键按布局产生的字符匹配，非拉丁字符 / ⌥ 特殊字符时退回物理键位', () => {
    const ev = (init: KeyboardEventInit) => new KeyboardEvent('keydown', init)
    const cmdW = parseCombo('Mod+KeyW', 'macos')
    // AZERTY：W 在物理 KeyZ 上
    expect(matchesEvent(cmdW, ev({ key: 'w', code: 'KeyZ', metaKey: true }))).toBe(true)
    expect(matchesEvent(cmdW, ev({ key: 'z', code: 'KeyW', metaKey: true }))).toBe(false)
    // Dvorak：C 在物理 KeyI 上；逗号在物理 KeyW 上，物理 Comma 键产生 w
    const cmdC = parseCombo('Mod+KeyC', 'macos')
    expect(matchesEvent(cmdC, ev({ key: 'c', code: 'KeyI', metaKey: true }))).toBe(true)
    const cmdComma = parseCombo('Mod+Comma', 'macos')
    expect(matchesEvent(cmdComma, ev({ key: ',', code: 'KeyW', metaKey: true }))).toBe(true)
    expect(matchesEvent(cmdComma, ev({ key: 'w', code: 'Comma', metaKey: true }))).toBe(false)
    // 俄文布局：Ctrl+С（西里尔字母）→ 物理 KeyC
    const ctrlC = parseCombo('Mod+KeyC', 'windows')
    expect(matchesEvent(ctrlC, ev({ key: 'с', code: 'KeyC', ctrlKey: true }))).toBe(true)
    // ⌥ 产生的特殊字符 → 物理键位
    expect(
      matchesEvent(parseCombo('Alt+KeyD', 'macos'), ev({ key: '∂', code: 'KeyD', altKey: true })),
    ).toBe(true)
    // Shift 改变了标点（US：Shift+, = <）→ 物理键位
    const shiftComma = parseCombo('Mod+Shift+Comma', 'windows')
    expect(
      matchesEvent(shiftComma, ev({ key: '<', code: 'Comma', ctrlKey: true, shiftKey: true })),
    ).toBe(true)
    // 修饰键必须完全一致；数字与方向键按物理键位
    expect(matchesEvent(cmdW, ev({ key: 'w', code: 'KeyW', metaKey: true, shiftKey: true }))).toBe(
      false,
    )
    expect(
      matchesEvent(
        parseCombo('Mod+Digit1', 'windows'),
        ev({ key: '&', code: 'Digit1', ctrlKey: true }),
      ),
    ).toBe(true)
  })
})

function key(init: KeyboardEventInit & { isComposing?: boolean; keyCode?: number }): KeyboardEvent {
  const e = new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...init })
  if (init.keyCode !== undefined) Object.defineProperty(e, 'keyCode', { value: init.keyCode })
  return e
}

const cleanups: Array<() => void> = []
afterEach(() => {
  for (const c of cleanups.splice(0)) c()
  document.body.textContent = ''
})

describe('keyRouter', () => {
  it('优先级：overlay > view > global；返回 false 交给下一层（Esc：先清空搜索，再关闭）', () => {
    const order: string[] = []
    let query = 'abc'
    cleanups.push(
      registerScope('global', [{ combo: 'Escape', run: () => void order.push('close') }], {
        layer: 'global',
      }),
      registerScope('clipboard', [
        {
          combo: 'Escape',
          run: () => {
            if (!query) return false
            query = ''
            order.push('clear')
          },
        },
      ]),
    )
    dispatch(key({ key: 'Escape', code: 'Escape' }))
    dispatch(key({ key: 'Escape', code: 'Escape' }))
    expect(order).toEqual(['clear', 'close'])
    cleanups.push(
      registerScope('menu', [{ combo: 'Escape', run: () => void order.push('menu') }], {
        layer: 'overlay',
        modal: true,
      }),
    )
    dispatch(key({ key: 'Escape', code: 'Escape' }))
    expect(order.at(-1)).toBe('menu')
    expect(activeScopes()[0]).toBe('menu')
  })

  it('IME 组合输入期间不处理任何快捷键（isComposing / keyCode 229）', () => {
    const run = vi.fn<() => void>()
    cleanups.push(registerScope('v', [{ combo: 'Enter', allowInInput: true, run }]))
    dispatch(key({ key: 'Enter', code: 'Enter', isComposing: true }))
    dispatch(key({ key: 'Process', code: 'Enter', keyCode: 229 }))
    expect(run).not.toHaveBeenCalled()
    dispatch(key({ key: 'Enter', code: 'Enter' }))
    expect(run).toHaveBeenCalledTimes(1)
  })

  it('输入框获焦时：默认不触发，allowInInput 的（⌘1、⌘P）与 Esc 照常触发', () => {
    const input = document.createElement('input')
    document.body.append(input)
    input.focus()
    const plain = vi.fn<() => void>()
    const pin = vi.fn<() => void>()
    const esc = vi.fn<() => void>()
    cleanups.push(
      registerScope('v', [
        { combo: 'Space', run: plain },
        { combo: 'Mod+KeyP', allowInInput: true, run: pin },
        { combo: 'Escape', run: esc },
      ]),
    )
    dispatch(key({ key: ' ', code: 'Space' }))
    // jsdom 的 UA 不是 macOS：Mod = Ctrl
    dispatch(key({ key: 'p', code: 'KeyP', ctrlKey: true }))
    dispatch(key({ key: 'Escape', code: 'Escape' }))
    expect(plain).not.toHaveBeenCalled()
    expect(pin).toHaveBeenCalledTimes(1)
    expect(esc).toHaveBeenCalledTimes(1)
  })

  it('修饰键必须完全一致；匹配后 preventDefault', () => {
    const run = vi.fn<() => void>()
    cleanups.push(registerScope('v', [{ combo: 'Mod+KeyC', run }]))
    const extra = key({ key: 'c', code: 'KeyC', ctrlKey: true, shiftKey: true })
    dispatch(extra)
    expect(run).not.toHaveBeenCalled()
    const exact = key({ key: 'c', code: 'KeyC', ctrlKey: true })
    dispatch(exact)
    expect(run).toHaveBeenCalledTimes(1)
    expect(exact.defaultPrevented).toBe(true)
  })

  it('modal 作用域挡住下层：没匹配的键不交给视图（交给菜单元素自己处理）', () => {
    const view = vi.fn<() => void>()
    cleanups.push(
      registerScope('list', [{ combo: 'ArrowDown', run: view }]),
      registerScope('menu', [], { layer: 'overlay', modal: true }),
    )
    const e = key({ key: 'ArrowDown', code: 'ArrowDown' })
    dispatch(e)
    expect(view).not.toHaveBeenCalled()
    expect(e.defaultPrevented).toBe(false)
  })

  it('repeat: false 时忽略按住产生的重复事件', () => {
    const run = vi.fn<() => void>()
    cleanups.push(registerScope('v', [{ combo: 'KeyR', run, repeat: false }]))
    dispatch(key({ key: 'r', code: 'KeyR', repeat: true }))
    expect(run).not.toHaveBeenCalled()
  })
})
