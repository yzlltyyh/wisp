/**
 * 剪贴板的操作注册表：视图里用户能触发的每个操作都在这里声明一次。
 *
 * 顺序就是菜单里的顺序（分组之间画分隔线）；快捷键按键位层声明，第一个可显示的组合就是菜单与提示条上显示的
 * 快捷键。快捷键速查（macOS 的 ⌘ 写作 `Mod`，Windows / Linux 自动换成 Ctrl；删除键在 macOS 是 ⌫（另接受 ⌦），在
 * Windows / Linux 是 Delete）：
 *
 * | 键 | 作用 |
 * | --- | --- |
 * | ↑ / ↓ | 移动选中项（跳过分组标题；按住连续移动，指示器直接跟到选中项） |
 * | ⌘↑ / ⌘↓ | 跳到第一条 / 最后一条 |
 * | PageUp / PageDown | 按一屏移动；全文预览打开时滚动预览 |
 * | ↩ / ⇧↩ / ⌘↩ | 粘贴 / 纯文本粘贴 / 仅复制 |
 * | ↩ / ⇧↩（有多选时） | 合并粘贴 / 合并为纯文本粘贴（按选择顺序，换行连接） |
 * | ⌘↩（选了两条以上时） | 连续粘贴：先粘贴第一条，之后在任何应用里每按一次 ⌘V 粘贴下一条（当前会话不支持时为合并复制） |
 * | ⌥⇧↩ | 「粘贴为…」子页（合并行、去空白、大小写、全半角、JSON、去跟踪参数；只列适用的） |
 * | ⌘1–9 / ⌥⌘1–9（Windows / Linux：Ctrl+1–9 / Ctrl+Shift+1–9） | 粘贴 / 纯文本粘贴第 n 条（按当前可见顺序，只数列出来的行；macOS 的 ⌘⇧3–5 是系统截屏键，见 keys.ts） |
 * | ⌘D | 收藏 / 取消收藏（与浏览器的书签键一致；多选时作用于全部） |
 * | ⌘E | 进入芯片条（识别出的片段）：←/→ 选择、↩ 粘贴、⌥↩ 仅复制、Esc 返回 |
 * | ⌘P | 交接到悬浮剪贴板（与翻译 / OCR 的「固定」同一个键；头部的钉子按钮同义） |
 * | ⇧↑ / ⇧↓、⌘ 点击、⇧ 点击 | 多选（行首按选择顺序编号）；Esc 先取消选择 |
 * | ⌫（搜索框为空时） | 删除；收藏先弹出行内确认，再按一次才删 |
 * | ⌘⌫ | 直接删除（含收藏），搜索框有字时同样生效 |
 * | Space（搜索框为空时）/ ⇧Space | 打开 / 关闭全文预览；搜索框有字时 Space 是空格 |
 * | ⌘K / ⇧F10 / ContextMenu | 选中项的操作菜单（右键菜单的全部操作 + 暂停记录、剪贴板设置、清空历史） |
 * | Tab / ⇧Tab | 循环切换类型过滤（焦点不会移走）；全文预览打开时改为在预览里的按钮之间移动焦点 |
 * | ⌘F | 打开类型过滤菜单 |
 * | Esc | 先关预览 / 确认条 / 芯片条，再取消多选、清空搜索，最后交给外壳关闭面板 |
 *
 * 只触发一次的键（↩、⌘D、⌫、Space…）按住时的重复事件同样被拦下、但什么都不做：不会把空格打进搜索框，也不会让
 * 预览开了又关。焦点在视图里的按钮上时（Tab 进了预览），↩ / Space 交给按钮本身。⌘W 关闭与 ⌘, 打开设置由面板外壳的
 * 全局层处理。
 */
