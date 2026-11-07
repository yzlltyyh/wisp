/**
 * 「展开阅读」的前端规则。面板里展开后的宽度、上限与版式由 Rust 算（`windows/placement.rs` 的 `reading_plan`，经
 * `panel_set_reading` 返回）；这里只放视图排版要用的比例，以及固定窗口按自己的宽度决定版式的那一条——固定窗口可以随意
 * 缩放，宽度够了就自动用阅读版式（翻译开着原文时两栏）。数值与 Rust 一致（测试 `reading_constants_match_frontend`）。
 */
import type { PhysSize, ReadingLayout } from '../shared/ipc/types'

/**
 * 展开 / 收起阅读的快捷键（翻译 / OCR 视图，面板宿主）：⌘⇧E / Ctrl+Shift+E。不用 ⌘E（剪贴板里是「提取」），面板里
 * 其余组合都已占用；Ctrl+Shift+F（微软拼音的简繁切换）、Ctrl+Shift+空格这类中文输入法占用的组合都避开了。较早的 IBus
 * （例如 1.5.17）把 Ctrl+Shift+E 用作 emoji 输入，按键可能到不了 WebView——头部的展开按钮始终可用
 */
export const READING_SHORTCUT = 'Mod+Shift+KeyE'

export const READING = {
  /** 阅读版式要求的最小卡片宽度（翻译两栏每栏约 300 以上，与 Rust 的 READING_TRANSLATE_MIN_WORK 同值） */
  minWidth: 680,
  /**
   * OCR 左右并排要求的最小卡片宽度：面板里工作区至少 800 宽才并排，那时卡片宽 800 − 两侧各 8 = 784；固定窗口按同一个
   * 宽度判断，再窄截图栏太窄，改为上下堆叠
   */
  ocrColumnsWidth: 784,
  /** 截图逻辑宽高比达到它算宽截图：上下堆叠、截图 1:1 显示在上方，识别文本逐行对照；更高的截图左右并排 */
  wideAspect: 2,
  /** 上下堆叠时截图区最多占视图高度的比例（截图更高时在框内滚动，其余高度留给识别文本） */
  shotShare: 0.45,
} as const

/** 宽截图（尺寸未知时按高截图处理，与 Rust 一致） */
export function isWideShot(size: PhysSize | null | undefined): boolean {
  return (
    !!size && size.width > 0 && size.height > 0 && size.width >= size.height * READING.wideAspect
  )
}

/**
 * 固定窗口的阅读版式：卡片宽度至少 `READING.minWidth` 时启用，否则 null（普通布局）。翻译只在开着原文时两栏（没有原文
 * 就没有可以并排的东西）；OCR 高截图在卡片够宽（`READING.ocrColumnsWidth`）时左右并排，宽截图或不够宽时上下堆叠
 */
export function pinnedReading(
  kind: 'translate' | 'ocr',
  cardWidth: number,
  options: { showSource?: boolean; shot?: PhysSize | null } = {},
): ReadingLayout | null {
  if (!(cardWidth >= READING.minWidth)) return null
  if (kind === 'translate') return options.showSource ? 'columns' : null
  return isWideShot(options.shot) || cardWidth < READING.ocrColumnsWidth ? 'stacked' : 'columns'
}
