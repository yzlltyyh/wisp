/**
 * 排版切换（以及放弃修改）时正文的替换：新文字从「变淡」回到不透明（presets.crossfade，120 ms），不用风格切换那种
 * 模糊——模糊表示「正在重新生成」，排版只是同一段文字换了换行。文本框只有一个、旧文字不叠放，所以不从 0 开始淡入，
 * 而是与设置分组切换同样从变淡的状态开始，不会有一帧空白。用户操作引起的替换按比例保持滚动位置（不跳回开头）；
 * 识别完成后第一次排好不动滚动位置，交给「流式结束回到开头」。文本框刚出现（流式区换成文本框）时不播。减少动效时
 * 直接替换。高度变化由卡片的高度跟随负责，这里不管。
 */
import { animate } from 'motion/react'
import { useLayoutEffect, useRef, type RefObject } from 'react'
import { presets } from '../../../shared/motion/presets'

/** 返回给文本框 onScroll 用的记录函数（记下滚动到的比例） */
export function useTextSwap(
  ref: RefObject<HTMLTextAreaElement | null>,
  /** 排版后的文字 V */
  base: string,
  /** 有没有编辑（编辑中的逐字变化不算替换；放弃修改算） */
  editing: boolean,
  /** 用户切换过排版或放弃过修改 */
  turned: boolean,
  reduced: boolean,
): (el: HTMLTextAreaElement) => void {
  const ratio = useRef(0)
  const seen = useRef<{ el: HTMLTextAreaElement | null; base: string; editing: boolean } | null>(
    null,
  )
  // 每次提交都核对（只比较几个值）：文本框换了元素、或文字没有被替换时什么也不做
  useLayoutEffect(() => {
    const el = ref.current
    const prev = seen.current
    seen.current = { el, base, editing }
    if (!el || prev?.el !== el || (prev.base === base && !(prev.editing && !editing))) return
    if (turned) el.scrollTop = ratio.current * Math.max(0, el.scrollHeight - el.clientHeight)
    if (!reduced) {
      void animate(el, { opacity: [presets.pageSwitch.initial.opacity, 1] }, presets.crossfade)
    }
  })
  return (el) => {
    const max = el.scrollHeight - el.clientHeight
    ratio.current = max > 0 ? el.scrollTop / max : 0
  }
}