import {
  ChevronsDownUp,
  ChevronsUpDown,
  CircleStop,
  ClipboardPaste,
  Copy,
  Eraser,
  ExternalLink,
  Eye,
  FolderOpen,
  ImageDown,
  Languages,
  ListOrdered,
  Pause,
  PencilLine,
  Pin,
  PinOff,
  Play,
  ScanText,
  Settings,
  Star,
  StarOff,
  TextQuote,
  Trash2,
  Type,
  WandSparkles,
  WholeWord,
  X,
} from 'lucide-react'
import { copyText } from '../../../shared/hooks/useCopy'
import type { ClipItemMeta, ClipKind, ClipInspect, PasteMode } from '../../../shared/ipc/types'
import { entityText, ENTITY_SPECS, TRANSFORM_SPECS } from '../../../shared/smart/specs'
import { plainNthCombo } from '../keys'
import { allFilesMissing, hasTextBody, isTextual } from '../kinds'
import { cycleFilter, firstItemIndex, indexOfId, lastItemIndex, nthItem, stepItem } from '../model'
import { isSearching, visibleHits } from '../store'
import {
  pageItem,
  type ActionArgs,
  type ActionCtx,
  type ActionEnv,
  type ClipAction,
  type ClipActionId,
  type ClipKeyLayer,
  type KeySpec,
} from './types'

/** 焦点在视图里的按钮 / 链接上（Tab 进了预览）：↩ / Space 交给它本身 */
function controlFocused(): boolean {
  const el = typeof document === 'undefined' ? null : document.activeElement
  return el instanceof HTMLButtonElement || el instanceof HTMLAnchorElement
}

const notOnControl = (): boolean => !controlFocused()
/** 搜索框为空：⌫ 是删除、Space 是预览；有字时它们照常编辑搜索框 */
const queryEmpty = (c: ActionCtx): boolean => c.s.query === ''
const kindIs =
  (kind: ClipKind) =>
  (c: ActionCtx): boolean =>
    c.target?.kind === kind
const textual = (c: ActionCtx): boolean => !!c.target && isTextual(c.target.kind)
/** 有多选 */
const many = (c: ActionCtx): boolean => c.marked.length > 0
/** ⌘↩ 此刻是连续粘贴：选了两条以上，且当前会话支持（否则 ⌘↩ 照旧是合并复制） */
const queueable = (c: ActionCtx): boolean => c.marked.length > 1 && c.caps.queueUnavailable === null
const notQueueable = (c: ActionCtx): boolean => !queueable(c)
/** 作用对象：多选时是全部标记的条目（选择顺序），否则是光标所在 / 菜单所在的那一条 */
function targets(c: ActionCtx): readonly ClipItemMeta[] {
  if (c.marked.length > 0) return c.marked
  return c.target ? [c.target] : []
}
/** 作用对象的检视结果（只对光标所在的那一条有；还没检视完时为 null） */
function inspected(c: ActionCtx): ClipInspect | null {
  const r = c.s.inspect
  return r && c.target && r.id === c.target.id ? r.result : null
}
const entitiesOf = (c: ActionCtx) => inspected(c)?.entities ?? []
const transformsOf = (c: ActionCtx) => inspected(c)?.transforms ?? []

/** 条目操作：右键菜单与 ⌘K 操作菜单 */
const ITEM_MENUS = { context: {}, command: {} } as const
/** 视图级操作：⋯ 菜单与 ⌘K 操作菜单（接在条目操作后面） */
const VIEW_MENUS = { more: {}, command: {} } as const
/** 删除的快捷键在删除确认进行中同样生效（再按一次只作用于发起确认的那一条 / 那一组） */
const DELETE_LAYERS: readonly ClipKeyLayer[] = ['list', 'confirm']
const DIGITS = [1, 2, 3, 4, 5, 6, 7, 8, 9] as const

function moveBy(c: ActionCtx, x: ActionEnv, step: 1 | -1, count: number, jump: boolean): void {
  const rows = x.ui.rows()
  const next = rows[stepItem(rows, indexOfId(rows, c.s.selectedId), step, count)]
  if (next?.type === 'item') x.cmd.select(next.key, true, jump)
}

function pasteNth(x: ActionEnv, n: number | undefined, mode: 'normal' | 'plain'): void {
  const row = n === undefined ? undefined : nthItem(x.ui.rows(), n)
  if (row) x.cmd.paste(mode, row.key)
}

/** 粘贴 / 纯文本粘贴 / 仅复制：多选时合并（按选择顺序、换行连接），否则粘贴作用对象 */
function pasteTargets(c: ActionCtx, x: ActionEnv, mode: PasteMode): void {
  if (many(c)) x.cmd.pasteMany(c.marked, mode)
  else x.cmd.paste(mode, c.target?.id)
}

type ActionDef<A> = Omit<ClipAction<A>, 'id'>

