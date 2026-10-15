/**
 * 面板帧模式实验台的内容（开发者工具，比较两种帧模式的高度动画；只在注入了 `__WISP_FRAME_LAB__` 时动态加载）。
 *
 * 每个会话按固定剧本驱动卡片高度：进入 → 流式增长约 2.4 s（每 40 ms 追加 6 个字）→ 停 0.5 s → 一次收缩到两行 →
 * 停 0.7 s → 一次追加约 6 行 → 停 0.7 s → 请求关闭。Rust 侧的 `frame_lab.rs` 负责反复 prepare，并输出逐帧统计。
 */
import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import type { HideReason } from '../../shared/ipc/types'
import type { PanelPhase } from '../types'

const SAMPLE =
  '划词翻译把选中的文字送进模型，结果以流式方式逐字出现。Wisp keeps the panel light: the window grows with ' +
  'the text, driven by a spring, never janky. 面板高度跟随内容变化，材质只出现在卡片矩形之内，透明区域的点击' +
  '落到下方的应用。Streaming output arrives in small chunks, so the card must follow every few frames without ' +
  'tearing at the bottom edge. 窗口即卡片：逐帧改变窗口高度时，原生材质、着色与阴影都在同一个事务里更新。'

const CHUNK = 6
const CHUNK_MS = 40
const STREAM_MS = 2400

export interface FrameLabProps {
  phase: PanelPhase
  requestHide: (reason: HideReason) => void
}

export function FrameLab({ phase, requestHide }: FrameLabProps) {
  const [text, setText] = useState(SAMPLE.slice(0, 40))
  const started = useRef(false)
  const hideRef = useRef(requestHide)
  useLayoutEffect(() => {
    hideRef.current = requestHide
  }, [requestHide])

  // 剧本只在第一次进入时启动一次；计时器在卸载（会话结束）时才清理——阶段从 entering 变为 shown 不能打断它
  const timers = useRef<ReturnType<typeof setTimeout>[]>([])
  useEffect(() => {
    if (phase !== 'entering' || started.current) return
    started.current = true
    const at = (ms: number, f: () => void) => timers.current.push(setTimeout(f, ms))
    let n = 40
    for (let t = CHUNK_MS; t <= STREAM_MS; t += CHUNK_MS) {
      at(t, () => {
        n += CHUNK
        setText(sample(n))
      })
    }
    at(STREAM_MS + 500, () => setText(SAMPLE.slice(0, 40)))
    at(STREAM_MS + 1200, () => setText(sample(40 + 160)))
    at(STREAM_MS + 1900, () => hideRef.current('other'))
  }, [phase])
  useEffect(
    () => () => {
      for (const t of timers.current) clearTimeout(t)
    },
    [],
  )

  return (
    <div style={{ fontSize: 'var(--text-body)', lineHeight: 'var(--lh-body)' }}>
      <div
        style={{ fontSize: 'var(--text-meta)', lineHeight: 'var(--lh-meta)', color: 'var(--fg-2)' }}
      >
        {/* i18n-ignore：开发者实验台的标记 */}
        frame lab
      </div>
      <p style={{ margin: 0 }}>{text}</p>
    </div>
  )
}

/** 卡片底边的 2 px 品红标记（相对卡片定位） */
export function FrameLabMarker() {
  return (
    <div
      aria-hidden
      style={{
        position: 'absolute',
        left: 0,
        right: 0,
        bottom: 0,
        height: 2,
        background: '#ff00ff',
        pointerEvents: 'none',
      }}
    />
  )
}

function sample(n: number): string {
  let s = ''
  while (s.length < n) s += SAMPLE
  return s.slice(0, n)
}
