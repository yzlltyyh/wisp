/**
 * 底栏的排版分体按钮 [整理 | ▾]（占「源码 / 预览」的位置，两者互斥）：
 * - 左半在「原样」与本次会话最近用过的整理方式之间切换（⌘L）；按下态沿用分体按钮的选中样式，aria-pressed 表示是否
 *   已整理，名称是那种整理方式；最近用过的方式变了，图标用 iconSwap 换过去；
 * - ▾ 打开单选菜单：原样 / 去掉空行 / 合并段内换行 / 合并为一行，各带一行说明（矮面板里先收起说明、再收起标题，整份
 *   菜单完整可见）；只列当前模式下可用的排版（合并段内换行只在逐行模式：段落模式的结果已经按段合并过）；
 *   不适用的置灰并把原因写在说明里——适用性来自芯片条对识别文本的检视，前端不另写判断；
 * - 文字改过之后不能切换（左半置灰、提示「文字已修改」），菜单里多出「放弃修改，恢复识别结果」（由视图先确认）。
 */
import { AnimatePresence, m } from 'motion/react'
import { useRef, useState } from 'react'
import type { MessageKey } from '../../../shared/i18n/i18n'
import { useT } from '../../../shared/i18n/useT'
import { OCR_LAYOUTS, layoutAvailableIn, type OcrLayout, type OcrMode } from '../../../shared/ipc'
import { usePreset } from '../../../shared/motion/presets'
import { TRANSFORM_SPECS } from '../../../shared/smart/specs'
import {
  DropdownMenu,
  Icon,
  IconButton,
  Tooltip,
  estimateMenuHeight,
  useMenuRoom,
  type MenuEntry,
} from '../../../shared/ui'
import { Split, SplitCaret } from '../parts'
import type { Tidy } from './model'
import type { OcrText } from './useOcrText'

/** 排版的显示名：两种整理方式与「粘贴为…」里的同名变换共用文案 */
export const layoutLabel = (l: OcrLayout): MessageKey =>
  l === 'keep' ? 'ocr.layout.keep' : TRANSFORM_SPECS[l].label
/** 说明行较长，给菜单一个下限宽度（与识别菜单相近），说明不至于每句都折成两行 */
const MENU_WIDTH = 240

export function LayoutSplit({
  text,
  mode,
  disabled,
  onDiscard,
}: {
  text: OcrText
  /** 当前识别模式：菜单只列这个模式下可用的排版 */
  mode: OcrMode
  /** 正在重新识别：整组置灰（不隐藏，底栏不跳） */
  disabled: boolean
  /** 菜单里的「放弃修改」（视图先确认） */
  onDiscard(): void
}) {
  const t = useT()
  const swap = usePreset('iconSwap')
  const box = useRef<HTMLSpanElement>(null)
  const room = useMenuRoom(box)
  /** 0 完整；1 收起说明行（不适用的原因保留）；2 再收起标题 */
  const [compact, setCompact] = useState(0)
  // 按下态与菜单勾选取选中的排版（会话排版在这个模式下可用时就是它）：按钮出现时只有正在重新识别（整组置灰）这段时间
  // 排版不作用，此时显示的也应是它，不闪成「原样」
  const { selected: layout, recent, edited } = text
  const pressed = layout !== 'keep'
  const name = (l: OcrLayout): string => t(layoutLabel(l))
  const reason = (l: Tidy): string => t(`ocr.layout.unavailable.${l}`)
  // 左半不能按：文字改过了；或者正处于原样、而最近用过的整理方式对这段文字不适用（按了也不会有变化）
  const blocked = edited
    ? t('ocr.layout.edited')
    : !pressed && !text.can(recent)
      ? reason(recent)
      : null
  const items = (level: number): MenuEntry[] => [
    ...(level < 2 ? [{ type: 'label' as const, id: 'title', label: t('ocr.layout.title') }] : []),
    ...OCR_LAYOUTS.filter((l) => layoutAvailableIn(l, mode)).map((l): MenuEntry => {
      const ok = text.can(l)
      return {
        id: l,
        label: name(l),
        radio: true,
        checked: l === layout,
        disabled: edited || !ok,
        description:
          !ok && l !== 'keep' ? reason(l) : level < 1 ? t(`ocr.layout.hint.${l}`) : undefined,
        onSelect: () => text.choose(l),
      }
    }),
    ...(edited
      ? [
          { type: 'separator' as const, id: 'sep' },
          { id: 'discard', label: t('ocr.layout.discard'), onSelect: onDiscard },
        ]
      : []),
  ]
  const fit = (): number => {
    const max = room()
    for (const level of [0, 1]) if (estimateMenuHeight(items(level)) <= max) return level
    return 2
  }
  const spec = TRANSFORM_SPECS[recent]
  return (
    <DropdownMenu
      items={items(compact)}
      label={t('ocr.layout.title')}
      side="top"
      align="start"
      minWidth={MENU_WIDTH}
      highlightSelected
      onOpenChange={(open) => {
        if (open) setCompact(fit())
      }}
      data-testid="ocr-layout-menu"
      renderTrigger={(trigger, open) => (
        <Split ref={box} selected={pressed} open={open} data-testid="ocr-layout">
          <Tooltip
            content={blocked ?? name(recent)}
            shortcut={blocked ? null : 'Mod+KeyL'}
            disabled={disabled || open}
          >
            <IconButton
              icon={spec.icon}
              glyph={
                <span className="ui-iconswap" aria-hidden="true">
                  <AnimatePresence initial={false} mode="popLayout">
                    <m.span key={recent} className="ui-iconswap-item" {...swap}>
                      <Icon icon={spec.icon} size={16} />
                    </m.span>
                  </AnimatePresence>
                </span>
              }
              label={name(recent)}
              active={pressed}
              noTooltip
              disabled={disabled}
              aria-disabled={blocked !== null || undefined}
              onClick={blocked === null ? text.toggle : undefined}
              data-testid="ocr-layout-toggle"
            />
          </Tooltip>
          <SplitCaret
            {...trigger}
            label={t('ocr.layout.menu')}
            disabled={disabled}
            data-testid="ocr-layout-caret"
          />
        </Split>
      )}
    />
  )
}
