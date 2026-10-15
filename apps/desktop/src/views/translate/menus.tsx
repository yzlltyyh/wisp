/**
 * 翻译视图头部的菜单：风格 chip 菜单、⋯ 菜单与切换模型菜单。
 * 菜单本体、动效、键盘导航都来自组件库（shared/ui/Menu，presets.menuIn / menuOut）。
 *
 * - **风格菜单**是组件库的 StyleMenu（OCR 的风格菜单、固定窗口共用同一份——分组标题、⌘1–9、数字直选、
 *   「编辑当前风格」），这里只给它 StyleChip 作触发器；
 * - **按下就打开**（组件库 menuOpener）：指针按下、Enter / Space / ↓ 按下的那一刻就打开、播放 menuIn；
 * - **预留与下拉帘**（组件库的浮层定位，floating.tsx）：打开时按菜单高度预留卡片高度，卡片 / 窗口追赶期间按卡片当前
 *   可见的下沿裁切菜单底部（保留菜单的圆角）——与卡片弹簧同步的下拉帘；Css 帧里预留引起的长高用 spring.snappy；关闭时
 *   等菜单淡出（100 ms）之后才释放预留，卡片不在菜单淡出的同时缩回；
 * - **浏览器加速键**：菜单打开时 ⌘R / ⌘⇧R / ⌘P 同样吞掉（Windows WebView2 默认会重载 / 打印，shared/keys/menuKeys）；
 * - **焦点**：由组件库的 Menu 负责（定位完成时移进菜单；关闭后焦点还在退场的菜单里或落到 body 时还给触发器）；
 * - 提升译文不可用的原因是该项的第二行说明（Menu 的 `description`），图标与勾选共用前导列，文字左缘对齐；
 * - 矮面板里 ⋯ 菜单放不下时收起低频项（「编辑当前风格」在风格菜单里也有、提升译文不可用的原因），保证关键项完整可见。
 *
 * 菜单第一次打开时才挂载（面板 prepare 的同步首帧里不渲染）。
 */
import {
  ChevronLeft,
  Copy,
  Cpu,
  Ellipsis,
  History,
  SlidersHorizontal,
  Sparkles,
  SpellCheck,
  WandSparkles,
} from 'lucide-react'
import { useEffect, useRef, useState } from 'react'
import { useT } from '../../shared/i18n/useT'
import type { ModelRef, TaskKind } from '../../shared/ipc/types'
import { useMenuKeys } from '../../shared/keys/menuKeys'
import {
  estimateMenuHeight,
  IconButton,
  Menu,
  StyleChip,
  StyleMenu as LibStyleMenu,
  useMenuOpener,
  useMenuRoom,
  type MenuEntry,
} from '../../shared/ui'
import type { ModelChoice, StyleInfo } from './catalog'
import { loadTranslateMessages } from './i18n'

loadTranslateMessages()

/** 能用 ⌘n / 数字直选的风格数（⌘1–9） */
export { STYLE_SHORTCUTS } from '../../shared/ui'

export function useStyleName(): (s: StyleInfo) => string {
  const t = useT()
  return (s) => (s.builtin ? t(`styles.${s.id}` as 'styles.natural') : s.name || s.id)
}

export interface StyleMenuProps {
  styles: readonly StyleInfo[]
  current: string
  onPick: (id: string) => void
  onEdit: () => void
}

export function StyleMenu({ styles, current, onPick, onEdit }: StyleMenuProps) {
  const t = useT()
  const name = useStyleName()
  const currentStyle = styles.find((s) => s.id === current)
  const styleName = currentStyle ? name(currentStyle) : t('styles.custom')
  return (
    <LibStyleMenu
      styles={styles.map((s) => ({ id: s.id, name: name(s) }))}
      current={current}
      onPick={onPick}
      onEdit={onEdit}
      label={t('translate.a11y.style_menu')}
      align="end"
      data-testid="tv-style-menu"
      renderTrigger={(p) => (
        <StyleChip
          {...p}
          name={styleName}
          aria-label={t('translate.a11y.style_menu_current', { name: styleName })}
          data-testid="tv-style-chip"
        />
      )}
    />
  )
}

