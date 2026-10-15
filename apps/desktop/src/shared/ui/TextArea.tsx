/**
 * TextArea：自动增高（minRows–maxRows，超出后内部滚动）、IME 安全。
 *
 * - `onSubmit`：⌘↩ / Ctrl+Enter 提交（输入法组合中不触发）；↩ 照常换行（输入模式）；
 * - 增高在布局阶段同步完成（先归零再取 scrollHeight），不会出现先溢出再长高的一帧；文字、行数上限、正文字号
 *   （设置「字号」，appearance:changed）变化时重算；宽度变化（视图切换改宽、固定窗口缩放）会改变折行，由
 *   ResizeObserver 只在**宽度**变化时、合并到一个 rAF 里重算（自己改高度引起的尺寸变化不触发）；
 * - `variant="bare"`：面板正文区的无边框写法（输入视图、OCR 可编辑文本）；
 * - `autoFocus` 走 `useAutoFocus`：所在面板视图还在切换的暂存层里时，推迟到视图显示时再聚焦；
 * - 挂载在视图切换的暂存层里时，挂载的提交里不量高度（不在那个任务里强制排版整个暂存视图）：暂存层排版之后
 *   ResizeObserver 第一次回报时在下一帧量，视图显示的那次提交里再核对一次。
 */
import {
  useCallback,
  useLayoutEffect,
  useRef,
  useSyncExternalStore,
  type KeyboardEvent as ReactKeyboardEvent,
  type Ref,
  type TextareaHTMLAttributes,
} from 'react'
import { getAppearance, subscribeAppearance } from '../hooks/useAppearance'
import { useAutoFocus, useViewActive } from '../hooks/useViewActive'
import { boot } from '../ipc/boot'
import { cx } from './cx'

/** 还没有按宽度量过（空输入框的快速路径）：宽度变化时不需要重算，有了文字之后按正常路径量 */
const UNMEASURED = -1

/** 只订阅正文字号（其他外观变化不重渲染输入框） */
const bodyFontSize = () => getAppearance().fontSize

/**
 * 按内容设置高度：先归零再取 scrollHeight，夹到 [minRows, maxRows] 行；超出上限时内部滚动。
 * 返回计算时的宽度（clientWidth，此时布局刚算过，读取没有额外开销），宽度变化检测以它为基准
 */
function fitHeight(el: HTMLTextAreaElement, minRows: number, maxRows: number): number {
  const style = getComputedStyle(el)
  const line = parseFloat(style.lineHeight) || parseFloat(style.fontSize) * 1.6
  const pad = parseFloat(style.paddingTop) + parseFloat(style.paddingBottom)
  const border = parseFloat(style.borderTopWidth) + parseFloat(style.borderBottomWidth)
  const min = minRows * line + pad + border
  if (el.value === '') {
    // 空输入框：高度就是 minRows 行，不用先归零再读 scrollHeight——少一次强制排版（唤出输入视图时它在 prepare →
    // panel_ready 的同步路径上）；宽度不影响空输入框的高度，不读 clientWidth（UNMEASURED，见 ResizeObserver 回调）
    el.style.height = `${Math.ceil(min)}px`
    el.style.overflowY = 'hidden'
    return UNMEASURED
  }
  el.style.height = '0px'
  const content = el.scrollHeight
  const max = maxRows * line + pad + border
  const h = Math.min(max, Math.max(min, content + border))
  el.style.height = `${Math.ceil(h)}px`
  el.style.overflowY = content + border > max ? 'auto' : 'hidden'
  return el.clientWidth
}

export interface TextAreaProps extends Omit<
  TextareaHTMLAttributes<HTMLTextAreaElement>,
  'onChange' | 'value' | 'onSubmit'
> {
  value: string
  onChange: (value: string) => void
  minRows?: number
  maxRows?: number
  invalid?: boolean
  variant?: 'field' | 'bare'
  /** ⌘↩ / Ctrl+Enter */
  onSubmit?: () => void
  ref?: Ref<HTMLTextAreaElement>
  'data-testid'?: string
  'data-force'?: string
}

export function TextArea({
  value,
  onChange,
  minRows = 3,
  maxRows = 12,
  invalid = false,
  variant = 'field',
  onSubmit,
  onKeyDown,
  className,
  autoFocus,
  ref,
  'data-testid': testId = 'text-area',
  'data-force': force,
  ...rest
}: TextAreaProps) {
  const inner = useRef<HTMLTextAreaElement | null>(null)
  const setRef = useCallback(
    (el: HTMLTextAreaElement | null) => {
      inner.current = el
      if (typeof ref === 'function') ref(el)
      else if (ref) ref.current = el
    },
    [ref],
  )
  const fontSize = useSyncExternalStore(subscribeAppearance, bodyFontSize, bodyFontSize)
  const active = useViewActive()
  useAutoFocus(inner, autoFocus)
  /** 最近一次计算高度时的宽度与行数范围（ResizeObserver 回调读取）；UNMEASURED 表示还没按宽度量过 */
  const fit = useRef({ width: UNMEASURED, minRows, maxRows })
  // 文本（含外部重置）、行数上限、正文字号变化或视图显示后重新计算高度：value / fontSize / active 只作为触发条件
  useLayoutEffect(() => {
    const el = inner.current
    if (!el) return
    if (!active) {
      // 暂存中：只记下行数范围，等 ResizeObserver（暂存层排版之后）或显示时再量
      fit.current = { ...fit.current, minRows, maxRows }
      return
    }
    fit.current = { width: fitHeight(el, minRows, maxRows), minRows, maxRows }
    // oxlint-disable-next-line react/exhaustive-effect-dependencies
  }, [value, minRows, maxRows, fontSize, active])
  // 宽度变化改变折行：只看宽度（fitHeight 自己改高度引起的回调忽略），多次变化合并到一个 rAF。
  // 回调在布局之后执行，读 clientWidth 不会强制布局
  useLayoutEffect(() => {
    const el = inner.current
    if (!el) return
    let frame = 0
    const observer = new ResizeObserver(() => {
      // 空输入框的高度与宽度无关（fitHeight 的快速路径）：不必为宽度变化再量一次
      if (frame || (fit.current.width === UNMEASURED && el.value === '')) return
      if (el.clientWidth === fit.current.width) return
      frame = requestAnimationFrame(() => {
        frame = 0
        const f = fit.current
        fit.current = { ...f, width: fitHeight(el, f.minRows, f.maxRows) }
      })
    })
    observer.observe(el)
    return () => {
      observer.disconnect()
      if (frame) cancelAnimationFrame(frame)
    }
  }, [])
  const onKey = (e: ReactKeyboardEvent<HTMLTextAreaElement>) => {
    onKeyDown?.(e)
    if (e.defaultPrevented || e.nativeEvent.isComposing || e.keyCode === 229) return
    const mod = boot.os === 'macos' ? e.metaKey : e.ctrlKey
    if (e.key === 'Enter' && mod && onSubmit) {
      e.preventDefault()
      onSubmit()
    }
  }
  return (
    <textarea
      {...rest}
      ref={setRef}
      className={cx('ui-textarea', className)}
      data-variant={variant}
      data-force={force}
      aria-invalid={invalid || undefined}
      data-invalid={invalid || undefined}
      data-testid={testId}
      value={value}
      rows={minRows}
      onChange={(e) => onChange(e.target.value)}
      onKeyDown={onKey}
    />
  )
}
