/**
 * 展开时跟随滚动：高度在展开的区块（Disclosure 正文、引导的默认模型卡片、测试报告）出现在滚动区下方时，让滚动和展开
 * **同时**进行——每一帧把滚动区往下推，刚好露出区块正在长高的下沿，而不是等弹簧结束后再开始一段平滑滚动
 * （那样大半个展开过程都在视野外，看起来像没反应）。
 *
 * 用法：在 `m.div` 的 `onUpdate` 里调用 `revealGrowing(el, latest.height, anchor)`；`anchor`（例如 Disclosure 的标题行）
 * 不会被推出顶部——区块比视口还高时，停在 anchor 贴着顶部的位置。只读写一次布局，不触发额外的重渲染。
 */

/** 区块下沿与视口下沿之间留出的距离 */
const BOTTOM_GAP_PX = 16

/** 设置页 / 引导页的滚动容器 */
export function scrollParent(el: Element | null): HTMLElement | null {
  return el?.closest<HTMLElement>('.st-page-scroll, .ob-step-body') ?? null
}

/** 顶部被标题条带 / 步骤条遮住的高度（滚动容器的 scroll-padding-top） */
function topInset(scroller: HTMLElement): number {
  const v = Number.parseFloat(getComputedStyle(scroller).scrollPaddingTop)
  return Number.isFinite(v) ? v : 0
}

/**
 * 区块（`el`，高度正在动画）当前帧的高度是 `height`：需要时把滚动容器往下推，露出它的下沿
 */
export function revealGrowing(
  el: HTMLElement | null,
  height: unknown,
  anchor: HTMLElement | null = null,
): void {
  if (!el || typeof height !== 'number') return
  const scroller = scrollParent(el)
  if (!scroller) return
  const box = scroller.getBoundingClientRect()
  // 区块顶部不随自己长高而移动：下沿 = 顶部 + 这一帧的高度（不用等这一帧的样式写进 DOM）
  const bottom = el.getBoundingClientRect().top + height
  let delta = bottom + BOTTOM_GAP_PX - box.bottom
  if (delta <= 0) return
  const top = (anchor ?? el).getBoundingClientRect().top
  delta = Math.min(delta, top - (box.top + topInset(scroller)))
  if (delta > 0) scroller.scrollTop += delta
}
