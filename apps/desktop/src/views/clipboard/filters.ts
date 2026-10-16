/**
 * 类型过滤菜单（⌘F / 头部的过滤 Chip）：勾选当前项，每项带与列表行一致的类型图标。切换过滤本身是数据命令
 * （`ClipCommands.setFilter`）；Tab / ⇧Tab 循环切换与打开这个菜单是注册表里的操作。
 */
import { Code, File, Image as ImageIcon, LayoutList, Link, Palette, Star, Type } from 'lucide-react'
import type { LucideIcon } from 'lucide-react'
import type { ClipFilter } from '../../shared/ipc/types'
import type { TFunction } from '../../shared/i18n/useT'
import type { MenuEntry } from '../../shared/ui'
import { FILTERS } from './model'

/** 每个过滤的图标（与列表行的类型图标一致；「全部」用列表图标，「收藏」用星标） */
const FILTER_ICONS: Record<ClipFilter, LucideIcon> = {
  all: LayoutList,
  favorite: Star,
  text: Type,
  image: ImageIcon,
  files: File,
  link: Link,
  code: Code,
  color: Palette,
}

/** 类型过滤菜单（Tab / ⇧Tab 循环切换的说明放在提示里） */
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
