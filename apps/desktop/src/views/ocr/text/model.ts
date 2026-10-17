/**
 * OCR 结果文字的三层（纯函数，不碰 React / IPC）：
 * - R：识别文本，即识别请求的流式全文（Run 的 `text()`），以 Run 的 `key` 区分每一次结果；
 * - V：排版结果 = layout(R)，由 Rust 的 `text_transform` 求得（与识别完成时自动复制、写入识别历史的是同一个函数、
 *   同一份输入），按 (key, 排版) 缓存；
 * - E：用户编辑，从 V 开始。
 * 显示 E ?? V；「已编辑」是 E 不为空且与 V 不同。排版只在逐行 / 段落模式、识别已结束时作用（`applies`），而且会话排版
 * 要在当前模式下可用（`layoutAvailableIn`：合并段内换行只在逐行模式），其余时候 V = R——与编排器交付（自动复制、写入
 * 识别历史）的规则相同，例如逐行模式选了合并段内换行、换到段落模式重新识别，显示的与复制的都是识别文本。
 * 会话排版与左半按钮的切换目标跨识别保留，其余字段只属于某一个 key：key 变了（重新识别、从固定交接还原）就
 * 一起作废，不需要在识别开始时逐个清空。
 */
import {
  layoutAvailableIn,
  OCR_LAYOUTS,
  type OcrLayout,
  type OcrMode,
  type TransformId,
} from '../../../shared/ipc'

/** 可以切换到的整理方式（「原样」之外） */
export type Tidy = Exclude<OcrLayout, 'keep'>

export interface OcrTextState {
  /** 下面这些按 key 记录的字段属于哪一次识别结果 */
  key: number
  /** 会话排版（用户选的；结构化与直接翻译下不作用，但保留到下一次逐行 / 段落结果） */
  layout: OcrLayout
  /** 左半按钮在「原样」与它之间切换：本次会话最近用过的整理方式 */
  recent: Tidy
  /** 这次结果已经求得的排版结果 */
  laid: Partial<Record<Tidy, string>>
  edit: string | null
  /** 切换后新结果还没到时继续显示的文字（切换前显示的排版结果）；null = 识别文本 */
  stale: string | null
  /** 用户切换过排版或放弃过修改（正文替换时按比例保持滚动位置；识别完成后第一次排好不算） */
  turned: boolean
  /** 识别文本适用的变换（芯片检视的结果）；null = 还不知道，按都适用处理 */
  applicable: readonly TransformId[] | null
  /** 最近一次切换的结果（读屏播报）：切到哪种排版、剪贴板有没有跟着更新 */
  notice: { layout: OcrLayout; copied: boolean } | null
}

/** 计算显示文字所需的外部输入 */
export interface TextInput {
  /** 当前结果（Run 的 key） */
  key: number
  /** 识别文本 R */
  raw: string
  /** 排版是否作用：逐行 / 段落模式、非直接翻译、识别已结束 */
  applies: boolean
  /** 当前识别模式：会话排版在这个模式下不可用时不作用（显示识别文本） */
  mode: OcrMode
}

export type TextAction =
  | { type: 'layout'; layout: OcrLayout; input: TextInput }
  | { type: 'laid'; key: number; layout: Tidy; text: string }
  | { type: 'edit'; key: number; text: string }
  | { type: 'discard'; key: number }
  | { type: 'inspected'; key: number; transforms: readonly TransformId[] }
  | { type: 'notice'; key: number; layout: OcrLayout; copied: boolean }
  | { type: 'restore'; key: number; layout: OcrLayout; edit: string | null; laid: string | null }

/** 选了某种排版之后左半按钮的切换目标：选的是整理方式就换成它，选原样不变 */
const recentOf = (layout: OcrLayout, prev: Tidy): Tidy => (layout === 'keep' ? prev : layout)

export function initialText(layout: OcrLayout): OcrTextState {
  return {
    key: 0,
    layout,
    recent: recentOf(layout, 'remove_blank_lines'),
    laid: {},
    edit: null,
    stale: null,
    turned: false,
    applicable: null,
    notice: null,
  }
}

/** 换到另一次结果：按 key 记录的字段作废，会话排版与切换目标保留 */
function at(s: OcrTextState, key: number): OcrTextState {
  if (s.key === key) return s
  return { ...initialText(s.layout), recent: s.recent, key }
}

