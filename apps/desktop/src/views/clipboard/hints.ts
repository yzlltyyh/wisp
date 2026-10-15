/**
 * 底部快捷键提示条的取舍：面板默认 420 宽放不下全部提示时，**按优先级**
 * 整项隐藏低优先级的项，不加宽面板、不出现半截提示。显示顺序与优先级分开：显示顺序是动作的逻辑顺序，优先级决定
 * 窄宽度下留下谁。
 *
 * 必留四项：↩ 粘贴、⇧↩ 纯文本、⌘F 筛选、Esc 关闭；其余按 仅复制 → 删除 → 置顶 → 预览 → 操作 的顺序让位。
 * 严格按优先级：第一项放不下之后，更低优先级的项即使更短也不再显示（窄宽度下留下的永远是「最重要的前 k 项」，
 * 不会出现「操作」在而「仅复制」不在这种跳跃）。
 */

export interface HintSize {
  id: string
  /** 优先级，越小越重要 */
  priority: number
  /** 这一项的自然宽度（px） */
  width: number
}

/**
 * 返回要隐藏的项。`available` 是提示条内容区的宽度，`gap` 是项与项之间的间距。宽度都为 0（jsdom、还没排版）时
 * 什么都不隐藏
 */
export function hiddenHints(
  items: readonly HintSize[],
  available: number,
  gap: number,
): Set<string> {
  const hidden = new Set<string>()
  if (available <= 0 || items.every((it) => it.width <= 0)) return hidden
  const byPriority = [...items].sort((a, b) => a.priority - b.priority)
  let used = 0
  let shown = 0
  let full = false
  for (const it of byPriority) {
    const need = it.width + (shown > 0 ? gap : 0)
    if (!full && used + need <= available + 0.5) {
      used += need
      shown += 1
    } else {
      full = true
      hidden.add(it.id)
    }
  }
  return hidden
}
