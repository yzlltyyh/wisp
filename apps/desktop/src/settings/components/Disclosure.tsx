/**
 * 可折叠的区块（高级选项、已知限制、许可分组）：标题行是按钮，展开用 presets.sectionExpand（高度 spring.gentle +
 * 内容交叉淡入；减少动效时直接到位）。折叠时内容不挂载（长列表不占 DOM）。
 *
 * - 在页面底部点开时，展开与滚动同时进行（lib/reveal.ts：每一帧露出正在长高的下沿，标题行不会被推出顶部），
 *   而不是等弹簧结束再滚——那样展开过程几乎全在视野外，看起来像没反应；减少动效时直接到位后滚入视野；
 * - 收起时高度与淡出同时在 duration.fast 内结束（presets.collapse：不留一段「内容已经淡没、高度还在慢慢收」的空白）。
 */
import { ChevronRight } from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useId, useRef, useState, type ReactNode } from 'react'
import { usePreset } from '../../shared/motion/presets'
import { useReducedMotionPref } from '../../shared/motion/useReducedMotionPref'
import { cx, Icon } from '../../shared/ui'
import { revealGrowing } from '../lib/reveal'

export interface DisclosureProps {
  title: ReactNode
  /** 标题右侧的附加信息（例如条目数） */
  meta?: ReactNode
  children: ReactNode
  defaultOpen?: boolean
  /** 受控 */
  open?: boolean
  onOpenChange?: (open: boolean) => void
  /** inline：嵌在分组卡片里的一行；plain：独立于卡片 */
  variant?: 'row' | 'plain'
  className?: string
  'data-testid'?: string
}

export function Disclosure({
  title,
  meta,
  children,
  defaultOpen = false,
  open: controlled,
  onOpenChange,
  variant = 'row',
  className,
  'data-testid': testId,
}: DisclosureProps) {
  const [inner, setInner] = useState(defaultOpen)
  const open = controlled ?? inner
  const expand = usePreset('sectionExpand')
  const collapse = usePreset('collapse')
  const reduced = useReducedMotionPref()
  const body = useRef<HTMLDivElement>(null)
  const head = useRef<HTMLButtonElement>(null)
  /** 用户点开的（不是初始就展开的）：展开过程中跟随滚动 */
  const opened = useRef(false)
  const id = useId()
  const toggle = () => {
    const next = !open
    opened.current = next
    if (controlled === undefined) setInner(next)
    onOpenChange?.(next)
  }
  return (
    <div
      className={cx('st-disclosure', className)}
      data-variant={variant}
      data-open={open || undefined}
      data-testid={testId}
    >
      <button
        ref={head}
        type="button"
        className="st-disclosure-head"
        aria-expanded={open}
        aria-controls={id}
        onClick={toggle}
        data-testid={testId ? `${testId}-toggle` : undefined}
      >
        <Icon icon={ChevronRight} size={14} className="st-disclosure-chevron" />
        <span className="st-disclosure-title">{title}</span>
        {meta ? <span className="st-disclosure-meta">{meta}</span> : null}
      </button>
      <AnimatePresence initial={false}>
        {open ? (
          <m.div
            key="body"
            ref={body}
            id={id}
            className="st-disclosure-clip"
            {...expand}
            exit={collapse.exit}
            onUpdate={(latest) => {
              if (opened.current) revealGrowing(body.current, latest.height, head.current)
            }}
            onAnimationComplete={() => {
              if (!opened.current) return
              opened.current = false
              // 减少动效时没有逐帧的高度：到位后滚入视野
              if (reduced) body.current?.scrollIntoView?.({ block: 'nearest' })
            }}
          >
            <div className="st-disclosure-body">{children}</div>
          </m.div>
        ) : null}
      </AnimatePresence>
    </div>
  )
}