/** id → 定义（插入顺序就是菜单顺序；类型保证与 `ActionArgs` 一一对应） */
const TABLE: { readonly [I in ClipActionId]: ActionDef<ActionArgs[I]> } = {
  // ----- 粘贴
  paste: {
    group: 'paste',
    arity: 'one_or_many',
    label: (c) => (many(c) ? 'clipboard.action.paste_merged' : 'clipboard.action.paste'),
    icon: ClipboardPaste,
    keys: [{ combo: 'Enter', when: notOnControl }],
    surfaces: ITEM_MENUS,
    hints: {
      list: { order: 0, priority: 0, label: 'clipboard.hint.paste' },
      preview: { order: 1, priority: 1, label: 'clipboard.hint.paste' },
      marks: { order: 0, priority: 0, label: 'clipboard.hint.paste_merged' },
    },
    run: (c, x) => pasteTargets(c, x, 'normal'),
  },
  paste_plain: {
    group: 'paste',
    arity: 'one_or_many',
    label: (c) =>
      many(c) ? 'clipboard.action.paste_plain_merged' : 'clipboard.action.paste_plain',
    icon: Type,
    // 快捷键对任何条目都可用（文件条目粘贴路径文本、富文本去掉格式），菜单只为文本类条目（或多选）列出
    keys: [{ combo: 'Shift+Enter', when: notOnControl }],
    surfaces: {
      context: { when: (c) => many(c) || textual(c) },
      command: { when: (c) => many(c) || textual(c) },
    },
    hints: { list: { order: 1, priority: 1, label: 'clipboard.hint.plain' } },
    run: (c, x) => pasteTargets(c, x, 'plain'),
  },
  copy_only: {
    group: 'paste',
    arity: 'one_or_many',
    label: (c) => (many(c) ? 'clipboard.action.copy_merged' : 'clipboard.action.copy_only'),
    icon: Copy,
    // 选了两条以上时 ⌘↩ 是连续粘贴（下面的 queue_start）；合并复制仍在菜单里
    keys: [{ combo: 'Mod+Enter', when: notQueueable, display: notQueueable }],
    surfaces: ITEM_MENUS,
    hints: { list: { order: 2, priority: 4, label: 'clipboard.hint.copy' } },
    run: (c, x) => pasteTargets(c, x, 'copy_only'),
  },
  queue_start: {
    group: 'paste',
    arity: 'many',
    // 当前会话不支持（Wayland 等）时照样列出但置灰，标签写明原因
    when: (c) => c.marked.length > 1,
    enabled: (c) => c.caps.queueUnavailable === null,
    label: (c) =>
      c.caps.queueUnavailable === null
        ? 'clipboard.action.queue'
        : 'clipboard.action.queue_unavailable',
    icon: ListOrdered,
    keys: [{ combo: 'Mod+Enter', when: queueable, display: queueable }],
    surfaces: ITEM_MENUS,
    hints: { marks: { order: 1, priority: 2, label: 'clipboard.hint.queue' } },
    run: (c, x) => {
      if (queueable(c)) x.cmd.startQueue(c.marked)
    },
  },
  paste_as: {
    group: 'paste',
    arity: 'one',
    when: textual,
    label: () => 'clipboard.action.paste_as',
    icon: WandSparkles,
    // ⌥⇧↩ 对文本类条目都有效（检视结果还在路上时，菜单等它到了再打开）；菜单只在有可用的变换时列出它——列表打开菜单
    // 之前先检视，菜单项一次到位
    keys: [{ combo: 'Alt+Shift+Enter', when: notOnControl }],
    surfaces: {
      context: { when: (c) => transformsOf(c).length > 0 },
      command: { when: (c) => transformsOf(c).length > 0 },
    },
    page: (c, t) =>
      transformsOf(c).map((id) =>
        pageItem('paste_transform', id, {
          key: `transform-${id}`,
          label: t(TRANSFORM_SPECS[id].label),
          icon: TRANSFORM_SPECS[id].icon,
        }),
      ),
    // 菜单里：换到子页；⌥⇧↩：在光标所在行打开操作菜单、直接到这一页（没有可用的变换时是操作菜单本身）
    run: (_c, x) => x.ui.openItemMenu('paste_as'),
  },
  paste_transform: {
    group: 'nav',
    arity: 'one',
    run: (c, x, transform) => {
      if (c.target && transform) x.cmd.paste('normal', c.target.id, transform)
    },
  },
  paste_nth: {
    group: 'nav',
    arity: 'none',
    label: () => 'clipboard.action.paste',
    keys: DIGITS.map((n): KeySpec<number> => ({ combo: `Mod+Digit${n}`, arg: n })),
    run: (_c, x, n) => pasteNth(x, n, 'normal'),
  },
  paste_plain_nth: {
    group: 'nav',
    arity: 'none',
    label: () => 'clipboard.action.paste_plain',
    keys: DIGITS.map((n): KeySpec<number> => ({ combo: (os) => plainNthCombo(os, n), arg: n })),
    run: (_c, x, n) => pasteNth(x, n, 'plain'),
  },

  // ----- 查看与条目操作
  preview: {
    group: 'item',
    arity: 'one',
    // 「快速预览（Quick Look）」是 macOS 的术语；Windows / Linux 叫「预览」
    label: (c) =>
      c.os === 'macos' ? 'clipboard.action.preview_macos' : 'clipboard.action.preview',
    icon: Eye,
    keys: [
      { combo: 'Space', when: (c) => queryEmpty(c) && notOnControl(), arg: 'toggle' },
      { combo: 'Shift+Space', when: notOnControl, arg: 'toggle' },
      // 预览打开时：Space（搜索框有字时同样）返回，Esc 也返回
      { combo: 'Space', layers: ['preview'], when: notOnControl, arg: 'close' },
      { combo: 'Shift+Space', layers: ['preview'], when: notOnControl, arg: 'close' },
      { combo: 'Escape', layers: ['preview'], repeat: 'run', arg: 'close', display: false },
    ],
    surfaces: ITEM_MENUS,
    hints: {
      list: { order: 5, priority: 7, label: 'clipboard.hint.preview' },
      preview: { order: 0, priority: 0, label: 'clipboard.hint.back', layer: 'preview' },
    },
    run: (c, x, mode = 'open') => {
      if (mode === 'close') {
        x.cmd.setPreview(false)
        return
      }
      if (c.target) x.cmd.select(c.target.id)
      x.cmd.setPreview(mode === 'open' || !c.s.preview)
    },
  },
  favorite: {
    group: 'item',
    arity: 'one_or_many',
    // 多选时：只要有一条还没收藏就是「收藏」（全部收藏），否则「取消收藏」
    label: (c) =>
      targets(c).every((it) => it.pinned)
        ? 'clipboard.action.unfavorite'
        : 'clipboard.action.favorite',
    icon: Star,
    iconFor: (c) => (targets(c).every((it) => it.pinned) ? StarOff : Star),
    keys: [{ combo: 'Mod+KeyD' }],
    surfaces: ITEM_MENUS,
    // 提示条的文案固定为「收藏」（⌘D 是开关）：不随选中项变化
    hints: {
      list: { order: 3, priority: 6, label: 'clipboard.hint.favorite' },
      marks: { order: 1, priority: 2, label: 'clipboard.hint.favorite' },
    },
    run: (c, x) => x.cmd.toggleFavorite(targets(c)),
  },
  rename: {
    group: 'item',
    arity: 'one',
    // 命名即收藏：任何条目都能命名（「命名…」），已有名称的是「重命名…」
    label: (c) => (c.target?.name ? 'clipboard.action.rename' : 'clipboard.action.name'),
    icon: PencilLine,
    surfaces: ITEM_MENUS,
    run: (c, x) => {
      if (c.target) x.cmd.startRename(c.target.id)
    },
  },
  extract: {
    group: 'item',
    arity: 'one',
    when: (c) => entitiesOf(c).length > 0,
    label: () => 'clipboard.action.extract',
    icon: TextQuote,
    keys: [{ combo: 'Mod+KeyE' }],
    surfaces: ITEM_MENUS,
    page: (c, t) =>
      entitiesOf(c).map((e, index) =>
        pageItem(
          'paste_fragment',
          { index, mode: 'normal' },
          {
            key: `entity-${index}`,
            label: entityText(e),
            icon: ENTITY_SPECS[e.kind].icon,
            hint: t(ENTITY_SPECS[e.kind].label),
          },
        ),
      ),
    // ⌘E：键盘焦点进入芯片条；菜单里：换到「提取」子页
    run: (c, x) => (c.source === 'key' ? x.cmd.enterChips() : x.ui.openItemMenu('extract')),
  },
  paste_fragment: {
    group: 'nav',
    arity: 'one',
    when: (c) => entitiesOf(c).length > 0,
    keys: [
      { combo: 'Enter', layers: ['chips'], arg: { mode: 'normal' } },
      { combo: 'Alt+Enter', layers: ['chips'], arg: { mode: 'copy_only' } },
    ],
    hints: { chips: { order: 1, priority: 0, label: 'clipboard.hint.paste', layer: 'chips' } },
    run: (c, x, arg) => {
      const e = entitiesOf(c)[arg?.index ?? c.s.chip ?? -1]
      if (e && c.target) x.cmd.pasteText(e.value, c.target.id, arg?.mode ?? 'normal')
    },
  },
  open_link: {
    group: 'item',
    arity: 'one',
    when: kindIs('link'),
    label: () => 'clipboard.action.open_link',
    icon: ExternalLink,
    surfaces: { ...ITEM_MENUS, preview_actions: {} },
    run: (c, x) => {
      if (c.target) x.cmd.openLink(c.target.id)
    },
  },
  reveal: {
    group: 'item',
    arity: 'one',
    when: kindIs('files'),
    enabled: (c) => !!c.target && !allFilesMissing(c.target),
    label: (c) => `clipboard.action.reveal.${c.os}`,
    icon: FolderOpen,
    surfaces: { ...ITEM_MENUS, preview_actions: {} },
    run: (c, x) => {
      if (c.target) x.cmd.reveal(c.target.id)
    },
  },
  save_image: {
    group: 'item',
    arity: 'one',
    when: kindIs('image'),
    label: () => 'clipboard.action.save_image',
    icon: ImageDown,
    surfaces: { ...ITEM_MENUS, preview_actions: {} },
    run: (c, x) => {
      if (c.target) x.cmd.saveImage(c.target.id)
    },
  },
  ocr: {
    group: 'item',
    arity: 'one',
    when: kindIs('image'),
    label: () => 'clipboard.action.ocr',
    icon: ScanText,
    surfaces: { ...ITEM_MENUS, preview_actions: {} },
    run: (c, x) => {
      if (c.target) x.cmd.ocr(c.target.id)
    },
  },
  translate: {
    group: 'item',
    arity: 'one',
    when: (c) => textual(c) && c.target?.kind !== 'color',
    label: () => 'clipboard.action.translate',
    icon: Languages,
    surfaces: ITEM_MENUS,
    run: (c, x) => {
      if (c.target) x.cmd.translate(c.target.id)
    },
  },
  monospace: {
    group: 'item',
    arity: 'one',
    // 等宽开关是全文预览自己的状态（换条目时回到该类型的默认值），只在预览提供时出现
    when: (c) => !!c.preview && !!c.target && hasTextBody(c.target.kind),
    label: () => 'clipboard.action.monospace',
    icon: WholeWord,
    checked: (c) => c.preview?.mono ?? false,
    surfaces: { preview_tools: {} },
    run: (c) => c.preview?.setMono(!c.preview.mono),
  },
  copy_value: {
    group: 'item',
    arity: 'none',
    // 片段的复制按钮（CopyButton）自己执行复制并显示 ✓ 反馈，这里提供标签、图标与同一个复制入口
    label: () => 'clipboard.action.copy_value',
    icon: Copy,
    surfaces: { fragment: {} },
    run: (_c, _x, text) => {
      if (text) void copyText(text)
    },
  },

  // ----- 删除
  delete: {
    group: 'remove',
    arity: 'one_or_many',
    danger: true,
    label: () => 'clipboard.action.delete',
    icon: Trash2,
    keys: [
      {
        combo: (os) => (os === 'macos' ? 'Backspace' : 'Delete'),
        layers: DELETE_LAYERS,
        // 搜索框有字时是正常的退格 / 删除
        when: queryEmpty,
        arg: false,
        // 菜单对收藏直接删除（不再弹行内确认），所以菜单里收藏标的是同样直接删除的 ⌘⌫ / Ctrl+Delete
        display: (c) => !targets(c).some((it) => it.pinned),
      },
      {
        combo: (os) => (os === 'macos' ? 'Delete' : null),
        layers: DELETE_LAYERS,
        when: queryEmpty,
        arg: false,
        display: false,
      },
      {
        combo: (os) => (os === 'macos' ? 'Mod+Backspace' : 'Control+Delete'),
        layers: DELETE_LAYERS,
        arg: true,
      },
      {
        combo: (os) => (os === 'macos' ? 'Mod+Delete' : null),
        layers: DELETE_LAYERS,
        arg: true,
        display: false,
      },
    ],
    // 菜单本身就是一次明确的操作：收藏也直接删除
    surfaces: { context: { arg: true }, command: { arg: true } },
    hints: {
      list: { order: 4, priority: 5, label: 'clipboard.hint.delete' },
      marks: { order: 2, priority: 3, label: 'clipboard.hint.delete' },
    },
    run: (c, x, force = false) => {
      if (many(c)) x.cmd.removeMany(c.marked, force)
      else if (c.target) x.cmd.remove(force, c.target.id)
    },
  },

  // ----- 视图级（⋯ 菜单；破坏性的「清空历史…」放最后、用分隔线隔开，HIG）
  float_open: {
    group: 'view',
    arity: 'none',
    hosts: ['panel'],
    when: (c) => c.caps.float,
    label: () => 'clipboard.action.float',
    icon: Pin,
    keys: [{ combo: 'Mod+KeyP' }],
    surfaces: { ...VIEW_MENUS, toolbar: {} },
    run: (_c, x) => x.cmd.openFloat(),
  },
  queue_cancel: {
    group: 'view',
    arity: 'none',
    when: (c) => c.s.queue?.active === true,
    label: () => 'clipboard.action.queue_cancel',
    icon: CircleStop,
    surfaces: { ...VIEW_MENUS, toolbar: {} },
    run: (_c, x) => x.cmd.cancelQueue(),
  },
  float_on_top: {
    group: 'view',
    arity: 'none',
    hosts: ['pinned'],
    when: (c) => c.onTop !== null,
    label: () => 'clipboard.action.on_top',
    icon: Pin,
    iconFor: (c) => (c.onTop ? Pin : PinOff),
    checked: (c) => c.onTop === true,
    surfaces: { toolbar: {} },
    run: (c, x) => x.host.float?.setOnTop(!c.onTop),
  },
  float_close: {
    group: 'view',
    arity: 'none',
    hosts: ['pinned'],
    label: () => 'clipboard.action.close',
    icon: X,
    // ⌘W 由悬浮窗外壳的全局层处理（与固定窗口一致），这里只显示
    keys: [{ combo: 'Mod+KeyW', owner: 'shell' }],
    surfaces: { toolbar: {} },
    run: (_c, x) => x.host.float?.close(),
  },
  favorites_toggle: {
    group: 'view',
    arity: 'none',
    // 只在列表分组、而且有收藏时（检索结果与收藏视图平铺，没有可折叠的分组）
    when: (c) =>
      c.s.filter !== 'favorite' && !isSearching(c.s) && visibleHits(c.s).some((h) => h.item.pinned),
    label: (c) =>
      c.s.favoritesCollapsed
        ? 'clipboard.action.expand_favorites'
        : 'clipboard.action.collapse_favorites',
    icon: ChevronsDownUp,
    iconFor: (c) => (c.s.favoritesCollapsed ? ChevronsUpDown : ChevronsDownUp),
    surfaces: VIEW_MENUS,
    run: (_c, x) => x.cmd.toggleFavoritesCollapsed(),
  },
  show_favorites: {
    group: 'nav',
    arity: 'none',
    run: (_c, x) => x.cmd.setFilter('favorite'),
  },
  pause: {
    group: 'record',
    arity: 'none',
    label: (c) => (c.s.paused ? 'clipboard.action.resume' : 'clipboard.action.pause'),
    icon: Pause,
    iconFor: (c) => (c.s.paused ? Play : Pause),
    surfaces: VIEW_MENUS,
    run: (c, x) => x.cmd.setPaused(!c.s.paused),
  },
  settings: {
    group: 'settings',
    arity: 'none',
    label: () => 'clipboard.action.settings',
    icon: Settings,
    keys: [{ combo: 'Mod+Comma', owner: 'shell' }],
    surfaces: VIEW_MENUS,
    run: (_c, x) => x.cmd.openSettings(),
  },
  clear: {
    group: 'history',
    arity: 'none',
    danger: true,
    // 与行操作的「删除」（垃圾桶）区分开：清空整个历史用橡皮擦
    label: () => 'clipboard.action.clear',
    icon: Eraser,
    enabled: (c) => c.target !== null || c.s.all.hits.length > 0,
    surfaces: VIEW_MENUS,
    // 打开含「清空历史…」的菜单时先读清空会不会同步到其他设备，确认条出现时文案就是对的
    prepare: (_c, x) => x.cmd.prepareClear(),
    run: (_c, x) => x.cmd.askClear(),
  },

  // ----- 只有快捷键 / 指针
  move: {
    group: 'nav',
    arity: 'none',
    // 按住 ↑↓ 的重复事件：指示器直接到位（spring 追不上 30 次 / s 的移动）
    keys: [
      { combo: 'ArrowUp', arg: -1, repeat: 'run' },
      { combo: 'ArrowDown', arg: 1, repeat: 'run' },
    ],
    hints: {
      preview: { order: 2, priority: 2, label: 'clipboard.hint.browse', all: true },
    },
    run: (c, x, step = 1) => moveBy(c, x, step, 1, c.repeat),
  },
  move_edge: {
    group: 'nav',
    arity: 'none',
    keys: [
      { combo: 'Mod+ArrowUp', arg: 'first' },
      { combo: 'Mod+ArrowDown', arg: 'last' },
    ],
    run: (_c, x, which = 'first') => {
      const rows = x.ui.rows()
      const row = rows[which === 'first' ? firstItemIndex(rows) : lastItemIndex(rows)]
      if (row?.type === 'item') x.cmd.select(row.key, true, true)
    },
  },
  move_page: {
    group: 'nav',
    arity: 'none',
    keys: [
      { combo: 'PageUp', arg: -1, repeat: 'run' },
      { combo: 'PageDown', arg: 1, repeat: 'run' },
    ],
    run: (c, x, step = 1) => moveBy(c, x, step, Math.max(1, x.ui.pageRows() - 1), true),
  },
  mark_extend: {
    group: 'nav',
    arity: 'none',
    keys: [
      { combo: 'Shift+ArrowUp', arg: -1, repeat: 'run' },
      { combo: 'Shift+ArrowDown', arg: 1, repeat: 'run' },
    ],
    run: (_c, x, step = 1) => x.cmd.extendMarks(x.ui.rows(), step),
  },
  mark_toggle: {
    group: 'nav',
    arity: 'none',
    run: (_c, x, id) => {
      if (id !== undefined) x.cmd.toggleMark(id)
    },
  },
  mark_range: {
    group: 'nav',
    arity: 'none',
    run: (_c, x, id) => {
      if (id !== undefined) x.cmd.markRange(x.ui.rows(), id)
    },
  },
  scroll_preview: {
    group: 'nav',
    arity: 'none',
    keys: [
      { combo: 'PageUp', layers: ['preview'], arg: -1, repeat: 'run' },
      { combo: 'PageDown', layers: ['preview'], arg: 1, repeat: 'run' },
    ],
    run: (_c, x, step = 1) => x.ui.scrollPreview(step),
  },
  cycle_preview_focus: {
    group: 'nav',
    arity: 'none',
    keys: [
      { combo: 'Tab', layers: ['preview'], arg: 1, repeat: 'run' },
      { combo: 'Shift+Tab', layers: ['preview'], arg: -1, repeat: 'run' },
    ],
    run: (_c, x, step = 1) => x.ui.cyclePreviewFocus(step),
  },
  cycle_filter: {
    group: 'nav',
    arity: 'none',
    keys: [
      { combo: 'Tab', arg: 1, repeat: 'run' },
      { combo: 'Shift+Tab', arg: -1, repeat: 'run' },
    ],
    run: (c, x, step = 1) => x.cmd.setFilter(cycleFilter(c.s.filter, step)),
  },
  open_filter_menu: {
    group: 'nav',
    arity: 'none',
    keys: [{ combo: 'Mod+KeyF' }],
    hints: { list: { order: 7, priority: 2, label: 'clipboard.hint.filter' } },
    run: (_c, x) => x.ui.openFilterMenu(),
  },
  open_actions: {
    group: 'nav',
    arity: 'none',
    keys: [{ combo: 'Mod+KeyK' }, { combo: 'Shift+F10' }, { combo: 'ContextMenu' }],
    hints: { list: { order: 6, priority: 8, label: 'clipboard.hint.actions' } },
    // 选中行的操作菜单由列表打开；没有选中行（空列表）时打开 ⋯ 菜单
    run: (c, x) => (c.target ? x.ui.openItemMenu() : x.ui.openMoreMenu()),
  },
  escape: {
    group: 'nav',
    arity: 'none',
    keys: [{ combo: 'Escape', repeat: 'run' }],
    hints: {
      // 面板：Esc 最后关闭面板；悬浮窗：Esc 最后把键盘还给原来的应用（窗口留着）
      list: {
        order: 8,
        priority: 3,
        label: (c) => (c.caps.escapeCloses ? 'clipboard.hint.close' : 'clipboard.hint.leave'),
      },
      marks: { order: 3, priority: 1, label: 'clipboard.hint.clear_marks' },
    },
    // 先关确认条，再取消多选、清空搜索；都没有时交给宿主（面板：外壳关闭面板）
    run: (c, x) => {
      if (c.s.confirm) {
        x.cmd.cancelConfirm()
        return true
      }
      if (c.s.marks.length > 0) {
        x.cmd.clearMarks()
        return true
      }
      if (c.s.query !== '') {
        x.cmd.setQuery('')
        return true
      }
      return x.host.escapeFallback()
    },
  },
  cancel_confirm: {
    group: 'nav',
    arity: 'none',
    keys: [{ combo: 'Escape', layers: ['confirm', 'clear'], repeat: 'run' }],
    run: (_c, x) => x.cmd.cancelConfirm(),
  },
  clear_confirm: {
    group: 'nav',
    arity: 'none',
    keys: [{ combo: 'Enter', layers: ['clear'] }],
    run: (_c, x) => x.cmd.clear(),
  },
  keep_pinned: {
    group: 'nav',
    arity: 'none',
    keys: [{ combo: 'Space', layers: ['clear'] }],
    run: (c, x, keep) => {
      const confirm = c.s.confirm
      if (confirm?.kind === 'clear') x.cmd.setKeepPinned(keep ?? !confirm.keepPinned)
    },
  },
  chip_move: {
    group: 'nav',
    arity: 'none',
    keys: [
      { combo: 'ArrowLeft', layers: ['chips'], arg: -1, repeat: 'run' },
      { combo: 'ArrowRight', layers: ['chips'], arg: 1, repeat: 'run' },
    ],
    hints: {
      chips: { order: 0, priority: 1, label: 'clipboard.hint.choose', layer: 'chips', all: true },
    },
    run: (_c, x, step = 1) => x.cmd.moveChip(step),
  },
  chip_exit: {
    group: 'nav',
    arity: 'none',
    keys: [{ combo: 'Escape', layers: ['chips'], repeat: 'run' }],
    hints: { chips: { order: 2, priority: 2, label: 'clipboard.hint.back', layer: 'chips' } },
    run: (_c, x) => x.cmd.exitChips(),
  },
  rename_commit: {
    group: 'nav',
    arity: 'none',
    keys: [{ combo: 'Enter', layers: ['rename'] }],
    run: (_c, x) => x.cmd.commitRename(),
  },
  rename_cancel: {
    group: 'nav',
    arity: 'none',
    keys: [{ combo: 'Escape', layers: ['rename'], repeat: 'run' }],
    run: (_c, x) => x.cmd.cancelRename(),
  },
}

/** 全部操作（菜单顺序） */
export const ACTIONS: readonly ClipAction<unknown>[] = (Object.keys(TABLE) as ClipActionId[]).map(
  (id): ClipAction<unknown> => ({ ...TABLE[id], id }) as ClipAction<unknown>,
)

const BY_ID = new Map<ClipActionId, ClipAction<unknown>>(ACTIONS.map((a) => [a.id, a]))

export function actionById<I extends ClipActionId>(id: I): ClipAction<ActionArgs[I]> {
  const a = BY_ID.get(id)
  if (!a) throw new Error(`unknown clipboard action: ${id}`)
  return a as ClipAction<ActionArgs[I]>
}
