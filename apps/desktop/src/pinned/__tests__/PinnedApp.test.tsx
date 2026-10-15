/**
 * 固定窗口：
 * - pinned_init → 首帧同步渲染 → pinned_ready(id, content_height) → 浮起；
 * - 流式中固定：stream_attach 回放完才 ready，之后继续接收；
 * - 在固定窗口里发起的请求 session_id = 0；
 * - 置顶开关（初始值由 Rust 在 PinState.on_top 给出）、Esc / ⌘W 关闭（窗口销毁）、⌘P 不落到 WebView；
 * - 浮起：首帧停在动画起点，交接之后动画确实启动；只有头部可拖动；
 * - 展开：按 Rust 给出的 PinState.expand_width、不设内部滚动上限测量首帧；
 * - 结构化状态行（PinState.status）、实际使用的模型（model_ref，重新生成沿用）与词典模式（dict_mode）。
 */
import { act, render, screen, waitFor, fireEvent } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { getAppearance } from '../../shared/hooks/useAppearance'
import { boot } from '../../shared/ipc'
import type { PinState } from '../../shared/ipc/types'
import {
  delta,
  done,
  flush,
  idleTransport,
  installFakeIpc,
  press,
  start,
  type FakeIpc,
} from '../../views/translate/__tests__/harness'
import { PinnedApp } from '../PinnedApp'

let ipc: FakeIpc
const savedLabel = boot.label

function pinState(patch: Partial<PinState> = {}): PinState {
  return {
    kind: 'translate',
    source_text: 'The quick brown fox jumps over the lazy dog.',
    result_text: '敏捷的棕色狐狸跳过了懒狗。',
    task: 'translate',
    style_id: 'natural',
    target_lang: 'zh-CN',
    detected_lang: 'en',
    request_id: null,
    capture_id: null,
    ocr_text: null,
    ocr_mode: null,
    show_source: true,
    status_line: 'gpt-5-mini · 1.2 s',
    status: {
      model_label: 'gpt-5-mini',
      elapsed_ms: 1200,
      usage: null,
      cached: false,
      cancelled: false,
      truncated: null,
    },
    model_ref: { provider_id: 'openai', model_id: 'gpt-5-mini' },
    ...patch,
  }
}

beforeEach(() => {
  ipc = installFakeIpc()
  localStorage.clear()
  boot.label = 'pinned-abc'
  vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) =>
    setTimeout(() => cb(performance.now()), 0),
  )
  vi.stubGlobal('cancelAnimationFrame', (id: number) => clearTimeout(id))
})
afterEach(() => {
  boot.label = savedLabel
  idleTransport()
  vi.unstubAllGlobals()
})

