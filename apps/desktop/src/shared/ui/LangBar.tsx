/**
 * LangBar：翻译 / OCR 视图头部的语言条「英语 → 中文 ▾」。
 *
 * - **两端同一种写法**：默认都写全称（「英语 → 中文」，交换方向只是左右对调、文字本身不变）；`compact` 时两端一起
 *   降级为短码（「EN → 中文」）。不会一端短码、一端全称（交换前后同一门语言写法不同，读起来像换了一门语言）；
 * - 源语言按钮就是「交换方向」的入口：无障碍名称以**可见文字开头**（WCAG 2.5.3：「英语 · 交换翻译方向」，短码时
 *   「EN（英语）· 交换翻译方向」——语音控制说出看到的字就能点到），悬停 / 键盘聚焦时组件库的 Tooltip 说明
 *   （`swapLabel`；不用原生 title：系统样式的提示与其他控件不一致，读屏还会把它和无障碍名称读两遍）；
 * - 交换没有意义时（`swapDisabledReason`，例如自动检测出的源语言与交换后的目标相同）用 `aria-disabled`：按钮仍可
 *   聚焦、可悬停，Tooltip 说明原因；检测中、没有 onSwap 时真正禁用（没有可说明的内容）；
 * - 目标语言按钮的无障碍名称同样以可见文字开头，说明它打开「目标语言」菜单（「中文 · 目标语言」，`targetLabel`）；
 * - 低置信度时源语言后面加一个弱提示点。
 */
import { ArrowRight, ChevronDown } from 'lucide-react'
import { useRef, useState } from 'react'
import { useT } from '../i18n/useT'
import type { Lang } from '../ipc/types'
import { cx } from './cx'
import { Icon } from './Icon'
import { Menu, type MenuEntry } from './Menu'
import { Tooltip } from './Tooltip'

export interface LangBarProps {
  /** 检测到的源语言；null 且 detecting 为 true 时显示「检测中…」 */
  source: Lang | null
  target: Lang
  detecting?: boolean
  lowConfidence?: boolean
  /** 可选的目标语言（打开菜单）；不传则用全部语言 */
  targets?: readonly Lang[]
  /** 不传则目标语言不可点 */
  onPickTarget?: (lang: Lang) => void
  /** 交换方向；不传则源语言不可点 */
  onSwap?: () => void
  /** 交换方向的说明（Tooltip 与无障碍名称的后半句），默认「交换翻译方向」 */
  swapLabel?: string
  /** 交换不可用的原因：给出时源语言按钮 aria-disabled、Tooltip 显示原因（onSwap 不会被调用） */
  swapDisabledReason?: string | null
  /** 目标语言按钮的无障碍名称，默认「目标语言：中文」 */
  targetLabel?: string
  /** 目标语言菜单打开 / 关闭（视图据此在菜单打开期间吞掉浏览器加速键等） */
  onMenuOpenChange?: (open: boolean) => void
  /** 两端都用短码（「EN → 中文」）：头部放不下全称时由视图给出 */
  compact?: boolean
  className?: string
  'data-testid'?: string
}

const LANGS: readonly Lang[] = ['zh-CN', 'en', 'ja', 'ko', 'other']

export function LangBar({
  source,
  target,
  detecting = false,
  lowConfidence = false,
  targets,
  onPickTarget,
  onSwap,
  swapLabel,
  swapDisabledReason = null,
  targetLabel,
  onMenuOpenChange,
  compact = false,
  className,
  'data-testid': testId = 'lang-bar',
}: LangBarProps) {
  const t = useT()
  const targetRef = useRef<HTMLButtonElement>(null)
  const [open, setOpenState] = useState(false)
  const setOpen = (next: boolean) => {
    setOpenState(next)
    onMenuOpenChange?.(next)
  }
  const code = (l: Lang) => t(`common.lang.code.${l}`)
  const name = (l: Lang) => t(`common.lang.name.${l}`)
  /** 两端同一种写法：全称，或一起用短码 */
  const shown = (l: Lang) => (compact ? code(l) : name(l))
  /** 无障碍名称的开头：可见文字；短码与全称不同时括注全称 */
  const spoken = (l: Lang) =>
    shown(l) === name(l)
      ? name(l)
      : t('common.lang.code_with_name', { code: code(l), name: name(l) })
  const items: MenuEntry[] = (targets ?? LANGS).map((l) => ({
    id: l,
    label: name(l),
    checked: l === target,
    onSelect: () => onPickTarget?.(l),
  }))
  const blocked = !detecting && !!onSwap && swapDisabledReason !== null
  const canSwap = !detecting && !!onSwap && !blocked
  const swapText = swapLabel ?? t('common.a11y.swap_direction')
  const sourceName = source ? spoken(source) : t('common.lang.auto')
  const tip = canSwap ? swapText : blocked ? swapDisabledReason : null
  return (
    <div className={cx('ui-langbar', className)} data-testid={testId}>
      <Tooltip content={tip} disabled={tip === null}>
        <button
          type="button"
          className="ui-langbar-part"
          disabled={detecting || !onSwap}
          aria-disabled={blocked || undefined}
          aria-label={canSwap ? `${sourceName} · ${swapText}` : sourceName}
          onClick={canSwap ? onSwap : undefined}
          data-testid={`${testId}-source`}
        >
          <span className="ui-langbar-source" data-detecting={detecting || !source || undefined}>
            {detecting
              ? t('common.state.detecting')
              : source
                ? shown(source)
                : t('common.lang.auto')}
          </span>
          {lowConfidence ? <span className="ui-langbar-dot" aria-hidden="true" /> : null}
        </button>
      </Tooltip>
      <Icon icon={ArrowRight} size={12} className="ui-langbar-arrow" />
      <button
        ref={targetRef}
        type="button"
        className="ui-langbar-part"
        aria-haspopup={onPickTarget ? 'menu' : undefined}
        aria-expanded={onPickTarget ? open : undefined}
        aria-label={targetLabel ?? t('common.a11y.target_lang', { name: spoken(target) })}
        disabled={!onPickTarget}
        onClick={() => setOpen(!open)}
        data-testid={`${testId}-target`}
      >
        <span className="ui-langbar-target">{shown(target)}</span>
        {onPickTarget ? <Icon icon={ChevronDown} size={12} className="ui-chip-caret" /> : null}
      </button>
      {onPickTarget ? (
        <Menu
          open={open}
          anchor={targetRef}
          items={items}
          initialActive="selected"
          label={t('common.a11y.target_menu')}
          onClose={() => setOpen(false)}
          data-testid={`${testId}-menu`}
        />
      ) : null}
    </div>
  )
}
