/**
 * 吞掉一次指针按下连同它的后续（mousedown / pointerup / mouseup / click / contextmenu）：事件不再落到下面的元素上，
 * 默认行为（聚焦、选中文字、开始拖拽）也不发生。菜单用它：
 * - 点外面关闭菜单的那一下只关菜单，不同时触发下面那一行的操作（剪贴板里就是直接粘贴）——与 macOS / Windows 的原生
 *   菜单一致；
 * - 分页菜单悬停换页之后，指针还停在换页处时的按下（本想点「›」、页面先翻了，按下去会落在子页的某一项上）。
 *
 * 必须在 document 的**捕获**阶段调用：早于 React 挂在根节点上的监听，阻止传播之后视图收不到这次按下。
 * 后续事件的监听都是一次性的：click 到达、下一次按下、任何按键、指针取消时撤掉；pointerup 之后再排一个 0 ms 的任务兜底
 * （click 与 mouseup 在同一个任务里派发，不会被它抢先）。不常驻，也不会误吞之后由键盘触发的 click。
 */
export function swallowPress(e: PointerEvent): void {
  e.preventDefault()
  e.stopPropagation()
  const doc = document
  let fallback: ReturnType<typeof setTimeout> | null = null
  const eat = (ev: Event) => {
    ev.preventDefault()
    ev.stopPropagation()
  }
  const onUp = (ev: Event) => {
    eat(ev)
    if (fallback === null) fallback = setTimeout(done, 0)
  }
  const onClick = (ev: Event) => {
    eat(ev)
    done()
  }
  const eaten = ['mousedown', 'mouseup', 'contextmenu'] as const
  function done() {
    if (fallback !== null) clearTimeout(fallback)
    fallback = null
    for (const type of eaten) doc.removeEventListener(type, eat, true)
    doc.removeEventListener('pointerup', onUp, true)
    doc.removeEventListener('click', onClick, true)
    doc.removeEventListener('pointerdown', done, true)
    doc.removeEventListener('pointercancel', done, true)
    doc.removeEventListener('keydown', done, true)
  }
  for (const type of eaten) doc.addEventListener(type, eat, true)
  doc.addEventListener('pointerup', onUp, true)
  doc.addEventListener('click', onClick, true)
  // 下一次按下是新的一次点击（是否吞掉由菜单再决定）：撤掉这一次的监听
  doc.addEventListener('pointerdown', done, true)
  doc.addEventListener('pointercancel', done, true)
  doc.addEventListener('keydown', done, true)
}
