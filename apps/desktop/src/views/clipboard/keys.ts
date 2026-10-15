/**
 * 剪贴板视图的键位表。由统一的键盘路由（shared/keys）拦截，
 * 焦点一直留在搜索框里，所以绑定基本都要 `allowInInput`。macOS 的 ⌘ 写作 `Mod`，Windows / Linux 自动换成 Ctrl；
 * 删除键在 macOS 是 ⌫（Backspace，另外接受 ⌦），在 Windows / Linux 是 Delete。
 *
 * | 键 | 作用 |
 * | --- | --- |
 * | ↑ / ↓ | 移动选中项（跳过分组标题；按住连续移动，指示器直接跟到选中项） |
 * | ⌘↑ / ⌘↓ | 跳到第一条 / 最后一条 |
 * | PageUp / PageDown | 按一屏移动 |
 * | ↩ / ⇧↩ / ⌘↩ | 粘贴 / 纯文本粘贴 / 仅复制 |
 * | ⌘1–9 / ⌥⌘1–9（Windows / Linux：Ctrl+1–9 / Ctrl+Shift+1–9） | 粘贴 / 纯文本粘贴第 n 条（按当前可见顺序，含置顶区）。macOS 的 ⌘⇧3 / ⌘⇧4 / ⌘⇧5 是系统截屏键、在系统层被拦下，所以 macOS 的纯文本快速粘贴改用 ⌥⌘1–9（[`plainNthCombo`]） |
 * | ⌘P | 置顶 / 取消置顶 |
 * | ⌫（搜索框为空时） | 删除；置顶项先弹出行内确认，2 s 内再按一次才删 |
 * | ⌘⌫ | 直接删除（含置顶项），搜索框有字时同样生效 |
 * | Space（搜索框为空时）/ ⇧Space | 打开 / 关闭全文预览；搜索框有字时 Space 是空格 |
 * | ⌘K / ⇧F10 / ContextMenu | 选中项的操作菜单（右键菜单的全部操作 + 暂停监听、剪贴板设置、清空历史） |
 * | Tab / ⇧Tab | 循环切换类型过滤（焦点不会移走）；全文预览打开时改为在预览里的按钮之间移动焦点 |
 * | ⌘F | 打开类型过滤菜单 |
 * | Esc | 先关预览 / 确认条，再清空搜索，最后交给外壳关闭面板 |
 *
 * 只触发一次的键（↩、⌘P、⌫、Space…）按住时的重复事件同样被拦下、但什么都不做：不会把空格打进搜索框，也不会让
 * 预览开了又关。焦点在视图里的按钮上时（Tab 进了预览），↩ / Space 交给按钮本身。
 * ⌘W 关闭与 ⌘, 打开设置由面板外壳的全局层处理。
 */
import type { KeyBinding } from '../../shared/keys/keyRouter'
import type { OsKind, PasteMode } from '../../shared/ipc/types'

export interface ClipKeyHandlers {
  /** 搜索框是否为空（⌫ / Space 的语义取决于它） */
  queryEmpty(): boolean
  /** 焦点在视图里的按钮上（↩ / Space 交给按钮） */
  controlFocused(): boolean
  /** `repeat`：按住不放时的重复事件（指示器直接到位） */
  move(step: 1 | -1, repeat: boolean): void
  page(step: 1 | -1): void
  edge(which: 'first' | 'last'): void
  paste(mode: PasteMode): void
  pasteNth(n: number, mode: 'normal' | 'plain'): void
  togglePin(): void
  remove(force: boolean): void
  togglePreview(): void
  cycleFilter(step: 1 | -1): void
  openFilterMenu(): void
  /** 选中项的操作菜单（⌘K / ⇧F10 / ContextMenu） */
  openActions(): void
  /** 返回 false 表示没有可做的事（交给外壳关闭面板） */
  escape(): boolean
}

/** 只在第一次按下时执行：按住时的重复事件照样拦下（不落进搜索框），但不再执行 */
export function once(fn: () => void): (e: KeyboardEvent) => void {
  return (e) => {
    if (!e.repeat) fn()
  }
}

const DIGITS = [1, 2, 3, 4, 5, 6, 7, 8, 9] as const

