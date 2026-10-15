/**
 * 帧监视：开发者模式下在面板内容之后的开发者行里，显示实测刷新率、帧间隔 p50 / p95、掉帧数
 * （> 1.5 × 帧周期）与每帧 JS 耗时；不拦截指针，面板隐藏时随外壳卸载、停止采样。
 */
import { useEffect, useState } from 'react'
import { useT } from '../i18n/useT'
import { framesWarn, startFrameMonitor, type FrameStats } from './frameMonitor'

export function FrameMonitorView({ className }: { className?: string }) {
  const t = useT()
  const [stats, setStats] = useState<FrameStats | null>(null)
  useEffect(() => startFrameMonitor(setStats), [])
  if (!stats) return null
  const warn = framesWarn(stats)
  return (
    <div
      className={className ?? 'dev-frames'}
      data-testid="frame-monitor"
      data-warn={warn || undefined}
    >
      {/* i18n-ignore：开发者面板的单位 */}
      <span>{stats.hz} Hz</span>
      <span>
        {t('panel.dev.frames')} {stats.p50}/{stats.p95} ms
      </span>
      <span>
        {t('panel.dev.dropped')} {stats.dropped}
      </span>
      <span>
        {t('panel.dev.js')} {stats.jsP95} ms
      </span>
    </div>
  )
}
