/**
 * 剪贴板操作注册表的类型。每个操作只声明一次：标签、图标、快捷键（按键位层）、出现在哪些界面、可用条件与执行；
 * 右键菜单、⌘K 操作菜单、⋯ 菜单、提示条、键位表、预览里的按钮与悬浮窗工具栏都从注册表派生（derive.ts），
 * 不在各处重复写快捷键与类型判断。
 */
import type { LucideIcon } from 'lucide-react'
import type { ViewHost } from '../../../panel/types'
import type { ClipItemMeta, OsKind, PasteMode, TransformId } from '../../../shared/ipc/types'
import type { MessageKey } from '../../../shared/i18n/i18n'
import type { TFunction } from '../../../shared/i18n/useT'
import type { ClipCaps, ClipHost } from '../host'
import type { Row } from '../model'
import type { ClipState } from '../store'
import type { ClipCommands } from '../useClipboard'

/** 全文预览的开关方式：菜单里是打开，Space 是切换，预览里的 Space / Esc 是关闭 */
export type PreviewArg = 'open' | 'close' | 'toggle'

/** 粘贴一个片段（芯片）：第几个片段（省略时是芯片条上有键盘焦点的那个）与粘贴方式（⌥ 点击 / ⌥↩ 只复制） */
export interface FragmentArg {
  readonly index?: number
  readonly mode: PasteMode
}

/**
 * 全部操作的 id 与各自的参数类型（没有参数的为 undefined）。注册表按它逐项实现（缺一项、多一项都编译不过），
 * 参数类型随之落到快捷键的 `arg` 与 `run` 上
 */
export interface ActionArgs {
  // 粘贴（多选时作用于全部标记的条目：按选择顺序以换行连接）
  paste: undefined
  paste_plain: undefined
  copy_only: undefined
  /** 「粘贴为…」子页（⌥⇧↩ 直接打开到这一页） */
  paste_as: undefined
  /** 按某种变换粘贴（子页里的项） */
  paste_transform: TransformId
  /** ⌘1–9：按当前可见顺序的第 n 条 */
  paste_nth: number
  paste_plain_nth: number
  // 条目
  preview: PreviewArg
  /** 收藏 / 取消收藏（多选时：有没收藏的就全部收藏，否则全部取消） */
  favorite: undefined
  /** 底部的命名条（命名即收藏） */
  rename: undefined
  /** ⌘E 进入芯片条；菜单里是「提取」子页 */
  extract: undefined
  /** 粘贴一个片段（芯片、「提取」子页里的项、芯片条上的 ↩） */
  paste_fragment: FragmentArg
  open_link: undefined
  reveal: undefined
  save_image: undefined
  /** 图片识别文字（面板切到 OCR 视图） */
  ocr: undefined
  translate: undefined
  monospace: undefined
  /** 复制一个派生片段（参数是片段文字） */
  copy_value: string
  /** true = 直接删除（收藏也不确认） */
  delete: boolean
  /** 连续粘贴多选的条目（⌘↩）：先粘贴第一条，之后在任何应用里每按一次粘贴键粘贴下一条 */
  queue_start: undefined
  // 视图
  /** ⌘P：交接到悬浮剪贴板 */
  float_open: undefined
  /** 悬浮剪贴板：置顶开关 */
  float_on_top: undefined
  /** 悬浮剪贴板：关闭 */
  float_close: undefined
  /** 结束进行中的连续粘贴 */
  queue_cancel: undefined
  /** 折叠 / 展开列表顶部的「收藏」分组 */
  favorites_toggle: undefined
  /** 「查看全部 N 个收藏」：切到收藏视图 */
  show_favorites: undefined
  pause: undefined
  settings: undefined
  clear: undefined
  // 只有快捷键：移动、过滤、菜单、预览与确认条
  move: 1 | -1
  move_edge: 'first' | 'last'
  move_page: 1 | -1
  scroll_preview: 1 | -1
  cycle_preview_focus: 1 | -1
  cycle_filter: 1 | -1
  open_filter_menu: undefined
  open_actions: undefined
  escape: undefined
  cancel_confirm: undefined
  clear_confirm: undefined
  /** 清空确认里的「保留收藏」；undefined = 切换 */
  keep_pinned: boolean
  /** 多选：⌘ 点击切换一条、⇧ 点击扩展到某一条（参数都是条目 id）、⇧↑↓ 扩展一步 */
  mark_toggle: number
  mark_range: number
  mark_extend: 1 | -1
  /** 芯片条：←/→ 移动、Esc 回到列表 */
  chip_move: 1 | -1
  chip_exit: undefined
  /** 命名条：↩ 保存、Esc 取消 */
  rename_commit: undefined
  rename_cancel: undefined
}

export type ClipActionId = keyof ActionArgs

