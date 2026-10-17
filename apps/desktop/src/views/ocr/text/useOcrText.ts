/**
 * OCR 结果文字的状态（三层模型见 model.ts）：向 Rust 要排版结果、切换排版、编辑与放弃修改、固定窗口的交接与还原。
 *
 * - 排版结果经 `text_transform` 求得，同一次结果、同一种排版只请求一次；等结果期间继续显示切换前的文字，不出现
 *   「处理中」；迟到的结果按 key 丢弃（重新识别之后不会被上一次的结果覆盖）；
 * - 用户切换之后的「剪贴板跟随」由视图给出（`follow`，返回是否已复制）：按切换的先后串行执行，只执行最近一次——
 *   连按 ⌘L 时，剪贴板最后留下的一定是眼前这一版；排队等着的跟随执行前后都再核对一次（见 `choose`）；
 * - 适用性不在前端判断：识别文本的可用变换来自芯片条已经在做的检视（`inspected`）。
 */
import { useEffect, useLayoutEffect, useReducer, useRef } from 'react'
import {
  api,
  layoutAvailableIn,
  type OcrLayers,
  type OcrLayout,
  type OcrMode,
  type TransformId,
} from '../../../shared/ipc'
import type { RunView } from '../pinState'
import { isSettled, warnCode } from '../useRun'
import {
  canApply,
  initialText,
  reduceText,
  selectedIn,
  viewOf,
  type TextInput,
  type TextView,
  type Tidy,
} from './model'

export interface OcrTextOptions {
  /** 会话排版的初值（载荷里的设置默认值；固定窗口为交接时的排版） */
  initial: OcrLayout
  /** 当前模式下排版是否作用（逐行 / 段落、非直接翻译） */
  enabled: boolean
  /** 当前识别模式：会话排版在这个模式下不可用时显示识别文本（与编排器交付一致），左半按钮换一种可用的整理方式 */
  mode: OcrMode
  /**
   * 用户切换排版、新的文字就绪之后：让剪贴板跟上，返回是否已复制。`still` 报告这次切换是否仍然作数（写入期间用户编辑、
   * 重新识别或又切换了就不再作数，不提示「已复制」）
   */
  follow(text: string, still: () => boolean): Promise<boolean>
}

export interface OcrText extends TextView {
  /** 会话排版（结构化等不作用、在当前模式下不可用时也保留） */
  session: OcrLayout
  /**
   * 按钮的按下态与菜单的勾选：会话排版在当前模式下可用就是它，否则是原样（显示的也是识别文本）。重新识别期间也按它，
   * 不闪成原样
   */
  selected: OcrLayout
  /** 左半按钮的切换目标：最近用过的整理方式；它在当前模式下不可用时（段落模式里的合并段内换行）换成去掉空行 */
  recent: Tidy
  /** 排版此刻能不能切换：识别已结束、排版作用、没有编辑 */
  switchable: boolean
  can(layout: OcrLayout): boolean
  choose(layout: OcrLayout): void
  /** 左半按钮：在「原样」与最近用过的整理方式之间切换 */
  toggle(): void
  setEdit(text: string): void
  discard(): void
  /** 芯片条的检视结果（只记识别文本的那一次） */
  inspected(text: string, transforms: readonly TransformId[]): void
  /** 从固定交接还原（`key` 为 Run.seed 返回的那次结果；`laid` 见 PinRestore.text） */
  restore(
    key: number,
    layers: { layout: OcrLayout; edit: string | null; laid: string | null },
  ): void
  /** 交给固定窗口的三层 */
  layers(): OcrLayers
}

export function useOcrText(run: RunView, opts: OcrTextOptions): OcrText {
  const [state, dispatch] = useReducer(reduceText, opts.initial, initialText)
  const { key } = run.state
  const raw = run.text()
  const input: TextInput = {
    key,
    raw,
    applies: opts.enabled && isSettled(run.state) && raw.trim() !== '',
    mode: opts.mode,
  }
  const view = viewOf(state, input)
  const switchable = input.applies && !view.edited
  const selected = selectedIn(state.layout, opts.mode)
  const recent: Tidy = layoutAvailableIn(state.recent, opts.mode)
    ? state.recent
    : 'remove_blank_lines'

  const now = { ...opts, key, switchable }
  const latest = useRef(now)
  useLayoutEffect(() => {
    latest.current = now
  })
  const inflight = useRef(new Map<string, Promise<string>>())
  /** 切换的序号：只有最近一次切换让剪贴板跟随 */
  const turn = useRef(0)
  const queue = useRef<Promise<unknown>>(Promise.resolve())

  const laid = (k: number, layout: Tidy, text: string): Promise<string> => {
    const id = `${k}:${layout}`
    let p = inflight.current.get(id)
    if (!p) {
      p = api
        .textTransform(text, layout)
        .catch((error: unknown) => {
          // 求不出来（例如超过 IPC 的长度上限）：按不适用处理，显示识别文本
          warnCode('text_transform')(error)
          return text
        })
        .then((out) => {
          inflight.current.delete(id)
          dispatch({ type: 'laid', key: k, layout, text: out })
          return out
        })
      inflight.current.set(id, p)
    }
    return p
  }

  // 识别完成（或切换到还没求过的排版）时求排版结果
  useEffect(() => {
    if (!input.applies || !view.pending || view.layout === 'keep') return
    void laid(key, view.layout, raw)
    // oxlint-disable-next-line react/exhaustive-deps -- 只在结果、排版或作用范围变化时请求
  }, [key, view.layout, view.pending, input.applies])

  const choose = (next: OcrLayout) => {
    // 与眼前选中的比（不是会话排版）：会话排版在这个模式下不可用时眼前是原样，选原样什么也不变
    const usable = layoutAvailableIn(next, opts.mode) && canApply(state, key, next)
    if (!switchable || next === selected || !usable) return
    dispatch({ type: 'layout', layout: next, input })
    const mine = ++turn.current
    const k = key
    const ready =
      next === 'keep' ? raw : (state.key === k && state.laid[next]) || laid(k, next, raw)
    // 跟随排在队列里、等着排版结果，可能晚于用户的下一步操作：执行前后都按最新的状态核对——仍是这一次结果、仍能切换
    // （识别已结束、没有编辑），也没有更新的切换；否则跳过，不写剪贴板，也不播报
    const still = () =>
      mine === turn.current && latest.current.key === k && latest.current.switchable
    queue.current = queue.current
      .then(async () => {
        const text = await ready
        if (!still()) return
        const copied = await latest.current.follow(text, still)
        if (still()) dispatch({ type: 'notice', key: k, layout: next, copied })
      })
      .catch(() => {})
  }

  return {
    ...view,
    session: state.layout,
    selected,
    recent,
    switchable,
    can: (layout) => canApply(state, key, layout),
    choose,
    toggle: () => choose(selected === 'keep' ? recent : 'keep'),
    setEdit: (text) => dispatch({ type: 'edit', key, text }),
    discard: () => dispatch({ type: 'discard', key }),
    inspected: (text, transforms) => {
      if (text === raw) dispatch({ type: 'inspected', key, transforms })
    },
    restore: (k, layers) => dispatch({ type: 'restore', key: k, ...layers }),
    layers: () => ({ raw, layout: state.layout, edit: view.edited ? view.edit : null }),
  }
}