export interface MoreMenuProps {
  task: TaskKind
  /** 提升译文是否可用、不可用时的原因 */
  refine: { enabled: boolean; hint: string | null }
  /** 提升译文已完成：可以在原译文与提升后的译文之间切换 */
  refined: 'improved' | 'original' | null
  models: readonly ModelChoice[]
  /** 本会话临时指定的模型（null = 默认模型） */
  model: ModelRef | null
  /** 默认模型的显示名（设置里的默认翻译模型；没有时为 null） */
  defaultModel: string | null
  onTask: (task: 'polish' | 'proofread') => void
  onRefine: () => void
  onToggleRefined: () => void
  onCopySource: () => void
  onModel: (ref: ModelRef | null) => void
  onEditStyle: () => void
  /** 外部要求打开模型菜单（错误卡片 / 思考耗尽的「切换模型」） */
  modelMenuRequest?: number
}

/** ⋯ 菜单在矮面板里的收起程度：0 完整；1 去掉原因小字与「编辑当前风格」；2 再去掉分隔线；3 再去掉不可用的提升译文 */
type Compact = 0 | 1 | 2 | 3

export function MoreMenu(props: MoreMenuProps) {
  const t = useT()
  const trigger = useRef<HTMLButtonElement>(null)
  const room = useMenuRoom(trigger)
  const label = t('common.a11y.more_actions')
  const [compact, setCompact] = useState<Compact>(0)
  const current = props.model
    ? (props.models.find(
        (m) =>
          m.ref.provider_id === props.model?.provider_id && m.ref.model_id === props.model.model_id,
      )?.label ?? props.model.model_id)
    : props.defaultModel

  const itemsFor = (level: Compact): MenuEntry[] => {
    const refineDisabled = props.refined === null && !props.refine.enabled
    const list: MenuEntry[] = [
      {
        id: 'polish',
        label: t('translate.more.polish'),
        icon: Sparkles,
        checked: props.task === 'polish' ? true : undefined,
        onSelect: () => props.onTask('polish'),
      },
      {
        id: 'proofread',
        label: t('translate.more.proofread'),
        icon: SpellCheck,
        checked: props.task === 'proofread' ? true : undefined,
        onSelect: () => props.onTask('proofread'),
      },
    ]
    if (props.refined !== null) {
      list.push({
        id: 'refined',
        label:
          props.refined === 'improved'
            ? t('translate.more.show_original')
            : t('translate.more.show_refined'),
        icon: props.refined === 'improved' ? History : WandSparkles,
        onSelect: props.onToggleRefined,
      })
    } else if (!(refineDisabled && level >= 3)) {
      list.push({
        id: 'refine',
        label: t('translate.more.refine'),
        icon: WandSparkles,
        // 置灰项不显示快捷键（按了也没用）；原因是标签下方的第二行说明（不与标签抢一行的宽度，窄面板 / 英文也完整显示）
        shortcut: props.refine.enabled ? 'Mod+Shift+KeyR' : null,
        disabled: !props.refine.enabled,
        description:
          refineDisabled && props.refine.hint && level < 1 ? props.refine.hint : undefined,
        onSelect: props.onRefine,
      })
    }
    if (level < 2) list.push({ type: 'separator', id: 'sep1' })
    list.push(
      {
        id: 'copy-source',
        label: t('translate.more.copy_source'),
        icon: Copy,
        onSelect: props.onCopySource,
      },
      {
        id: 'model',
        label: t('translate.more.switch_model'),
        icon: Cpu,
        // 右侧显示当前模型：选择后在原位打开模型菜单（菜单里有「返回」），不是级联子菜单
        hint: current ?? undefined,
        onSelect: () => modelOpener.request('keyboard'),
      },
    )
    // 「编辑当前风格」在风格菜单里也有：矮面板里先收起它
    if (level < 1)
      list.push({
        id: 'edit-style',
        label: t('translate.more.edit_style'),
        icon: SlidersHorizontal,
        onSelect: props.onEditStyle,
      })
    return list
  }
  const items = itemsFor(compact)

  /** 卡片上限放得下的收起程度（打开时决定，打开期间不变） */
  const fitLevel = (): Compact => {
    const space = room()
    for (const level of [0, 1, 2, 3] as const) {
      if (estimateMenuHeight(itemsFor(level)) <= space) return level
    }
    return 3
  }

  // 两个菜单都在第一次打开时才挂载（面板 prepare 的同步首帧里不渲染），之后保留以播放退出动画。⋯ 菜单打开之前按卡片
  // 上限决定收起程度
  const opener = useMenuOpener(trigger, () => setCompact(fitLevel()))
  const modelOpener = useMenuOpener(trigger)
  useMenuKeys(opener.open || modelOpener.open, [])

  // 错误卡片 / 思考耗尽的「切换模型」：外部计数器变化时打开模型菜单（挂载时的初值不算）
  const seenRequest = useRef(props.modelMenuRequest ?? 0)
  useEffect(() => {
    const n = props.modelMenuRequest ?? 0
    if (n === seenRequest.current) return
    seenRequest.current = n
    modelOpener.request('keyboard')
    // 只在计数器变化时执行
    // oxlint-disable-next-line react/exhaustive-deps
  }, [props.modelMenuRequest])

  return (
    <>
      <IconButton
        ref={trigger}
        icon={Ellipsis}
        label={label}
        noTooltip={opener.open || modelOpener.open}
        aria-haspopup="menu"
        aria-expanded={opener.open || modelOpener.open}
        data-open={opener.open || modelOpener.open || undefined}
        data-testid="tv-more"
        onPointerDown={opener.onPointerDown}
        onClick={opener.onClick}
        onKeyDown={opener.onKeyDown}
      />
      {opener.mounted ? (
        <Menu
          open={opener.open}
          anchor={trigger}
          items={items}
          align="end"
          initialActive={opener.via === 'keyboard' ? 'first' : 'none'}
          label={label}
          onClose={() => opener.close()}
          data-testid="tv-more-menu"
        />
      ) : null}
      {modelOpener.mounted ? (
        <Menu
          open={modelOpener.open && !opener.open}
          anchor={trigger}
          items={modelItems(
            props,
            t,
            () => {
              modelOpener.close()
              opener.request('keyboard')
            },
            props.onModel,
          )}
          align="end"
          initialActive="selected"
          label={t('translate.a11y.model_menu')}
          onClose={() => modelOpener.close()}
          data-testid="tv-model-menu"
        />
      ) : null}
    </>
  )
}

