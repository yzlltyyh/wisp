/**
 * 设置 / 引导窗口里「还没写进磁盘的改动」的统一出口。
 *
 * 写入有三种延迟：文本框停顿后提交（useDraft）、合并写入的停顿（store 的 queuePatch）、术语表与风格的串行写入。
 * 关窗口时 WebView 直接被销毁，React 的卸载清理不会执行，异步 IPC 也可能发不出去，所以：
 * - 各处把「立即提交」的函数登记到这里（`registerFlusher`），在途的命令登记为 `trackWrite`；
 * - 草稿 Key 不在测试时写进凭据存储（`provider_test / provider_models` 直接带上），关窗前没有
 *   要放弃的东西；
 * - ⌘W / Ctrl+W 与引导「完成」先 `settleWrites()`（立即提交 + 等在途命令返回）再关窗（`closeWindowSafely`），
 *   重启前同样（`relaunchSafely`）；
 * - 窗口失焦、页面隐藏、指针离开窗口（Windows / Linux 去点原生标题栏的关闭按钮之前）时立即提交（`installFlushTriggers`）。
 * 原生关闭按钮（macOS 交通灯）没有前端钩子：Rust 拦下第一次 CloseRequested 并发 `window:close_requested`，
 * 前端收尾之后再自己关窗（store.ts）。
 */
import { api, getTransport } from '../../shared/ipc'
import type { WindowTarget } from '../../shared/ipc/types'

const flushers = new Set<() => void>()
/** 最后执行的一组（store 的合并写入队列：文本草稿提交时可能再排进队列，所以它必须在草稿之后） */
const finalFlushers = new Set<() => void>()
const inflight = new Set<Promise<unknown>>()

/** 关窗前最多等在途写入这么久（不是动效时长）：钥匙串授权框之类的长操作不阻塞关窗 */
export const CLOSE_SETTLE_TIMEOUT_MS = 1500

/**
 * 登记一个「立即提交」函数（只做同步的发起，不等结果）；`final` 的在其他之后执行。返回注销函数
 */
export function registerFlusher(flush: () => void, { final = false } = {}): () => void {
  const set = final ? finalFlushers : flushers
  set.add(flush)
  return () => {
    set.delete(flush)
  }
}

/** 登记一次在途的写入命令；返回同一个 Promise */
export function trackWrite<T>(p: Promise<T>): Promise<T> {
  inflight.add(p)
  const release = () => {
    inflight.delete(p)
  }
  p.then(release, release)
  return p
}

/** 立即提交所有登记的延迟改动 */
export function flushAllWrites(): void {
  for (const f of flushers) f()
  for (const f of finalFlushers) f()
}

/** 立即提交，并等待所有在途写入返回（最多 `timeoutMs`） */
export async function settleWrites(timeoutMs = CLOSE_SETTLE_TIMEOUT_MS): Promise<void> {
  flushAllWrites()
  const deadline = Date.now() + timeoutMs
  // 等待期间可能又有串行写入接着发出（术语表 / 风格的下一份），循环到真正清空
  while (inflight.size > 0) {
    const left = deadline - Date.now()
    if (left <= 0) return
    // oxlint-disable-next-line no-await-in-loop -- 有意串行：每轮等当前这批，再看有没有接着发出的
    await Promise.race([
      Promise.allSettled(inflight),
      new Promise((resolve) => setTimeout(resolve, left)),
    ])
  }
}

/** 当前是否还有在途写入（测试用） */
export function pendingWriteCount(): number {
  return inflight.size
}

/**
 * 离开窗口前的统一收尾：让焦点所在的输入框失焦（触发它的提交）→ 提交所有延迟改动 → 等在途写入返回
 * （最多 CLOSE_SETTLE_TIMEOUT_MS）
 */
export async function prepareToLeave(): Promise<void> {
  const active = document.activeElement
  if (active instanceof HTMLElement && active !== document.body) active.blur()
  await settleWrites()
}

/** 安全关闭窗口（⌘W / Ctrl+W） */
export async function closeWindowSafely(): Promise<void> {
  await prepareToLeave()
  await getTransport().window.close()
}

/**
 * 提交所有写入后重启 Wisp。`resume`：重启后直接打开的窗口与路由（引导的同一步、设置的同一分组；Rust 记进 state.json，
 * 下次启动 5 分钟内取走一次）
 */
export async function relaunchSafely(resume: WindowTarget | null = null): Promise<void> {
  await prepareToLeave()
  await api.appRelaunch(resume)
}

/**
 * 窗口失焦 / 隐藏 / 关闭前、指针离开窗口时立即提交（返回取消函数）。只发起写入，不等待
 */
export function installFlushTriggers(): () => void {
  const flush = () => flushAllWrites()
  const onVisibility = () => {
    if (document.visibilityState === 'hidden') flush()
  }
  // 指针离开整个文档（relatedTarget 为空）：去点 Windows / Linux 原生标题栏的关闭按钮之前一定会经过这里
  const onLeave = (e: MouseEvent) => {
    if (e.relatedTarget === null) flush()
  }
  addEventListener('blur', flush)
  addEventListener('pagehide', flush)
  document.addEventListener('visibilitychange', onVisibility)
  document.documentElement.addEventListener('mouseleave', onLeave)
  return () => {
    removeEventListener('blur', flush)
    removeEventListener('pagehide', flush)
    document.removeEventListener('visibilitychange', onVisibility)
    document.documentElement.removeEventListener('mouseleave', onLeave)
  }
}

/** 测试用：清空登记 */
export function resetWrites(): void {
  inflight.clear()
}
