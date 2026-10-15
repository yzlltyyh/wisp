/**
 * 视图的 JIT 预热：面板窗口常驻、启动时（以及 45 s 回收重建后）隐藏创建。第一次执行某个视图的
 * 代码路径（解析 / 编译组件与它用到的子组件、第一次匹配样式）要十几毫秒，会落在唤出时 prepare 的 40 ms 预算里——
 * 回收让日常的大多数唤出都落在新页面上，输入 / 剪贴板视图冷启动时 ready 只剩几毫秒余量。
 *
 * 做法：外壳挂载、窗口标记为隐藏（`<html data-hidden>`）之后，把视图在不可见、不参与布局的暂存节点里同步渲染一次、
 * 排一次版、立即卸载。预热渲染里视图读 `usePrewarm()` 为 true：不发 IPC、不订阅推送；视图不活动（ViewActivityContext
 * 为 INACTIVE），键盘作用域、自动聚焦、计时器都不启动。每个视图一个任务、整个页面只做一次；真实的视图已经挂载过
 * （那次挂载已经付掉了冷启动）时跳过。
 *
 * **时机**：三个视图排成一个按优先级的队列（输入 → 翻译 → 剪贴板：要焦点、对
 * ready 余量最敏感的先做），隐藏之后**紧接着**逐个执行，每个一个宏任务（MessageChannel，不是计时器——隐藏页面里
 * WebKit 会把计时器节流到秒级，空闲回调 / setTimeout 链在回收后 1.5 s 内常常还没轮到输入视图）。页面加载后
 * 一两百毫秒内全部做完，远在用户第一次唤出之前；单个任务 ≤ 几十毫秒，期间到达的 prepare 只多等这一个任务。
 * 每一步之前确认窗口仍隐藏着：队列没做完时面板又显示了，就停下、等下一次隐藏再接着做（不让剩下的预热落进进入动画）。
 */
import { createContext, useContext, type ReactNode } from 'react'
import { flushSync } from 'react-dom'
import { createRoot } from 'react-dom/client'
import { ViewActivityContext, type ViewActivity } from '../hooks/useViewActive'
import { boot, windowKindOf } from '../ipc/boot'

/** 预热渲染中（视图据此跳过 IPC 与订阅） */
export const PrewarmContext = createContext(false)

/** 当前是不是 JIT 预热渲染 */
export function usePrewarm(): boolean {
  return useContext(PrewarmContext)
}

/** 预热渲染里视图永远不活动 */
export const INACTIVE: ViewActivity = { get: () => false, subscribe: () => () => undefined }

/** 最近一次预热渲染的结果（单测核对确实渲染了、没有吞掉异常） */
export const lastPrewarm: { nodes: number; error: unknown } = { nodes: 0, error: null }

/**
 * 在不可见的暂存节点里同步渲染一次、排版、卸载，不留节点；任何异常都吞掉（预热失败只是少了一次优化）
 */
export function prewarmRender(node: ReactNode, size: { width: number; height: number }): void {
  lastPrewarm.nodes = 0
  lastPrewarm.error = null
  if (typeof document === 'undefined' || !document.body) return
  const stage = document.createElement('div')
  stage.setAttribute('aria-hidden', 'true')
  stage.inert = true
  stage.style.cssText = `position:fixed;left:-10000px;top:0;width:${size.width}px;height:${size.height}px;visibility:hidden;pointer-events:none;contain:strict`
  document.body.append(stage)
  const root = createRoot(stage)
  try {
    flushSync(() =>
      root.render(
        <PrewarmContext value={true}>
          <ViewActivityContext value={INACTIVE}>{node}</ViewActivityContext>
        </PrewarmContext>,
      ),
    )
    void stage.offsetHeight
    lastPrewarm.nodes = stage.getElementsByTagName('*').length
  } catch (error) {
    // 忽略：只是少了一次预热
    lastPrewarm.error = error
  } finally {
    root.unmount()
    stage.remove()
  }
}

/** 预热的先后（小的先做）：输入、翻译、剪贴板 */
export const PREWARM_ORDER = { input: 0, translate: 1, clipboard: 2 } as const

const queue: Array<{ order: number; run: () => void }> = []
let armed = false
let draining = false
/** 等窗口重新隐藏的观察者（只在队列没做完、窗口又显示了时存在） */
let observer: MutationObserver | null = null

function hidden(): boolean {
  return document.documentElement.hasAttribute('data-hidden')
}

/** 下一个宏任务（MessageChannel；没有时退回 setTimeout 0） */
function nextTask(fn: () => void): void {
  if (typeof MessageChannel === 'function') {
    const ch = new MessageChannel()
    ch.port1.onmessage = () => {
      ch.port1.close()
      fn()
    }
    ch.port2.postMessage(null)
  } else setTimeout(fn, 0)
}

/** 窗口隐藏着就开始逐个执行；显示着就等它再隐藏（同一时刻至多一个观察者） */
function whenHidden(): void {
  if (hidden()) {
    drain()
    return
  }
  if (observer) return
  const html = document.documentElement
  const watch = new MutationObserver(() => {
    if (!hidden()) return
    watch.disconnect()
    if (observer === watch) observer = null
    drain()
  })
  observer = watch
  watch.observe(html, { attributes: true, attributeFilter: ['data-hidden'] })
}

/**
 * 逐个执行，每个一个宏任务。每一步之前都确认窗口仍是隐藏的：用户在队列做完之前又唤出了面板（例如空闲卸载后冷唤出、
 * 第一次隐藏后 200 ms 内又按了快捷键），剩下的预热**不能**落进进入动画里——停下，等下一次隐藏再接着做
 */
function drain(): void {
  if (draining) return
  draining = true
  const step = () => {
    if (queue.length === 0) {
      draining = false
      return
    }
    if (!hidden()) {
      draining = false
      whenHidden()
      return
    }
    const item = queue.shift()
    try {
      item?.run()
    } finally {
      nextTask(step)
    }
  }
  nextTask(step)
}

/**
 * 登记一个预热（只在面板窗口里生效）：外壳把窗口标记为隐藏之后按 `order` 逐个执行（见文件头与 `drain`）。等外壳是因为
 * 预热渲染会订阅外观等事件：外壳挂载之后这些订阅已经存在，预热不会提前触发传输层的初始化
 */
export function schedulePrewarm(run: () => void, order: number): void {
  if (typeof window === 'undefined' || typeof MutationObserver === 'undefined') return
  if (windowKindOf(boot.label) !== 'panel') return
  const at = queue.findIndex((q) => q.order > order)
  queue.splice(at < 0 ? queue.length : at, 0, { order, run })
  if (armed) {
    // 已经开始过：窗口隐藏着就接着做，显示着就等下一次隐藏（原来这里显示着时什么也不做，这一项永远不会执行）
    whenHidden()
    return
  }
  armed = true
  // 同一轮模块初始化里登记的视图都排进队列之后再开始（微任务），保证按优先级而不是按导入顺序
  queueMicrotask(whenHidden)
}

/** 单测用：清空队列与状态 */
export function resetPrewarmForTest(): void {
  queue.length = 0
  armed = false
  draining = false
  observer?.disconnect()
  observer = null
}