/** 某个模式下实际选中的排版：会话排版在这个模式下可用就是它，否则是原样（按钮的按下态、菜单的勾选与交付都按它） */
export const selectedIn = (layout: OcrLayout, mode: OcrMode): OcrLayout =>
  layoutAvailableIn(layout, mode) ? layout : 'keep'

/**
 * 左半按钮在某个模式下的切换目标：最近用过的整理方式在这个模式下可用就是它（对这段文字不适用时按钮置灰、写明原因）；
 * 不可用时（段落模式里的合并段内换行）按 OCR_LAYOUTS 的顺序换成第一个在这个模式下可用、且对这段文字适用的——固定退回
 * 某一种的话，它恰好不适用时左半与 ⌘L 什么也不做，别的整理方式却明明可用。都不适用时取第一个可用的，按钮照常置灰。
 * 只决定按钮切到哪种，不改写最近用过的（回到逐行模式还是它）
 */
export function recentIn(recent: Tidy, mode: OcrMode, can: (layout: Tidy) => boolean): Tidy {
  if (layoutAvailableIn(recent, mode)) return recent
  const usable = OCR_LAYOUTS.filter((l): l is Tidy => l !== 'keep' && layoutAvailableIn(l, mode))
  return usable.find(can) ?? usable[0] ?? recent
}

export interface TextView {
  /** 实际作用的排版（不作用时为 keep） */
  layout: OcrLayout
  /** 排版后的文字 V（还在等结果时是切换前的文字） */
  base: string
  /** 显示的文字 E ?? V */
  display: string
  edit: string | null
  edited: boolean
  /** 正在等这次排版的结果 */
  pending: boolean
  turned: boolean
  notice: OcrTextState['notice']
}

export function viewOf(s: OcrTextState, input: TextInput): TextView {
  const own = s.key === input.key ? s : at(s, input.key)
  const layout = input.applies ? selectedIn(s.layout, input.mode) : 'keep'
  const done = layout === 'keep' ? input.raw : own.laid[layout]
  const base = done ?? own.stale ?? input.raw
  const edit = own.edit
  return {
    layout,
    base,
    display: edit ?? base,
    edit,
    edited: edit !== null && edit !== base,
    pending: done === undefined,
    turned: own.turned,
    notice: own.notice,
  }
}

/** 某种排版是否适用（「原样」总是适用；检视结果还没到时都当作适用，不先置灰） */
export function canApply(s: OcrTextState, key: number, layout: OcrLayout): boolean {
  if (layout === 'keep' || s.key !== key || s.applicable === null) return true
  return s.applicable.includes(layout)
}

export function reduceText(s: OcrTextState, a: TextAction): OcrTextState {
  switch (a.type) {
    case 'layout': {
      const before = viewOf(s, a.input)
      const own = at(s, a.input.key)
      return {
        ...own,
        layout: a.layout,
        recent: recentOf(a.layout, own.recent),
        stale: before.base,
        turned: true,
        notice: null,
        // 编辑后又改回与排版结果一样：不算编辑，跟着新排版走
        edit: before.edited ? own.edit : null,
      }
    }
    case 'laid': {
      // 更早一次结果迟到的排版：丢弃
      if (a.key < s.key) return s
      const own = at(s, a.key)
      return { ...own, laid: { ...own.laid, [a.layout]: a.text } }
    }
    case 'edit':
      if (a.key < s.key) return s
      return { ...at(s, a.key), edit: a.text }
    case 'discard':
      if (a.key !== s.key) return s
      return { ...s, edit: null, turned: true, notice: null }
    case 'inspected':
      if (a.key < s.key) return s
      return { ...at(s, a.key), applicable: a.transforms }
    case 'notice':
      if (a.key !== s.key) return s
      return { ...s, notice: { layout: a.layout, copied: a.copied } }
    case 'restore': {
      const own = at(s, a.key)
      return {
        ...own,
        layout: a.layout,
        recent: recentOf(a.layout, own.recent),
        edit: a.edit,
        // 交接时已经求得的排版结果：固定窗口首帧直接显示它，不必再等一次 text_transform；没有时（交接时还在等结果）
        // 由 useOcrText 重新求
        laid: a.laid && a.layout !== 'keep' ? { [a.layout]: a.laid } : {},
      }
    }
  }
}
