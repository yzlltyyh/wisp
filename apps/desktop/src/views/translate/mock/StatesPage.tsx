/**
 * 翻译视图的状态总览页（只在浏览器 mock 中加载：mock 场景 `translate.states` / `translate.errors` 动态导入）。
 * 每个状态一张静态面板卡片（真实的 TranslateView，流由场景预置），便于一次审阅全部错误码、未就绪与长文本卡片。
 *
 * i18n-ignore-file：只在浏览器 mock 中加载的审阅页，标题与说明是写死的参照文字（视图本身的文案全部走 t()）。
 */
import type { PanelPayload } from '../../../shared/ipc/types'
import type { ViewProps } from '../../../panel/types'
import { TranslateView } from '../TranslateView'
import './states.css'

export interface StateCase {
  caption: string
  payload: Extract<PanelPayload, { kind: 'translate' }>
  maxHeight?: number
}

const noop = () => undefined

export function StatesPage({
  title,
  desc,
  cases,
}: {
  title: string
  desc: string
  cases: readonly StateCase[]
}) {
  return (
    <main className="tvm-page" data-testid="tv-states">
      <h1 className="tvm-title">{title}</h1>
      <p className="tvm-desc">{desc}</p>
      <div className="tvm-grid">
        {cases.map((c, i) => {
          const props: ViewProps<'translate'> = {
            sessionId: i + 1,
            payload: c.payload,
            phase: 'shown',
            focused: false,
            host: 'panel',
            frame: 'css',
            placement: 'below',
            maxHeight: c.maxHeight ?? 420,
            requestHide: noop,
            switchView: noop,
            activate: noop,
          }
          return (
            <div key={c.caption} className="tvm-cell">
              <span className="tvm-caption">{c.caption}</span>
              <div className="tvm-card">
                <div className="panel-content">
                  <TranslateView {...props} />
                </div>
              </div>
            </div>
          )
        })}
      </div>
    </main>
  )
}
