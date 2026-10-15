/**
 * 引导各步共用的版式：步骤标题 + 说明、可滚动的正文、底栏。
 *
 * - 底栏不在步骤的动画容器里：OnboardingApp 渲染一个固定的 `<footer>`，各步经 portal 把自己的按钮放进去
 *   （`StepLayout` 的 `footer`）。换步骤时旧按钮组原地淡出、新按钮组与正文同时淡入（presets.stepChrome，按 `animate`
 *   决定：窗口刚打开的第一步不动）；同一步里主按钮的切换（「跳过这一步」⇄「下一步」/「保存并继续」）用 `FooterSwap` 交叉淡入，宽度按较宽的
 *   那个占位，不跳；
 * - 主按钮标记 `data-default`：焦点不在任何控件上时按 Return 触发它（macOS 引导的默认按钮，OnboardingApp 注册按键）；
 * - 标题带 `data-step-title`（tabIndex=-1）：换步骤 / 换阶段后焦点交给它（`focusStepTitle`），读屏播报新的一步，
 *   下一次 Tab 从这一步的内容开始；已经有输入框自动获得焦点时不动；
 * - 正文滚动时上下边缘渐隐（滚过的内容不会在步骤条下面被硬切）。
 */
import { m, useIsPresent } from 'motion/react'
import {
  createContext,
  useContext,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
  type UIEvent,
} from 'react'
import { createPortal } from 'react-dom'
import { keepQuoted } from '../shared/i18n/keepQuoted'
import { useT } from '../shared/i18n/useT'
import { adaptPreset, presets } from '../shared/motion/presets'
import { useReducedMotionPref } from '../shared/motion/useReducedMotionPref'
import { Button, cx } from '../shared/ui'

export type StepId = 'welcome' | 'permissions' | 'model' | 'try'

/** 步骤编号（`WindowTarget.onboarding.step`、`boot.route`、`window:navigate`、重启续接的 `app_relaunch(resume)`） */
export const STEP_NUMBERS: Record<StepId, number> = { welcome: 0, permissions: 1, model: 2, try: 3 }

export interface StepProps {
  onNext: () => void
  onBack: () => void
  /** 跳过整个引导（同样记为已完成，下次启动不再自动打开） */
  onSkipAll: () => void
  /** 完成引导 */
  onFinish: () => void
  /** 跳到某一步（「试一试」里没有模型 / 权限时的「去接入」） */
  onGoTo: (step: StepId) => void
  /** 前面还有可返回的步骤 */
  canBack: boolean
}

/** 窗口底栏的挂载点（OnboardingApp 提供）；`animate`：换过步骤之后，底栏的按钮组随正文淡入 */
export const FooterSlot = createContext<{ el: HTMLElement | null; animate: boolean }>({
  el: null,
  animate: false,
})

/** 底栏按钮组的进出（presets.stepChrome：与新步骤同时淡入，旧的原地淡出；减少动效时 ≤ 100 ms 纯淡入淡出） */
function useFooterMotion(animate: boolean) {
  const reduced = useReducedMotionPref()
  const p = adaptPreset(presets.stepChrome, 'fade', reduced)
  return { initial: animate ? p.initial : false, animate: p.animate, exit: p.exit }
}

/**
 * 把底栏内容放进窗口固定的底栏。换步骤时旧步骤（AnimatePresence 里还挂着）的按钮组不立即消失：脱离排版叠在原位淡出
 * （data-leaving：绝对定位、不可点、读屏跳过），新步骤的按钮组同时淡入——主按钮不会一帧硬切成另一种样式
 */
export function StepFooterPortal({ children }: { children: ReactNode }) {
  const slot = useContext(FooterSlot)
  const motion = useFooterMotion(slot.animate)
  const present = useIsPresent()
  return slot.el
    ? createPortal(
        <m.div
          className="ob-foot-inner"
          data-leaving={present ? undefined : ''}
          inert={!present}
          aria-hidden={present ? undefined : true}
          {...motion}
        >
          {children}
        </m.div>,
        slot.el,
      )
    : null
}

/** 换步骤 / 换阶段后把焦点交给标题（已经有输入框获得焦点时不动） */
export function focusStepTitle(root: HTMLElement | null): void {
  if (!root) return
  const active = document.activeElement
  if (
    active instanceof HTMLElement &&
    root.contains(active) &&
    active.matches('input, textarea, select, [contenteditable="true"]')
  )
    return
  root.querySelector<HTMLElement>('[data-step-title]')?.focus({ preventScroll: true })
}