describe('固定窗口', () => {
  it('已结束的结果：首帧同步写入并测量，随即 pinned_ready；之后浮起；原文对照与状态行随 PinState 交接', async () => {
    ipc.handlers.pinned_init = () => pinState()
    render(<PinnedApp />)
    await waitFor(() => expect(ipc.names()).toContain('pinned_ready'))
    const names = ipc.names()
    expect(names.indexOf('pinned_init')).toBeLessThan(names.indexOf('pinned_ready'))
    const [, args] = ipc.calls.find(([c]) => c === 'pinned_ready') ?? []
    expect(args).toMatchObject({ pinned_id: 'abc' })
    expect(Number.isFinite(args?.content_height)).toBe(true)
    expect(screen.getByTestId('tv-stream-body')).toHaveTextContent('敏捷的棕色狐狸跳过了懒狗。')
    expect(screen.getByTestId('tv-source')).toBeInTheDocument()
    // 结构化状态行（PinState.status）：与面板同一个 StatusLine
    const parts = Array.from(screen.getByTestId('tv-status').querySelectorAll('.ui-status-part'))
    expect(parts.map((p) => p.textContent)).toEqual(['gpt-5-mini', '1.2 s'])
    await waitFor(() => expect(screen.getByTestId('pinned-card')).toHaveAttribute('data-lifted'))
    // 固定窗口保留复制、重新生成、原文对照，没有固定 / 展开；窗口控件并进视图头部
    expect(screen.getByTestId('tv-regenerate')).toBeInTheDocument()
    expect(screen.queryByTestId('tv-pin')).toBeNull()
    expect(screen.queryByTestId('tv-expand')).toBeNull()
    expect(screen.getByTestId('pinned-on-top')).toBeInTheDocument()
    expect(document.querySelector('.pinned-header')).toBeNull()
  })

  it('流式中固定：stream_attach 回放完才 ready，之后继续接收；不调用 stream 的命令以外的任何流', async () => {
    ipc.handlers.pinned_init = () =>
      pinState({ request_id: 'rp', result_text: '敏捷', status_line: null })
    ipc.replay.set('rp', [start('rp'), delta('敏捷'), delta('的')])
    // pinned_ready 的那一刻（首帧测量）回放的文字已经在 DOM 里
    let atReady = ''
    ipc.handlers.pinned_ready = () => {
      atReady = document.querySelector('[data-testid="tv-stream-body"]')?.textContent ?? ''
      return undefined
    }
    render(<PinnedApp />)
    await waitFor(() => expect(ipc.names()).toContain('pinned_ready'))
    const names = ipc.names()
    expect(names.indexOf('stream_attach')).toBeLessThan(names.indexOf('pinned_ready'))
    expect(atReady).toBe('敏捷的')
    expect(screen.getByTestId('tv-stream-body')).toHaveTextContent('敏捷的')
    ipc.push('rp', delta('狐狸'), done())
    await waitFor(() =>
      expect(screen.getByTestId('tv-stream-body')).toHaveTextContent('敏捷的狐狸'),
    )
  })

  it('请求已结束、日志已释放（attach 失败）：退回 PinState 里的结果', async () => {
    ipc.handlers.pinned_init = () => pinState({ request_id: 'gone' })
    ipc.handlers.stream_attach = () => {
      throw { code: 'not_found', message: 'gone', llm: null }
    }
    render(<PinnedApp />)
    await waitFor(() => expect(ipc.names()).toContain('pinned_ready'))
    expect(screen.getByTestId('tv-stream-body')).toHaveTextContent('敏捷的棕色狐狸跳过了懒狗。')
  })

  it('在固定窗口里换风格：请求的 session_id 为 0；方向随内容一起脱离', async () => {
    ipc.handlers.pinned_init = () => pinState()
    render(<PinnedApp />)
    await waitFor(() => expect(ipc.names()).toContain('pinned_ready'))
    press('Digit2', { mod: true })
    await flush()
    expect(ipc.starts.at(-1)).toMatchObject({
      session_id: 0,
      style_id: 'technical',
      target_lang: 'zh-CN',
    })
  })

  it('置顶开关与关闭：Esc / ⌘W / ✕ 关闭窗口', async () => {
    ipc.handlers.pinned_init = () => pinState()
    render(<PinnedApp />)
    await waitFor(() => expect(ipc.names()).toContain('pinned_ready'))
    const attaches = ipc.names().filter((n) => n === 'stream_attach').length
    fireEvent.click(screen.getByTestId('pinned-on-top'))
    await flush()
    // 切换置顶不会让视图重新接上 / 重新请求
    expect(ipc.names().filter((n) => n === 'stream_attach')).toHaveLength(attaches)
    expect(ipc.starts).toHaveLength(0)
    expect(ipc.calls.find(([c]) => c === 'pinned_set_on_top')?.[1]).toEqual({
      pinned_id: 'abc',
      on_top: false,
    })
    expect(screen.getByTestId('pinned-on-top')).toHaveAttribute('aria-pressed', 'false')
    // 新窗口的默认值由 Rust 记录，前端不再自己存
    expect(localStorage.length).toBe(0)
    press('Escape')
    press('KeyW', { mod: true })
    fireEvent.click(screen.getByTestId('pinned-close'))
    expect(ipc.names().filter((n) => n === 'window.close')).toHaveLength(3)
  })

  it('置顶开关的初始值取 PinState.on_top（Rust 填写），一次点击就生效；旧 Rust 不给时默认置顶', async () => {
    ipc.handlers.pinned_init = () => pinState({ on_top: false })
    render(<PinnedApp />)
    await waitFor(() => expect(ipc.names()).toContain('pinned_ready'))
    expect(screen.getByTestId('pinned-on-top')).toHaveAttribute('aria-pressed', 'false')
    fireEvent.click(screen.getByTestId('pinned-on-top'))
    await flush()
    expect(ipc.calls.filter(([c]) => c === 'pinned_set_on_top').map(([, a]) => a.on_top)).toEqual([
      true,
    ])
    expect(screen.getByTestId('pinned-on-top')).toHaveAttribute('aria-pressed', 'true')
  })

  it('浮起：首帧（窗口出现的那一帧）卡片就停在动画起点，交接之后动画确实启动并回到 0；只有头部可拖动', async () => {
    let atReady = ''
    ipc.handlers.pinned_init = () => pinState()
    ipc.handlers.pinned_ready = () => {
      atReady = (document.querySelector('[data-testid="pinned-card"]') as HTMLElement).style
        .transform
      return undefined
    }
    render(<PinnedApp />)
    await waitFor(() => expect(ipc.names()).toContain('pinned_ready'))
    expect(atReady).toBe('translateY(-2px)')
    const card = screen.getByTestId('pinned-card')
    await waitFor(() => expect(card).toHaveAttribute('data-lifted'))
    await waitFor(() => expect(card.style.transform).not.toBe('translateY(-2px)'))
    await waitFor(() => expect(['none', '', 'translateY(0px)']).toContain(card.style.transform), {
      timeout: 3000,
    })
    expect(document.querySelector('.pinned-content')).not.toHaveAttribute('data-tauri-drag-region')
    expect(document.querySelector('.tv-header')).toHaveAttribute('data-tauri-drag-region')
  })

  it('⌘P / Ctrl+P 在固定窗口里被吞掉（不落到 WebView 的打印预览）', async () => {
    ipc.handlers.pinned_init = () => pinState()
    render(<PinnedApp />)
    await waitFor(() => expect(ipc.names()).toContain('pinned_ready'))
    const mac = boot.os === 'macos'
    const event = new KeyboardEvent('keydown', {
      code: 'KeyP',
      key: 'p',
      metaKey: mac,
      ctrlKey: !mac,
      bubbles: true,
      cancelable: true,
    })
    act(() => {
      document.body.dispatchEvent(event)
    })
    expect(event.defaultPrevented).toBe(true)
    expect(ipc.names()).not.toContain('panel_pin')
  })

  it('展开 ⤢：首帧按 PinState.expand_width 测量、正文不设内部滚动上限；窗口追上之后铺满窗口', async () => {
    let width = ''
    let scrollable = true
    ipc.handlers.pinned_init = () => pinState({ expand_width: 640 })
    ipc.handlers.pinned_ready = () => {
      width = (document.querySelector('[data-testid="pinned-card"]') as HTMLElement).style.width
      scrollable =
        document.querySelector('[data-testid="tv-stream"]')?.classList.contains('ui-scroll') ?? true
      return undefined
    }
    render(<PinnedApp />)
    await waitFor(() => expect(ipc.names()).toContain('pinned_ready'))
    expect(width).toBe('640px')
    expect(scrollable).toBe(false)
  })

  it('普通固定（没有 expand_width）：首帧按窗口宽度测量，卡片不设宽度', async () => {
    let width = 'unset'
    ipc.handlers.pinned_init = () => pinState()
    ipc.handlers.pinned_ready = () => {
      width = (document.querySelector('[data-testid="pinned-card"]') as HTMLElement).style.width
      return undefined
    }
    render(<PinnedApp />)
    await waitFor(() => expect(ipc.names()).toContain('pinned_ready'))
    expect(width).toBe('')
  })

  it('重新生成沿用固定时实际使用的模型（model_ref）；词典模式随 dict_mode 交接', async () => {
    ipc.handlers.pinned_init = () =>
      pinState({ dict_mode: true, result_text: 'fox /fɒks/ n. 狐狸' })
    render(<PinnedApp />)
    await waitFor(() => expect(ipc.names()).toContain('pinned_ready'))
    fireEvent.click(screen.getByTestId('tv-regenerate'))
    await flush()
    expect(ipc.starts.at(-1)).toMatchObject({
      session_id: 0,
      model_ref: { provider_id: 'openai', model_id: 'gpt-5-mini' },
    })
  })

  it('结构化状态行：已取消只显示「已取消」；命中缓存不显示耗时', async () => {
    ipc.handlers.pinned_init = () =>
      pinState({
        status: {
          model_label: 'gpt-5-mini',
          elapsed_ms: null,
          usage: null,
          cached: true,
          cancelled: false,
          truncated: null,
        },
      })
    const { unmount } = render(<PinnedApp />)
    await waitFor(() => expect(ipc.names()).toContain('pinned_ready'))
    const parts = Array.from(screen.getByTestId('tv-status').querySelectorAll('.ui-status-part'))
    expect(parts.map((p) => p.textContent)).toEqual(['gpt-5-mini'])
    unmount()
    ipc.handlers.pinned_init = () =>
      pinState({
        status: {
          model_label: 'gpt-5-mini',
          elapsed_ms: 800,
          usage: null,
          cached: false,
          cancelled: true,
          truncated: null,
        },
      })
    render(<PinnedApp />)
    await waitFor(() => expect(screen.getByTestId('tv-status')).toHaveTextContent('已取消'))
  })

  it('置顶命令失败：开关回到原状态', async () => {
    ipc.handlers.pinned_init = () => pinState({ on_top: true })
    ipc.handlers.pinned_set_on_top = () => {
      throw new Error('window gone')
    }
    render(<PinnedApp />)
    await waitFor(() => expect(ipc.names()).toContain('pinned_ready'))
    const toggle = screen.getByTestId('pinned-on-top')
    fireEvent.click(toggle)
    await flush()
    expect(toggle).toHaveAttribute('aria-pressed', 'true')
  })

  it('展开没有成功（窗口一直是面板宽度）：到时放开卡片宽度、铺满窗口，不被裁掉', async () => {
    ipc.handlers.pinned_init = () => pinState({ expand_width: 640 })
    // 窗口按面板宽度（360）创建
    const innerWidth = window.innerWidth
    Object.defineProperty(window, 'innerWidth', { value: 360, configurable: true })
    try {
      render(<PinnedApp />)
      await waitFor(() => expect(ipc.names()).toContain('pinned_ready'))
      const card = screen.getByTestId('pinned-card')
      await waitFor(() => expect(card).toHaveAttribute('data-lifted'))
      expect(card.style.width).toBe('640px')
      // 窗口一直没有长到展开宽度：约 1.5 s 后放开
      await waitFor(() => expect(card.style.width).toBe(''), { timeout: 3000 })
      expect(card).not.toHaveAttribute('data-expanding')
    } finally {
      Object.defineProperty(window, 'innerWidth', { value: innerWidth, configurable: true })
    }
  })

  it('OCR 结果的固定窗口：独立标题栏 + OCR 视图，挂载后即 ready；缩略图与尺寸来自 PinState', async () => {
    ipc.handlers.pinned_init = () =>
      pinState({
        kind: 'ocr',
        ocr_text: 'text',
        ocr_mode: 'lines',
        capture_id: 'cap-1',
        thumb_url: 'crop/cap-1.png',
        image_size: { width: 1280, height: 360 },
      })
    render(<PinnedApp />)
    await waitFor(() => expect(ipc.names()).toContain('pinned_ready'))
    expect(document.querySelector('.pinned-header')).not.toBeNull()
    expect(screen.getByTestId('view-ocr')).toBeInTheDocument()
    // 逻辑尺寸的单位按平台：macOS 写 pt，Windows / Linux 写 px
    const unit = getAppearance().os === 'macos' ? 'pt' : 'px'
    expect(screen.getByTestId('ocr-size')).toHaveTextContent(`1280 × 360 ${unit}`)
    // 头部 ⋯（重新截图 / 识别历史）只在面板里
    expect(screen.queryByTestId('ocr-more')).toBeNull()
  })
})
