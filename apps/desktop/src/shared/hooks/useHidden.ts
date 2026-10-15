/**
 * 窗口隐藏状态与隐藏期间的资源释放。
 *
 * - 面板外壳在 `panel_hide` 之后调用 `setWindowHidden(true)`、在下一次 prepare 时调用 `setWindowHidden(false)`；
 *   `<html data-hidden>` 随之切换，CSS 动画（光标、骨架屏）统一暂停；`document.hidden` 同样视为隐藏；
 * - 视图用 `useHidden()` 停掉自己的计时器 / rAF / Observer；
 * - 隐藏超过 2 分钟（`timing.releaseAfterHidden`）：执行 `onLongHidden` 注册的回调、`close()` 所有经 `trackBitmap`
 *   登记的 ImageBitmap。计时器只在隐藏时调度一次，显示时取消，空闲时没有周期唤醒。
 */
import { useSyncExternalStore } from 'react'
import { ms, timing } from '../motion/tokens'

let windowHidden = false
let documentHidden = typeof document !== 'undefined' ? document.hidden : false
const listeners = new Set<() => void>()
const releasers = new Set<() => void>()
const bitmaps = new Set<ImageBitmap>()
let releaseTimer: ReturnType<typeof setTimeout> | null = null

if (typeof document !== 'undefined') {
  document.addEventListener('visibilitychange', () => {
    documentHidden = document.hidden
    emit()
  })
}

function emit(): void {
  for (const l of listeners) l()
}

/** 窗口是否隐藏（面板外壳维护） */
export function setWindowHidden(hidden: boolean): void {
  if (hidden === windowHidden) return
  windowHidden = hidden
  if (typeof document !== 'undefined') {
    if (hidden) document.documentElement.dataset.hidden = ''
    else delete document.documentElement.dataset.hidden
  }
  if (releaseTimer !== null) {
    clearTimeout(releaseTimer)
    releaseTimer = null
  }
  if (hidden) releaseTimer = setTimeout(releaseNow, ms(timing.releaseAfterHidden))
  emit()
}

export function isHidden(): boolean {
  return windowHidden || documentHidden
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

/** 窗口或文档是否隐藏（响应式） */
export function useHidden(): boolean {
  return useSyncExternalStore(subscribe, isHidden, isHidden)
}

/** 注册「长时间隐藏后释放」回调（返回注销函数），例如清空缩略图缓存 */
export function onLongHidden(release: () => void): () => void {
  releasers.add(release)
  return () => releasers.delete(release)
}

/** 登记一个 ImageBitmap：长时间隐藏后统一 close()；用完自行 close 时调用返回的函数注销 */
export function trackBitmap(bitmap: ImageBitmap): () => void {
  bitmaps.add(bitmap)
  return () => bitmaps.delete(bitmap)
}

/** 立即执行释放（计时器到期时调用；测试也可直接调用） */
export function releaseNow(): void {
  releaseTimer = null
  for (const b of bitmaps) b.close()
  bitmaps.clear()
  for (const release of releasers) release()
}

/**
 * 清空容器内 `<img>` 的 src（保存在 data-wisp-src），`restoreImages` 还原。用于隐藏后仍保留 DOM 的场景；
 * 面板视图在隐藏时整体卸载，通常不需要。
 */
export function releaseImages(root: ParentNode): void {
  for (const img of root.querySelectorAll('img[src]')) {
    const src = img.getAttribute('src')
    if (!src) continue
    img.setAttribute('data-wisp-src', src)
    img.setAttribute('src', '')
  }
}

export function restoreImages(root: ParentNode): void {
  for (const img of root.querySelectorAll('img[data-wisp-src]')) {
    img.setAttribute('src', img.getAttribute('data-wisp-src') ?? '')
    img.removeAttribute('data-wisp-src')
  }
}
