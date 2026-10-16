/**
 * 识别出的片段（芯片）的展示部件：剪贴板的芯片条 / 预览里的芯片与 OCR 视图的芯片条共用同一个芯片与同一组样式
 * （chips.css），文案与图标来自展示注册表（specs.ts，文案在 `smart` 命名空间）。只负责画出来与点击的方式（单击粘贴、
 * ⌥ 单击只复制），粘贴由调用方决定走哪条命令。
 *
 * 焦点一直在各自视图的输入框里：芯片不进 Tab 顺序（剪贴板经 ⌘E 进入芯片条）；证件号、银行卡号只显示首尾几位，
 * 无障碍名称同样遮罩。
 */
import './chips.css'
import type { MouseEvent as ReactMouseEvent } from 'react'
import type { Entity, OsKind } from '../ipc/types'
import { useT } from '../i18n/useT'
import { formatModifiers } from '../keys/formatHotkey'
import { Icon } from '../ui'
import { entityText, ENTITY_SPECS } from './specs'

/** 单击粘贴；⌥ / Alt 单击只复制（与芯片条上的 ⌥↩ 一致） */
export type ChipMode = 'normal' | 'copy_only'

export function chipMode(e: ReactMouseEvent): ChipMode {
  return e.altKey ? 'copy_only' : 'normal'
}

/** 芯片的提示（「单击粘贴，⌥ 单击只复制」，修饰键按平台写法） */
export function useChipTip(os: OsKind): string {
  const t = useT()
  const alt = formatModifiers({ ctrl: false, alt: true, shift: false, meta: false }, os).join('')
  return t('smart.chip.tip', { alt })
}

/** 一个芯片：种类图标 + 值（敏感的遮罩） */
export function EntityChip({
  entity,
  id,
  active = false,
  tip,
  onPick,
}: {
  entity: Entity
  /** 有键盘焦点的芯片条里才带 id（输入框的 aria-activedescendant 指向它） */
  id?: string
  active?: boolean
  tip: string
  onPick(mode: ChipMode): void
}) {
  const t = useT()
  const spec = ENTITY_SPECS[entity.kind]
  const text = entityText(entity)
  return (
    <button
      type="button"
      id={id}
      className="clip-chip"
      tabIndex={-1}
      title={tip}
      aria-label={t('smart.chip.label', { kind: t(spec.label), value: text })}
      data-kind={entity.kind}
      data-active={active || undefined}
      data-testid="clip-chip"
      onClick={(e) => {
        // 悬停卡片里的芯片：点击不再冒泡成「点卡片 = 点这一行」（那会粘贴整条）
        e.stopPropagation()
        onPick(chipMode(e))
      }}
    >
      <Icon icon={spec.icon} size={12} className="clip-chip-icon" />
      <span className="clip-chip-text">{text}</span>
    </button>
  )
}