/** 正文滚动的边缘状态：还能往上 / 往下滚时对应边缘渐隐 */
function edgeOf(el: HTMLElement): string {
  const top = el.scrollTop > 1
  const bottom = el.scrollTop + el.clientHeight < el.scrollHeight - 1
  return top && bottom ? 'both' : top ? 'top' : bottom ? 'bottom' : 'none'
}

export function StepLayout({
  title,
  description,
  children,
  footer,
  'data-testid': testId,
}: {
  /** 一级标题；为空时由正文自己提供标题（例如接入模型的表单：服务头部就是这一屏的标题） */
  title?: ReactNode
  description?: ReactNode
  children: ReactNode
  footer: ReactNode
  'data-testid'?: string
}) {
  const body = useRef<HTMLDivElement>(null)
  const inner = useRef<HTMLDivElement>(null)
  const [edge, setEdge] = useState('none')
  // 内容高度变化（展开默认模型、测试结果出现）时也更新边缘状态
  useEffect(() => {
    const el = body.current
    const content = inner.current
    if (!el || !content || typeof ResizeObserver === 'undefined') return
    const ro = new ResizeObserver(() => setEdge(edgeOf(el)))
    ro.observe(content)
    ro.observe(el)
    return () => ro.disconnect()
  }, [])
  const onScroll = (e: UIEvent<HTMLDivElement>) => {
    const next = edgeOf(e.currentTarget)
    if (next !== edge) setEdge(next)
  }
  return (
    <section className="ob-step" data-testid={testId}>
      <div ref={body} className="ob-step-body" data-edge={edge} onScroll={onScroll}>
        <div ref={inner} className="ob-step-inner">
          {title ? (
            <h1 className="ob-title" tabIndex={-1} data-step-title>
              {title}
            </h1>
          ) : null}
          {description ? <p className="ob-desc">{keepQuoted(description)}</p> : null}
          {children}
        </div>
      </div>
      <StepFooterPortal>{footer}</StepFooterPortal>
    </section>
  )
}

/**
 * 底栏：左边「稍后设置」（轻量的链接样式，和右边的「跳过这一步」拉开区别），右边「上一步」与这一步的按钮（`next`）
 */
export function StepFooter({
  onSkipAll,
  onBack,
  canBack,
  next,
}: {
  onSkipAll?: () => void
  onBack?: () => void
  canBack: boolean
  /** 右侧的主按钮（及其左边的次要按钮） */
  next: ReactNode
}) {
  const t = useT()
  return (
    <>
      {onSkipAll ? <LaterButton onClick={onSkipAll} /> : null}
      <span className="ob-spacer" />
      {canBack && onBack ? (
        <Button variant="ghost" onClick={onBack} data-testid="onboarding-back">
          {t('common.action.back')}
        </Button>
      ) : null}
      {next}
    </>
  )
}

/** 「稍后设置」：跳过整个引导（下次启动不再自动打开，可以从托盘重新进入） */
export function LaterButton({ onClick }: { onClick: () => void }) {
  const t = useT()
  return (
    <button type="button" className="ob-later" onClick={onClick} data-testid="onboarding-skip-all">
      {t('onboarding.skip_all')}
    </button>
  )
}

/** 「跳过这一步」：权限、接入模型两步统一的措辞与样式 */
export function SkipStep({ onClick }: { onClick: () => void }) {
  const t = useT()
  return (
    <Button variant="secondary" size="md" onClick={onClick} data-testid="onboarding-next">
      {t('onboarding.skip_step')}
    </Button>
  )
}

/**
 * 同一个位置上两个按钮的交叉淡入（「跳过这一步」⇄「下一步」）：两个都渲染、叠在同一格里，宽度按较宽的那个；
 * 不显示的那个 `inert`（不能点、不能 Tab 到、读屏跳过）。切换时焦点原来在旧按钮上，就交给新按钮
 */
export function FooterSwap({ active, a, b }: { active: 'a' | 'b'; a: ReactNode; b: ReactNode }) {
  const host = useRef<HTMLSpanElement>(null)
  const prev = useRef(active)
  useLayoutEffect(() => {
    if (prev.current === active) return
    const was = host.current?.querySelector<HTMLElement>(`[data-slot="${prev.current}"]`)
    prev.current = active
    if (was?.contains(document.activeElement)) {
      host.current
        ?.querySelector<HTMLElement>(`[data-slot="${active}"] button`)
        ?.focus({ preventScroll: true })
    }
  }, [active])
  return (
    <span ref={host} className="ob-swap" data-active={active}>
      <span className={cx('ob-swap-slot')} data-slot="a" inert={active !== 'a'}>
        {a}
      </span>
      <span className={cx('ob-swap-slot')} data-slot="b" inert={active !== 'b'}>
        {b}
      </span>
    </span>
  )
}
