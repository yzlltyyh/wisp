/**
 * 组件库。所有交互组件：props 带 `data-testid`、有 aria 属性与 focus-visible 焦点环，
 * 七种状态（空 / 加载 / 错误 / 禁用 / 悬停 / 按下 / 焦点，适用者）样式在 `shared/styles/components.css`。
 * 展示页：`?scenario=shell.gallery`；高保真视觉稿：`?scenario=design.*`。
 */
export { AppIcon, type AppIconProps } from './AppIcon'
export { AutoHeight, type AutoHeightProps } from './AutoHeight'
export { Badge, type BadgeProps, type Tone } from './Badge'
export { Banner, type BannerProps, type BannerTone } from './Banner'
export {
  Button,
  IconButton,
  type ButtonProps,
  type ButtonSize,
  type ButtonVariant,
  type IconButtonProps,
} from './Button'
export { Checkbox, type CheckboxProps } from './Checkbox'
export { Chip, StyleChip, type ChipProps, type StyleChipProps } from './Chip'
export { CopyButton, type CopyButtonProps } from './CopyButton'
export { CursorPulse } from './CursorPulse'
export { cx } from './cx'
export { Divider } from './Divider'
export { EmptyState, type EmptyStateProps } from './EmptyState'
export {
  ERROR_ACTIONS,
  ErrorCard,
  isCardError,
  type ErrorAction,
  type ErrorCardProps,
} from './ErrorCard'
export {
  computePosition,
  EdgeStore,
  FloatingContext,
  FloatingPortal,
  useFloating,
  useFloatingBoundary,
  type FloatingBoundary,
  type VisibleEdge,
} from './floating'
export { HotkeyRecorder, type HotkeyRecorderProps } from './HotkeyRecorder'
export { Icon, ICON_STROKE, type IconProps } from './Icon'
export { InlineConfirm, type InlineConfirmProps } from './InlineConfirm'
export {
  InlineToast,
  useInlineToast,
  type InlineToastControl,
  type InlineToastProps,
} from './InlineToast'
export { Kbd, type KbdProps } from './Kbd'
export { LangBar, type LangBarProps } from './LangBar'
export { Highlight, ListRow, type ListRowProps } from './ListRow'
export {
  ContextMenu,
  DropdownMenu,
  Menu,
  MenuEntryView,
  MenuLead,
  type ContextMenuProps,
  type DropdownMenuProps,
  type MenuCloseReason,
  type MenuEntry,
  type MenuEntryViewProps,
  type MenuItemEntry,
  type MenuProps,
  type MenuTriggerProps,
} from './Menu'
export {
  estimateMenuHeight,
  MENU_OFFSET,
  MENU_PADDING,
  useMenuOpener,
  useMenuRoom,
  type MenuOpener,
  type MenuVia,
} from './menuOpener'
export { PagedMenu, type MenuPage, type PagedMenuProps } from './PagedMenu'
export { NotReadyBanner, type NotReadyBannerProps } from './NotReadyBanner'
export { NotReadyCard, type NotReadyCardProps } from './NotReadyCard'
export { Popover, type PopoverProps } from './Popover'
export { ProgressBar, type ProgressBarProps } from './ProgressBar'
export { RadioGroup, type RadioGroupProps, type RadioOption } from './Radio'
export { ScrollArea, type ScrollAreaProps } from './ScrollArea'
export { Field, Section, type FieldProps, type SectionProps } from './Section'
export {
  SegmentedControl,
  type SegmentedControlProps,
  type SegmentOption,
} from './SegmentedControl'
export { Select, type SelectOption, type SelectProps } from './Select'
export { Skeleton, type SkeletonProps } from './Skeleton'
export { Slider, type SliderProps } from './Slider'
export { Spinner } from './Spinner'
export { StatusLine, type StatusLineProps } from './StatusLine'
export { Switch, type SwitchProps } from './Switch'
export { TextArea, type TextAreaProps } from './TextArea'
export {
  NumberField,
  SearchField,
  TextField,
  type NumberFieldProps,
  type SearchFieldProps,
  type TextFieldProps,
} from './TextField'
export { Thumbnail, type ThumbnailProps } from './Thumbnail'
export { Tooltip, type TooltipProps } from './Tooltip'
export { VirtualList, type VirtualListHandle, type VirtualListProps } from './VirtualList'
export {
  STYLE_SHORTCUTS,
  StyleMenu,
  styleMenuItems,
  type StyleMenuOption,
  type StyleMenuProps,
  type StyleMenuTriggerProps,
} from './StyleMenu'
