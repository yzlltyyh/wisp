/**
 * Thumbnail：截图 / 图片条目的缩略图。高度上限（OCR 视图 ≤ 96），等比缩放、圆角 + hairline；
 * 加载完成前显示占位底色，加载后 120 ms 淡入（不改变尺寸，CLS = 0：宽高由 `naturalSize` 预先确定）；
 * 透明图片下垫棋盘格。可点击放大（`onOpen`）。换了 `src`（例如重新截图）时加载状态重新开始：之前的错误图标不会残留，
 * 新图同样淡入。
 */
import { ImageOff } from 'lucide-react'
import { useState } from 'react'
import { cx } from './cx'
import { Icon } from './Icon'

export interface ThumbnailProps {
  src: string
  alt: string
  /** 原始像素尺寸（预先占位，避免加载后布局跳动） */
  naturalSize?: { width: number; height: number } | null
  maxHeight?: number
  maxWidth?: number
  /** 透明背景下垫棋盘格 */
  checkerboard?: boolean
  onOpen?: () => void
  /** 打开按钮的无障碍名称 */
  openLabel?: string
  className?: string
  'data-testid'?: string
}

export function Thumbnail({
  src,
  alt,
  naturalSize,
  maxHeight = 96,
  maxWidth,
  checkerboard = false,
  onOpen,
  openLabel,
  className,
  'data-testid': testId = 'thumbnail',
}: ThumbnailProps) {
  const [load, setLoad] = useState<{ src: string; state: 'loading' | 'loaded' | 'error' }>({
    src,
    state: 'loading',
  })
  // 渲染期比较：src 变了就回到 loading（不等 effect，不会有一帧旧状态）
  if (load.src !== src) setLoad({ src, state: 'loading' })
  const state = load.src === src ? load.state : 'loading'
  const settle = (next: 'loaded' | 'error') =>
    setLoad((cur) => (cur.src === src ? { src, state: next } : cur))
  let width: number | undefined
  let height: number | undefined
  if (naturalSize && naturalSize.width > 0 && naturalSize.height > 0) {
    const scale = Math.min(
      1,
      maxHeight / naturalSize.height,
      maxWidth ? maxWidth / naturalSize.width : 1,
    )
    width = Math.round(naturalSize.width * scale)
    height = Math.round(naturalSize.height * scale)
  }
  const body = (
    <span
      className={cx('ui-thumb', className)}
      data-state={state}
      data-checker={checkerboard || undefined}
      style={{ width, height, maxHeight, maxWidth }}
    >
      {state === 'error' ? (
        <Icon icon={ImageOff} size={20} label={alt} />
      ) : (
        <img
          key={src}
          src={src}
          alt={alt}
          draggable={false}
          decoding="async"
          data-releasable=""
          onLoad={() => settle('loaded')}
          onError={() => settle('error')}
        />
      )}
    </span>
  )
  if (!onOpen) return <span data-testid={testId}>{body}</span>
  return (
    <button
      type="button"
      className="ui-thumb-button"
      aria-label={openLabel ?? alt}
      onClick={onOpen}
      data-testid={testId}
    >
      {body}
    </button>
  )
}
