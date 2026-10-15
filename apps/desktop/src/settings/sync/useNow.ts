/**
 * 相对时间（「已同步 · 2 分钟前」「最近同步于 3 天前」）的刷新时钟：只在同步页挂载且窗口可见时每分钟走一次，
 * 页面卸载或窗口隐藏即停——只为界面上的文字不过时，不是后台定时任务；回到可见时立即刷新一次。
 */
import { useEffect, useState } from 'react'

/** 刷新间隔：相对时间的最小单位是分钟 */
const CLOCK_MS = 60_000

export function useNow(): number {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    let timer: ReturnType<typeof setInterval> | null = null
    const stop = () => {
      if (timer !== null) clearInterval(timer)
      timer = null
    }
    const start = () => {
      if (timer !== null || document.visibilityState === 'hidden') return
      setNow(Date.now())
      timer = setInterval(() => setNow(Date.now()), CLOCK_MS)
    }
    const onVisibility = () => (document.visibilityState === 'hidden' ? stop() : start())
    start()
    document.addEventListener('visibilitychange', onVisibility)
    return () => {
      stop()
      document.removeEventListener('visibilitychange', onVisibility)
    }
  }, [])
  return now
}