/** 切换模型菜单的项：「返回」、默认模型（提示实际模型名）、按服务分组的模型 */
function modelItems(
  props: Pick<MoreMenuProps, 'models' | 'model' | 'defaultModel'>,
  t: ReturnType<typeof useT>,
  onBack: () => void,
  onPick: (ref: ModelRef | null) => void,
): MenuEntry[] {
  const { models, model, defaultModel } = props
  const items: MenuEntry[] = [
    // ‹ 在前导列里，文字与下面的勾选项左缘对齐
    { id: 'back', label: t('translate.model.back'), icon: ChevronLeft, onSelect: onBack },
    { type: 'separator', id: 'sep-back' },
    {
      id: 'default',
      label: t('translate.model.default'),
      hint: defaultModel ?? t('translate.model.default_hint'),
      checked: model === null,
      onSelect: () => onPick(null),
    },
  ]
  if (models.length === 0) {
    items.push({ id: 'none', label: t('translate.model.none'), disabled: true })
  }
  let provider: string | null = null
  for (const m of models) {
    if (m.providerName !== provider) {
      provider = m.providerName
      items.push({ type: 'separator', id: `sep-${m.ref.provider_id}` })
      items.push({ type: 'label', id: `p-${m.ref.provider_id}`, label: m.providerName })
    }
    items.push({
      id: `${m.ref.provider_id}/${m.ref.model_id}`,
      label: m.label,
      checked:
        model !== null &&
        model.provider_id === m.ref.provider_id &&
        model.model_id === m.ref.model_id,
      onSelect: () => onPick(m.ref),
    })
  }
  return items
}