/** 列出操作的界面：菜单与按钮组 */
export type MenuSurface =
  /** 右键菜单 */
  | 'context'
  /** ⌘K 操作菜单（选中项的操作 + 视图级操作，纯键盘也能到达全部操作） */
  | 'command'
  /** 头部的 ⋯ 菜单 */
  | 'more'
  /** 全文预览头部的图标按钮 */
  | 'preview_tools'
  /** 全文预览正文下方的按钮（图片另存为、在 Finder 中显示、打开链接） */
  | 'preview_actions'
  /** 条目里派生出的片段（颜色的 HEX / RGB / HSL 值） */
  | 'fragment'
  /** 视图头部的图标按钮（面板：交接到悬浮窗的钉子；悬浮窗：置顶、关闭、结束连续粘贴） */
  | 'toolbar'

/** 构造上下文时的来源：界面之外还有快捷键、指针（单击 / 双击行）、提示条与横幅 / 确认条上的按钮 */
export type ActionSource = MenuSurface | 'key' | 'pointer' | 'hint' | 'button'

/**
 * 键位层：每层对应键盘路由里的一个作用域。list = 视图层；preview = 全文预览打开时的覆盖层；confirm = 删除收藏的
 * 行内确认；clear = 清空历史的确认条（模态：其余按键都不落到列表上）；chips = 芯片条有键盘焦点时（←/→ / ↩ / Esc，
 * 其余按键照常落到列表）；rename = 命名条（模态：输入框里打字，↩ / Esc 由这一层处理）
 */
export type ClipKeyLayer = 'list' | 'preview' | 'confirm' | 'clear' | 'chips' | 'rename'

/** 提示条的模式：列表 / 全文预览 / 多选 / 芯片条（各自一组提示） */
export type HintMode = 'list' | 'preview' | 'marks' | 'chips'

/** 操作的作用对象数：none = 视图级；one = 光标所在条目（或打开菜单的那一行）；many = 多选标记的条目 */
export type Arity = 'none' | 'one' | 'many' | 'one_or_many'

/**
 * 分组：菜单在相邻的不同分组之间画分隔线（粘贴 | 查看与条目操作 | 删除 | 视图 | 记录 | 设置 | 清空）。nav 是只有
 * 快捷键 / 指针的移动与视图控制，不出现在菜单里
 */
export type ActionGroup =
  'paste' | 'item' | 'remove' | 'view' | 'record' | 'settings' | 'history' | 'nav'

export interface ActionCtx {
  readonly os: OsKind
  readonly host: ViewHost
  /** 宿主提供的能力（悬浮剪贴板是否可用…），见 host.ts */
  readonly caps: ClipCaps
  /** 置顶开关的当前状态（悬浮剪贴板；面板为 null） */
  readonly onTop: boolean | null
  readonly source: ActionSource
  /** 作用对象：光标所在条目，或打开菜单 / 单击的那一行；没有时为 null */
  readonly target: ClipItemMeta | null
  /**
   * 多选标记的条目（按选择顺序）。右键一个没有被标记的行时为空（菜单只作用于那一行，与 Finder 一致）
   */
  readonly marked: readonly ClipItemMeta[]
  readonly s: ClipState
  /** 按住不放时的重复按键（只有允许重复的快捷键会收到 true） */
  readonly repeat: boolean
  /** 全文预览提供的本地状态：等宽开关跟随预览本身（换条目时回到该类型的默认值），不进 store */
  readonly preview?: PreviewLocal
}

export interface PreviewLocal {
  readonly mono: boolean
  setMono(on: boolean): void
}

/** 视图提供给操作的界面能力（菜单、预览滚动、焦点、当前的行） */
export interface ClipViewUi {
  /** 当前显示的行（键盘移动、⌘n 按可见顺序） */
  rows(): readonly Row[]
  /** 列表区一屏能放下的行数（PageUp / PageDown） */
  pageRows(): number
  openFilterMenu(): void
  /** ⋯ 菜单（键盘打开，第一项高亮） */
  openMoreMenu(): void
  /**
   * 在光标所在行打开 ⌘K 操作菜单；给出 `page` 时直接打开到那个操作的子页（⌥⇧↩ →「粘贴为…」）。菜单已经打开时
   * 只换到那一页（菜单里的「粘贴为…」项）
   */
  openItemMenu(page?: ClipActionId): void
  scrollPreview(step: 1 | -1): void
  /** 全文预览打开时 Tab / ⇧Tab：在搜索框与预览的按钮之间移动焦点 */
  cyclePreviewFocus(step: 1 | -1): void
}

export interface ActionEnv {
  readonly cmd: ClipCommands
  readonly ui: ClipViewUi
  readonly host: ClipHost
}