export function clipboardBindings(os: OsKind, h: ClipKeyHandlers): KeyBinding[] {
  const deleteKeys = os === 'macos' ? ['Backspace', 'Delete'] : ['Delete']
  const forceKeys = os === 'macos' ? ['Mod+Backspace', 'Mod+Delete'] : ['Control+Delete']
  const notOnControl = () => !h.controlFocused()
  return [
    { combo: 'ArrowDown', allowInInput: true, run: (e) => h.move(1, e.repeat) },
    { combo: 'ArrowUp', allowInInput: true, run: (e) => h.move(-1, e.repeat) },
    { combo: 'Mod+ArrowDown', allowInInput: true, run: once(() => h.edge('last')) },
    { combo: 'Mod+ArrowUp', allowInInput: true, run: once(() => h.edge('first')) },
    { combo: 'PageDown', allowInInput: true, run: () => h.page(1) },
    { combo: 'PageUp', allowInInput: true, run: () => h.page(-1) },
    { combo: 'Enter', allowInInput: true, when: notOnControl, run: once(() => h.paste('normal')) },
    {
      combo: 'Shift+Enter',
      allowInInput: true,
      when: notOnControl,
      run: once(() => h.paste('plain')),
    },
    { combo: 'Mod+Enter', allowInInput: true, run: once(() => h.paste('copy_only')) },
    ...DIGITS.flatMap((n): KeyBinding[] => [
      { combo: `Mod+Digit${n}`, allowInInput: true, run: once(() => h.pasteNth(n, 'normal')) },
      {
        combo: plainNthCombo(os, n),
        allowInInput: true,
        run: once(() => h.pasteNth(n, 'plain')),
      },
    ]),
    { combo: 'Mod+KeyP', allowInInput: true, run: once(() => h.togglePin()) },
    ...deleteKeys.map((combo): KeyBinding => ({
      combo,
      allowInInput: true,
      // 搜索框有字时是正常的退格 / 删除
      when: () => h.queryEmpty(),
      run: once(() => h.remove(false)),
    })),
    ...forceKeys.map((combo): KeyBinding => ({
      combo,
      allowInInput: true,
      run: once(() => h.remove(true)),
    })),
    {
      combo: 'Space',
      allowInInput: true,
      when: () => h.queryEmpty() && notOnControl(),
      run: once(() => h.togglePreview()),
    },
    {
      combo: 'Shift+Space',
      allowInInput: true,
      when: notOnControl,
      run: once(() => h.togglePreview()),
    },
    { combo: 'Mod+KeyK', allowInInput: true, run: once(() => h.openActions()) },
    { combo: 'Shift+F10', allowInInput: true, run: once(() => h.openActions()) },
    { combo: 'ContextMenu', allowInInput: true, run: once(() => h.openActions()) },
    { combo: 'Tab', allowInInput: true, run: () => h.cycleFilter(1) },
    { combo: 'Shift+Tab', allowInInput: true, run: () => h.cycleFilter(-1) },
    { combo: 'Mod+KeyF', allowInInput: true, run: once(() => h.openFilterMenu()) },
    { combo: 'Escape', allowInInput: true, run: () => h.escape() },
  ]
}

/**
 * 纯文本粘贴第 n 条的组合串：macOS 是 ⌥⌘n（⌘⇧3–5 被系统截屏占用，到不了 WebView），Windows / Linux 是 Ctrl+Shift+n。
 * 键位表、`aria-keyshortcuts` 与设置页的说明都从这里取
 */
export function plainNthCombo(os: OsKind, n: number): string {
  return os === 'macos' ? `Alt+Mod+Digit${n}` : `Mod+Shift+Digit${n}`
}

/** 删除键的组合串（提示条与确认文案用） */
export function deleteCombo(os: OsKind): string {
  return os === 'macos' ? 'Backspace' : 'Delete'
}

/** 直接删除（含置顶项、不弹确认）的组合串（菜单里置顶项的「删除」标这个） */
export function forceDeleteCombo(os: OsKind): string {
  return os === 'macos' ? 'Mod+Backspace' : 'Control+Delete'
}
