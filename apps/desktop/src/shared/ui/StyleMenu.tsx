/**
 * 翻译风格菜单：翻译视图的风格 chip、OCR 的分体按钮 ▾ 与直接翻译 /
 * 译文区的风格 chip、固定窗口共用同一份——分组标题「翻译风格」+ 风格列表（勾选当前、⌘1–9）+「编辑当前风格」。
 *
 * - 按下就打开（menuOpener）；卡片高度的预留、追赶期间的下拉帘、关闭淡出后再释放由组件库的浮层负责；
 * - **数字直选**：菜单打开时接受裸数字 1–9 与 ⌘1–9（菜单是 modal 作用域，视图层的 ⌘n 此时够不到，见 menuKeys）；
 *   直选先把焦点还给触发器再关闭；
 * - 键盘打开（Enter / Space / ↑↓）时高亮当前风格，指针打开时不高亮；
 * - **矮面板里的收起规则**（与翻译视图 ⋯ 菜单同一种做法：先收起低频项，保证关键项完整、不用滚动）：卡片上限放不下
 *   整份菜单时，先去掉分组标题与「编辑当前风格」（设置里、⋯ 菜单里都还有入口）。
 */
import { SlidersHorizontal } from 'lucide-react'
import { useRef, useState, type ReactNode, type RefObject } from 'react'
import { useT } from '../i18n/useT'
import type { KeyBinding } from '../keys/keyRouter'
import { useMenuKeys } from '../keys/menuKeys'
import type { Align, Side } from './floating'
import { Menu, type MenuEntry } from './Menu'
import { estimateMenuHeight, useMenuOpener, useMenuRoom, type MenuOpener } from './menuOpener'

/** 能用 ⌘n / 数字直选的风格数（⌘1–9） */
export const STYLE_SHORTCUTS = 9

export interface StyleMenuOption {
  id: string
  /** 显示名（内置风格的译名 / 自定义风格的名字） */
  name: string
}

/** 风格菜单触发器需要的属性（展开到 StyleChip / 分体按钮的 ▾ 上） */
export interface StyleMenuTriggerProps {
  ref: RefObject<HTMLButtonElement | null>
  onPointerDown: MenuOpener['onPointerDown']
  onClick: MenuOpener['onClick']
  onKeyDown: MenuOpener['onKeyDown']
  'aria-haspopup': 'menu'
  'aria-expanded': boolean
  'data-open': true | undefined
}

export interface StyleMenuProps {
  styles: readonly StyleMenuOption[]
  current: string
  onPick: (id: string) => void
  /** 「编辑当前风格」（打开设置 › 翻译）；省略时没有这一项 */
  onEdit?: () => void
  renderTrigger: (props: StyleMenuTriggerProps, open: boolean) => ReactNode
  /** 菜单的无障碍名称 */
  label: string
  side?: Side
  align?: Align
  'data-testid'?: string
}

/** 风格菜单的项（compact：矮面板里去掉分组标题与「编辑当前风格」） */
export function styleMenuItems(
  t: ReturnType<typeof useT>,
  {
    styles,
    current,
    onPick,
    onEdit,
  }: Pick<StyleMenuProps, 'styles' | 'current' | 'onPick' | 'onEdit'>,
  compact = false,
): MenuEntry[] {
  const list: MenuEntry[] = styles.map((s, i) => ({
    id: s.id,
    label: s.name,
    checked: s.id === current,
    shortcut: i < STYLE_SHORTCUTS ? `Mod+Digit${i + 1}` : null,
    onSelect: () => onPick(s.id),
  }))
  if (compact) return list
  return [
    { type: 'label', id: 'title', label: t('styles.menu.title') },
    ...list,
    ...(onEdit
      ? ([
          { type: 'separator', id: 'sep' },
          // 图标在前导列里（勾选列），文字与上面的风格名左缘对齐
          { id: 'edit', label: t('styles.menu.edit'), icon: SlidersHorizontal, onSelect: onEdit },
        ] satisfies MenuEntry[])
      : []),
  ]
}

export function StyleMenu({
  styles,
  current,
  onPick,
  onEdit,
  renderTrigger,
  label,
  side = 'bottom',
  align = 'end',
  'data-testid': testId,
}: StyleMenuProps) {
  const t = useT()
  const trigger = useRef<HTMLButtonElement | null>(null)
  const room = useMenuRoom(trigger)
  const [compact, setCompact] = useState(false)
  const opener = useMenuOpener(trigger, () => {
    // 打开之前按卡片上限决定要不要收起（打开期间不变）
    const full = estimateMenuHeight(styleMenuItems(t, { styles, current, onPick, onEdit }))
    setCompact(full > room())
  })
  const pick = (index: number) => {
    const s = styles[index]
    if (!s) return false
    opener.close(true)
    onPick(s.id)
    return true
  }
  const digits: KeyBinding[] = []
  for (let i = 0; i < STYLE_SHORTCUTS; i++) {
    digits.push({ combo: `Digit${i + 1}`, run: () => pick(i) })
    digits.push({ combo: `Mod+Digit${i + 1}`, run: () => pick(i) })
  }
  useMenuKeys(opener.open, digits)
  const props: StyleMenuTriggerProps = {
    ref: trigger,
    onPointerDown: opener.onPointerDown,
    onClick: opener.onClick,
    onKeyDown: opener.onKeyDown,
    'aria-haspopup': 'menu',
    'aria-expanded': opener.open,
    'data-open': opener.open || undefined,
  }
  return (
    <>
      {renderTrigger(props, opener.open)}
      {opener.mounted ? (
        <Menu
          open={opener.open}
          anchor={trigger}
          items={styleMenuItems(t, { styles, current, onPick, onEdit }, compact)}
          side={side}
          align={align}
          initialActive={opener.via === 'keyboard' ? 'selected' : 'none'}
          label={label}
          onClose={() => opener.close()}
          data-testid={testId}
        />
      ) : null}
    </>
  )
}
