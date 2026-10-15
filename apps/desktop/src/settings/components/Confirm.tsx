/**
 * 危险操作的二次确认，**在触发的位置原地变形**：
 * - `DangerRow`：卡片里的一行（删除服务 / 风格、清空历史、还原默认）。平时右侧是红字的次级按钮；点了之后整行变成
 *   确认条「确认删除？[取消][删除]」，实心红色只出现在确认时，同一屏不会有两个红按钮；
 * - `HeaderConfirm`：分组标题右侧的操作（「全部恢复默认」）。确认就地出现在标题右侧，不跑到卡片下面；
 * - `useConfirmKeys`：打开时焦点移到「取消」、滚到可见位置；Esc 取消（overlay 层，先于编辑器的 Esc 返回）；
 *   取消后焦点回到触发按钮。
 * 视觉沿用组件库 InlineConfirm 的确认条（`.ui-confirm`），出现时 presets.footerIn 淡入。
 * 原地变形不改变高度：确认条的最小高度取点击时那一行的实际高度（有说明的行 52、没有的 44），页面不会跳 8 px，
 * 页尾的行打开 / 取消确认时 scrollTop 也不变；所以这里不需要 confirmIn 的高度弹簧（那是给插在下方的确认条用的）。
 */
import { Trash2, TriangleAlert, type LucideIcon } from 'lucide-react'
import { m } from 'motion/react'
import { useEffect, useRef, useState, type ReactNode, type RefObject } from 'react'
import { useT } from '../../shared/i18n/useT'
import { useKeyScope } from '../../shared/keys/useKeyScope'
import { usePreset } from '../../shared/motion/presets'
import { useReducedMotionPref } from '../../shared/motion/useReducedMotionPref'
import { Button, Field, Icon } from '../../shared/ui'

/** 确认条打开时：焦点给「取消」、滚到可见位置、Esc 取消；关闭后焦点回到触发按钮 */
export function useConfirmKeys(
  open: boolean,
  onCancel: () => void,
  refs: {
    cancel: RefObject<HTMLButtonElement | null>
    container: RefObject<HTMLElement | null>
    trigger?: RefObject<HTMLButtonElement | null>
  },
): void {
  const reduced = useReducedMotionPref()
  useKeyScope('confirm', [{ combo: 'Escape', allowInInput: true, run: () => onCancel() }], {
    layer: 'overlay',
    enabled: open,
  })
  const wasOpen = useRef(open)
  useEffect(() => {
    if (open) {
      refs.cancel.current?.focus({ preventScroll: true })
      refs.container.current?.scrollIntoView?.({
        block: 'nearest',
        behavior: reduced ? 'auto' : 'smooth',
      })
    } else if (wasOpen.current) {
      // 取消（或确认后这一行还在）：焦点回到触发按钮，键盘用户不用从头 Tab
      const active = document.activeElement
      if (!active || active === document.body) refs.trigger?.current?.focus({ preventScroll: true })
    }
    wasOpen.current = open
    // refs 是稳定的 ref 对象
    // eslint-disable-next-line react/exhaustive-deps
  }, [open, reduced])
}

export interface DangerRowProps {
  label: ReactNode
  description?: ReactNode
  /** 触发按钮的文字（「删除服务」「清空历史」） */
  action: string
  /** 确认条的文字 */
  confirm: ReactNode
  /** 确认按钮的文字（默认同 action） */
  confirmLabel?: string
  onConfirm: () => void
  icon?: LucideIcon
  /** 还原类操作：确认条用强调色而不是红色 */
  tone?: 'danger' | 'default'
  disabled?: boolean
  'data-testid'?: string
}

export function DangerRow({
  label,
  description,
  action,
  confirm,
  confirmLabel,
  onConfirm,
  icon = Trash2,
  tone = 'danger',
  disabled = false,
  'data-testid': testId,
}: DangerRowProps) {
  const t = useT()
  const [open, setOpen] = useState(false)
  const cancel = useRef<HTMLButtonElement>(null)
  const container = useRef<HTMLDivElement>(null)
  const trigger = useRef<HTMLButtonElement>(null)
  const fade = usePreset('footerIn')
  /** 点击时那一行的高度：确认条至少这么高（原地变形，不让布局跳动） */
  const [rowHeight, setRowHeight] = useState<number | null>(null)
  useConfirmKeys(open, () => setOpen(false), { cancel, container, trigger })

  if (open) {
    return (
      <m.div
        ref={container}
        className="st-confirm-row ui-confirm"
        data-tone={tone}
        role="alert"
        style={rowHeight ? { minHeight: rowHeight } : undefined}
        data-testid={testId ? `${testId}-confirm` : undefined}
        {...fade}
      >
        <Icon icon={TriangleAlert} size={14} className="ui-confirm-icon" />
        <span className="ui-confirm-text">{confirm}</span>
        <span className="ui-confirm-actions">
          <Button ref={cancel} size="sm" variant="ghost" onClick={() => setOpen(false)}>
            {t('common.action.cancel')}
          </Button>
          <Button
            size="sm"
            variant={tone === 'danger' ? 'danger' : 'primary'}
            onClick={() => {
              setOpen(false)
              onConfirm()
            }}
            data-testid={testId ? `${testId}-ok` : undefined}
          >
            {confirmLabel ?? action}
          </Button>
        </span>
      </m.div>
    )
  }
  return (
    <Field
      label={label}
      description={description}
      data-testid={testId ? `${testId}-row` : undefined}
    >
      <Button
        ref={trigger}
        size="sm"
        icon={icon}
        className={tone === 'danger' ? 'st-btn-danger' : undefined}
        disabled={disabled}
        onClick={() => {
          const row = trigger.current?.closest('.ui-fieldrow')
          setRowHeight(row ? row.getBoundingClientRect().height : null)
          setOpen(true)
        }}
        data-testid={testId}
      >
        {action}
      </Button>
    </Field>
  )
}

/** 分组标题右侧的操作 + 就地确认（「全部恢复默认」） */
export function HeaderConfirm({
  action,
  confirm,
  confirmLabel,
  onConfirm,
  icon,
  'data-testid': testId,
}: {
  action: string
  /** 简短的确认问句（标题行放得下） */
  confirm: string
  confirmLabel: string
  onConfirm: () => void
  icon?: LucideIcon
  'data-testid'?: string
}) {
  const t = useT()
  const [open, setOpen] = useState(false)
  const cancel = useRef<HTMLButtonElement>(null)
  const container = useRef<HTMLSpanElement>(null)
  const trigger = useRef<HTMLButtonElement>(null)
  const fade = usePreset('footerIn')
  useConfirmKeys(open, () => setOpen(false), { cancel, container, trigger })
  if (!open) {
    return (
      <Button
        ref={trigger}
        size="sm"
        variant="ghost"
        icon={icon}
        onClick={() => setOpen(true)}
        data-testid={testId}
      >
        {action}
      </Button>
    )
  }
  return (
    <m.span
      ref={container}
      className="st-header-confirm"
      role="alert"
      data-testid={testId ? `${testId}-confirm` : undefined}
      {...fade}
    >
      <span className="st-header-confirm-text">{confirm}</span>
      <Button ref={cancel} size="sm" variant="ghost" onClick={() => setOpen(false)}>
        {t('common.action.cancel')}
      </Button>
      <Button
        size="sm"
        variant="primary"
        onClick={() => {
          setOpen(false)
          onConfirm()
        }}
        data-testid={testId ? `${testId}-ok` : undefined}
      >
        {confirmLabel}
      </Button>
    </m.span>
  )
}
