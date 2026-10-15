/**
 * 开发者模式的延迟埋点条：订阅 `panel:latency`，显示最近一次唤出的
 * 取词 / 就绪 / 首帧 / 首 token 耗时，就绪超时标红。与 FrameMonitor 一起贴在卡片右下角，不参与布局、不拦截指针。
 *
 * 埋点事件在进入动画的第一帧之后才到达，而浮层只在面板可见时挂载：订阅放在模块级（外壳挂载时无条件启动一次，
 * 不看当时的 boot.dev——开发者模式可以在运行时打开），浮层挂载时直接读最近一次的值，不会错过。
 */
import { useSyncExternalStore } from 'react'
import { on } from '../../shared/ipc'
import type { LatencyMarks } from '../../shared/ipc/types'
import { useT } from '../../shared/i18n/useT'

let latest: LatencyMarks | null = null
const listeners = new Set<() => void>()
let started = false

/** 开始订阅 panel:latency（幂等；外壳挂载时无条件调用） */
export function startLatencyFeed(): void {
  if (started) return
  started = true
  void on('panel:latency', (marks) => {
    latest = marks
    for (const l of listeners) l()
  })
}

function subscribe(l: () => void): () => void {
  listeners.add(l)
  return () => listeners.delete(l)
}

const snapshot = () => latest

export function DevLatency() {
  const t = useT()
  const marks = useSyncExternalStore(subscribe, snapshot, snapshot)
  if (!marks) return null
  return (
    <span
      className="dev-latency"
      data-testid="dev-latency"
      data-timeout={marks.ready_timeout || undefined}
    >
      {marks.selection_ms !== null ? (
        <span>
          {t('panel.dev.selection')} {marks.selection_ms}
        </span>
      ) : null}
      <span>
        {t('panel.dev.ready')} {marks.ready_ms}
      </span>
      <span>
        {t('panel.dev.shown')} {marks.shown_ms}
      </span>
      {marks.first_token_ms !== null ? (
        <span>
          {t('panel.dev.first_token')} {marks.first_token_ms}
        </span>
      ) : null}
      {marks.ready_timeout ? <span>{t('panel.dev.timeout')}</span> : null}
    </span>
  )
}
