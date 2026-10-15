/**
 * AutoHeight（组件库）：内容**换一种状态**时外框高度有过渡的容器——`swapKey` 变化（读取中 → 列表、空状态 →
 * 列表、占位行 → 真实候选）的那次提交，外框从换之前画在屏幕上的高度用 spring.gentle（与卡片的高度跟随同一条弹簧）
 * 追到新内容的高度；追赶期间是显式高度、裁掉溢出（变高像帘子一样露出，变矮空白随之收起），追到之后回到
 * `height: auto`。减少动效时直接到位。
 *
 * 其余时候（`swapKey` 不变）外框就是 `height: auto`，**与内容逐帧同高**：里面的行自己做展开 / 收起 / 插入 / 删除的
 * 高度动画时，外框不用第二条弹簧去追第一条（那样外框会落后内容 200 多 px——展开时下面的行整行被裁掉、
 * 收起 / 删除后留白 0.7 s，总时长远超动效规范）。挂载时不做动画（第一帧就是内容的高度）。
 *
 * 做法：ResizeObserver（布局之后、绘制之前回调）只记下内层每一帧的高度，不触发 React 渲染；`swapKey` 变化的那次
 * 提交（layout effect，绘制之前）把外框钉在上一帧的高度，用 Motion 的 `animate` 驱动一个 MotionValue 到内层的新高度；
 * 追赶期间内层又变了（新内容自己的进入动画、数据又到了），ResizeObserver 把目标改到新高度（保留速度重定目标）。
 * 空闲时没有计时器、没有渲染，也不裁切（焦点环、阴影照常画出外框）。
 */
import { animate, useMotionValue, type AnimationPlaybackControls } from 'motion/react'
import { useCallback, useEffect, useLayoutEffect, useRef, type ReactNode } from 'react'
import { spring } from '../motion/tokens'
import { useReducedMotionPref } from '../motion/useReducedMotionPref'
import { cx } from './cx'

export interface AutoHeightProps {
  children: ReactNode
  /**
   * 内容的「状态」：变化时外框高度过渡（例如 `loading | failed | empty | list`）；不变时外框同步跟随内容。
   * 省略时永远同步跟随（只是一个带 popLayout 定位上下文的容器）
   */
  swapKey?: string | number
  className?: string
  'data-testid'?: string
}

export function AutoHeight({
  children,
  swapKey,
  className,
  'data-testid': testId,
}: AutoHeightProps) {
  const box = useRef<HTMLDivElement>(null)
  const inner = useRef<HTMLDivElement>(null)
  const reduced = useReducedMotionPref()
  const height = useMotionValue(0)
  /** 内层最近一次排版的高度（= 上一帧画在屏幕上的内容高度）；挂载后第一次回调之前为 null */
  const last = useRef<number | null>(null)
  /** 进行中的过渡（目标高度 + 控制柄）；null = 空闲（height: auto） */
  const run = useRef<{ target: number; controls: AnimationPlaybackControls } | null>(null)
  const prevKey = useRef(swapKey)

  /** 结束过渡：回到 height: auto */
  const settle = useCallback(() => {
    run.current?.controls.stop()
    run.current = null
    const el = box.current
    if (!el) return
    el.style.height = ''
    delete el.dataset.animating
  }, [])

  /** 驱动到目标高度（进行中时从当前值、当前速度接着走） */
  const drive = useCallback(
    (target: number) => {
      run.current?.controls.stop()
      const controls = animate(height, target, {
        ...spring.gentle,
        onComplete: () => {
          if (run.current?.controls === controls) settle()
        },
      })
      run.current = { target, controls }
    },
    [height, settle],
  )

  // 过渡期间把 MotionValue 写到外框（直接写样式，不经 React）
  useEffect(
    () =>
      height.on('change', (v) => {
        const el = box.current
        if (el && run.current) el.style.height = `${v}px`
      }),
    [height],
  )

  useLayoutEffect(() => {
    const el = inner.current
    if (!el || typeof ResizeObserver === 'undefined') return
    const ro = new ResizeObserver(() => {
      const h = el.offsetHeight
      last.current = h
      // 过渡中内层又变了：改目标（空闲时外框是 auto，本来就同高）
      if (run.current && run.current.target !== h) drive(h)
    })
    ro.observe(el)
    return () => {
      ro.disconnect()
      settle()
    }
  }, [drive, settle])

  // 换状态的那次提交：绘制之前把外框钉在上一帧的高度，再过渡到新内容的高度
  useLayoutEffect(() => {
    if (prevKey.current === swapKey) return
    prevKey.current = swapKey
    const outer = box.current
    const el = inner.current
    if (!outer || !el) return
    const to = el.offsetHeight
    const from = run.current ? height.get() : last.current
    if (reduced || from === null || Math.abs(from - to) < 0.5) {
      if (run.current) settle()
      return
    }
    if (!run.current) {
      height.jump(from)
      outer.style.height = `${from}px`
      outer.dataset.animating = ''
    }
    drive(to)
  }, [swapKey, reduced, height, drive, settle])

  return (
    <div ref={box} className={cx('ui-autoheight', className)} data-testid={testId}>
      <div ref={inner} className="ui-autoheight-inner">
        {children}
      </div>
    </div>
  )
}