export interface KeySpec<A> {
  /**
   * 组合串（keyRouter 的写法）；函数按平台给出，返回 null 表示这个平台不绑定（例如 ⌦ 只在 macOS 上是删除的备用键）。
   * 同一层里第一个可显示的组合就是菜单与提示条上显示的快捷键
   */
  readonly combo: string | ((os: OsKind) => string | null)
  /** 所在的键位层，默认只在 list */
  readonly layers?: readonly ClipKeyLayer[]
  /** 按键此刻是否属于这个操作（例如搜索框有字时 ⌫ 是退格）；不满足时按键照常落到搜索框 / 下一层 */
  when?(c: ActionCtx): boolean
  /** 按住不放的重复事件：swallow（默认）拦下但不执行，run 每次都执行（移动、翻页） */
  readonly repeat?: 'swallow' | 'run'
  /** 执行时的参数（↑ / ↓ 的方向、⌘n 的序号…） */
  readonly arg?: A
  /** 是否作为显示的快捷键：false 表示备用键（只绑定、不显示）；函数按上下文决定 */
  readonly display?: false | ((c: ActionCtx) => boolean)
  /**
   * shell：由面板外壳的全局层绑定（⌘, 打开设置），这里只用来在菜单里显示，不重复绑定；剪贴板自己的键不能占用它
   */
  readonly owner?: 'shell'
}

export interface SurfaceSpec<A> {
  /** 这个界面上额外的出现条件（快捷键不受它约束：⇧↩ 对文件条目粘贴路径文本，菜单却只为文本类条目列出它） */
  when?(c: ActionCtx): boolean
  /** 从这个界面执行时的参数（菜单里的「删除」对收藏直接删除） */
  readonly arg?: A
}

/** 子页里的一项：执行注册表里的另一个操作（「粘贴为…」→ paste_transform，「提取」→ paste_fragment） */
export interface PageItem {
  readonly key: string
  readonly label: string
  readonly icon?: LucideIcon
  /** 菜单项右侧的说明（片段的种类） */
  readonly hint?: string
  readonly action: ClipActionId
  readonly arg: unknown
}

/** 子页里的一项（参数类型跟随要执行的操作） */
export function pageItem<I extends ClipActionId>(
  action: I,
  arg: ActionArgs[I],
  item: Omit<PageItem, 'action' | 'arg'>,
): PageItem {
  return { ...item, action, arg }
}

export interface HintSpec {
  /** 提示条里的显示顺序（动作的逻辑顺序） */
  readonly order: number
  /** 放不下时的取舍，越小越重要（hints.ts） */
  readonly priority: number
  /**
   * 提示条上的短标签：固定不随条目变化，移动选中项时提示条不跳动；函数按宿主的能力取（例如 Esc 在面板里是「关闭」、
   * 在悬浮窗里是「返回」）
   */
  readonly label: MessageKey | ((c: ActionCtx) => MessageKey)
  /** 显示哪一层的按键（默认 list） */
  readonly layer?: ClipKeyLayer
  /** 显示这一层全部可显示的按键（↑↓ 浏览），默认只显示第一个 */
  readonly all?: boolean
}

export interface ClipAction<A = undefined> {
  readonly id: ClipActionId
  readonly group: ActionGroup
  readonly arity: Arity
  /** 只在这些宿主里提供（默认全部） */
  readonly hosts?: readonly ViewHost[]
  /** 菜单 / 按钮上的标签（返回文案 key：测试据此核对两种语言都有） */
  label?(c: ActionCtx): MessageKey
  readonly icon?: LucideIcon
  /** 随状态变化的图标（收藏 / 取消收藏、暂停 / 恢复）；给出时优先于 `icon` */
  iconFor?(c: ActionCtx): LucideIcon
  readonly keys?: readonly KeySpec<A>[]
  readonly surfaces?: Partial<Record<MenuSurface, SurfaceSpec<A>>>
  readonly hints?: Partial<Record<HintMode, HintSpec>>
  /** 是否适用（例如「打开链接」只对链接）：不适用时任何界面都不列出，快捷键拦下但不执行 */
  when?(c: ActionCtx): boolean
  /** 列出但置灰（例如文件都已不存在时的「在 Finder 中显示」） */
  enabled?(c: ActionCtx): boolean
  /** 开关类操作的当前状态（按钮的 aria-pressed） */
  checked?(c: ActionCtx): boolean
  readonly danger?: boolean
  /** 含这一项的菜单打开时调用一次（例如「清空历史…」先读清空会不会同步到其他设备） */
  prepare?(c: ActionCtx, x: ActionEnv): void
  /**
   * 菜单里的子页（「粘贴为…」、「提取」）：给出时菜单项带「›」、选中它不关闭菜单而是换到这一页（执行 `run`，由
   * `ui.openItemMenu(id)` 换页）
   */
  page?(c: ActionCtx, t: TFunction): readonly PageItem[]
  /** 执行；返回 false 表示没有处理（Esc 交给外壳） */
  run(c: ActionCtx, x: ActionEnv, arg?: A): boolean | void
}

/** 菜单项 / 按钮的派生结果 */
export interface ActionEntry {
  readonly id: ClipActionId
  readonly group: ActionGroup
  readonly label: string
  readonly icon: LucideIcon | undefined
  readonly shortcut: string | null
  readonly disabled: boolean
  readonly danger: boolean
  readonly checked: boolean | undefined
  /** 有子页（菜单项带「›」，选中后换页） */
  readonly submenu: boolean
  run(): void
}

/** 提示条的一项 */
export interface HintItem {
  readonly id: ClipActionId
  readonly combos: readonly string[]
  readonly label: string
  readonly priority: number
}
