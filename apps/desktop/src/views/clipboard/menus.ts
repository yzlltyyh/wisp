/**
 * 剪贴板视图的菜单：右键菜单（全部条目操作并标注快捷键）、⌘K 操作菜单（右键菜单 +
 * 视图级操作，纯键盘也能到达全部操作）、类型过滤菜单、⋯ 菜单。快捷键列与键位表（keys.ts）一致。
 */
import {
  ClipboardPaste,
  Code,
  Copy,
  Eraser,
  Eye,
  ExternalLink,
  File,
  FolderOpen,
  Image as ImageIcon,
  ImageDown,
  Languages,
  LayoutList,
  Link,
  Palette,
  Pause,
  Pin,
  PinOff,
  Play,
  Settings,
  Trash2,
  Type,
} from 'lucide-react'
import type { LucideIcon } from 'lucide-react'
import type { ClipFilter, ClipItemMeta, OsKind } from '../../shared/ipc/types'
import type { TFunction } from '../../shared/i18n/useT'
import type { MenuEntry } from '../../shared/ui'
import { deleteCombo, forceDeleteCombo } from './keys'
import { FILTERS } from './model'
import type { ClipActions } from './useClipboard'

const TEXTUAL = new Set(['text', 'rich_text', 'code', 'link', 'color'])

/** 右键菜单：粘贴类 → 查看 / 置顶 → 按类型的操作 → 送翻译 → 删除 */
export function contextMenuItems(
  item: ClipItemMeta,
  os: OsKind,
  t: TFunction,
  a: ClipActions,
): MenuEntry[] {
  const id = item.id
  const textual = TEXTUAL.has(item.kind)
  const items: MenuEntry[] = [
    {
      id: 'paste',
      label: t('clipboard.action.paste'),
      icon: ClipboardPaste,
      shortcut: 'Enter',
      onSelect: () => a.paste('normal', id),
    },
  ]
  if (textual) {
    items.push({
      id: 'paste-plain',
      label: t('clipboard.action.paste_plain'),
      icon: Type,
      shortcut: 'Shift+Enter',
      onSelect: () => a.paste('plain', id),
    })
  }
  items.push(
    {
      id: 'copy',
      label: t('clipboard.action.copy_only'),
      icon: Copy,
      shortcut: 'Mod+Enter',
      onSelect: () => a.paste('copy_only', id),
    },
    { type: 'separator', id: 'sep-view' },
    {
      id: 'preview',
      // 「快速预览（Quick Look）」是 macOS 的术语；Windows / Linux 叫「预览」
      label: os === 'macos' ? t('clipboard.action.preview_macos') : t('clipboard.action.preview'),
      icon: Eye,
      shortcut: 'Space',
      onSelect: () => {
        a.select(id)
        a.setPreview(true)
      },
    },
    {
      id: 'pin',
      label: item.pinned ? t('clipboard.action.unpin') : t('clipboard.action.pin'),
      icon: item.pinned ? PinOff : Pin,
      shortcut: 'Mod+KeyP',
      onSelect: () => a.togglePin(id),
    },
  )
  if (item.kind === 'link') {
    items.push({
      id: 'open-link',
      label: t('clipboard.action.open_link'),
      icon: ExternalLink,
      onSelect: () => a.openLink(id),
    })
  }
  if (item.kind === 'files') {
    items.push({
      id: 'reveal',
      label: t(`clipboard.action.reveal.${os}`),
      icon: FolderOpen,
      disabled: !!item.files?.length && item.files.every((f) => !f.exists),
      onSelect: () => a.reveal(id),
    })
  }
  if (item.kind === 'image') {
    // 图片送 OCR：编排器还没有「对已有图片做 OCR」的入口（Rust 的 clipboard_send_to(ocr) 返回 not_implemented），暂不提供
    items.push({
      id: 'save-image',
      label: t('clipboard.action.save_image'),
      icon: ImageDown,
      onSelect: () => a.saveImage(id),
    })
  }
  if (textual && item.kind !== 'color') {
    items.push({
      id: 'translate',
      label: t('clipboard.action.translate'),
      icon: Languages,
      onSelect: () => a.translate(id),
    })
  }
  items.push(
    { type: 'separator', id: 'sep-delete' },
    {
      id: 'delete',
      label: t('clipboard.action.delete'),
      icon: Trash2,
      danger: true,
      // 菜单本身就是一次明确的操作：置顶项也直接删除（不再弹行内确认），所以快捷键列标的是同样直接删除的
      // ⌘⌫ / Ctrl+Delete，而不是置顶项上会先弹确认的 ⌫ / Delete
      shortcut: item.pinned ? forceDeleteCombo(os) : deleteCombo(os),
      onSelect: () => a.remove(true, id),
    },
  )
  return items
}

/** 类型过滤菜单每一项的图标（与列表行的类型图标一致；「全部」用列表图标） */
const FILTER_ICONS: Record<ClipFilter, LucideIcon> = {
  all: LayoutList,
  text: Type,
  image: ImageIcon,
  files: File,
  link: Link,
  code: Code,
  color: Palette,
}

/** 类型过滤菜单（勾选当前项，每项带与列表行一致的类型图标；Tab / ⇧Tab 循环切换的说明放在提示里） */
export function filterMenuItems(
  current: ClipFilter,
  t: TFunction,
  onPick: (f: ClipFilter) => void,
): MenuEntry[] {
  return FILTERS.map((f) => ({
    id: `filter-${f}`,
    label: t(`clipboard.filter.${f}`),
    icon: FILTER_ICONS[f],
    checked: f === current,
    onSelect: () => onPick(f),
  }))
}

/**
 * ⋯ 菜单：暂停 / 恢复监听 → 剪贴板设置… → 清空历史…（破坏性操作放最后、用分隔线隔开，HIG）
 */
export function moreMenuItems(
  paused: boolean,
  hasItems: boolean,
  t: TFunction,
  a: ClipActions,
): MenuEntry[] {
  return [
    {
      id: 'pause',
      label: paused ? t('clipboard.action.resume') : t('clipboard.action.pause'),
      icon: paused ? Play : Pause,
      onSelect: () => a.setPaused(!paused),
    },
    { type: 'separator', id: 'sep-settings' },
    {
      id: 'settings',
      label: t('clipboard.action.settings'),
      icon: Settings,
      shortcut: 'Mod+Comma',
      onSelect: () => a.openSettings(),
    },
    { type: 'separator', id: 'sep-clear' },
    {
      id: 'clear',
      // 与行操作的「删除」（垃圾桶）区分开：清空整个历史用橡皮擦
      label: t('clipboard.action.clear'),
      icon: Eraser,
      danger: true,
      disabled: !hasItems,
      onSelect: () => a.askClear(),
    },
  ]
}

/**
 * ⌘K 操作菜单：选中项的全部操作（同右键菜单），后面接视图级操作（⋯ 菜单的内容）——焦点留在搜索框、Tab 用来切过滤时，
 * 纯键盘用户由此到达「图片另存为」「在 Finder 中显示」「翻译」「清空历史」等全部操作
 */
export function actionMenuItems(
  item: ClipItemMeta,
  os: OsKind,
  paused: boolean,
  t: TFunction,
  a: ClipActions,
): MenuEntry[] {
  const more = moreMenuItems(paused, true, t, a).map((e) =>
    e.type === 'separator' ? { ...e, id: `more-${e.id}` } : e,
  )
  // 行操作的「删除」与视图的「清空历史…」都是危险项：清空放在最后
  return [...contextMenuItems(item, os, t, a), { type: 'separator', id: 'sep-actions' }, ...more]
}
