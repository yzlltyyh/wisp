/**
 * 动效令牌。全应用的动画时长、曲线、弹簧与动效相关的时间常量**只能**在这里定义，
 * 其他代码引用这里的导出项或 CSS 变量 `--dur-*` / `--ease-*`（tokens.css，同值）。
 *
 * Rust 侧的镜像（`windows::sizing`，原生帧模式的高度弹簧与窗口 alpha）由单测 `motion_tokens_match_frontend`
 * 对照本文件的 `duration` / `ease` / `spring` 字面量——修改这三项的写法或数值时同步 Rust。
 */
/** 单位：秒 */
export const duration = { instant: 0.08, fast: 0.14, base: 0.22, slow: 0.32 } as const
export const ease = {
  standard: [0.2, 0, 0, 1],
  exit: [0.4, 0, 1, 1],
  /** 往返脉冲（流式光标、录制中、状态点）：对称、两端都缓；CSS 动画用 --ease-pulse */
  pulse: [0.42, 0, 0.58, 1],
} as const
export const spring = {
  /** 面板进入、选中指示器 */
  snappy: { type: 'spring', stiffness: 520, damping: 38, mass: 1 },
  /** 高度跟随、固定窗口脱离 */
  gentle: { type: 'spring', stiffness: 260, damping: 30, mass: 1 },
} as const

/**
 * 其余与动效相关的时间常量（秒）。视图里的延迟、停留时长、stagger 同样不得写字面量。
 */
export const timing = {
  /** 视图切换的交叉淡入 */
  crossfade: 0.12,
  /** 流式文字块淡入 */
  chunkIn: 0.12,
  /** 减少动效时的淡入淡出上限 */
  reducedFade: 0.1,
  /** 菜单退出 */
  menuOut: 0.1,
  /**
   * 两种排版完全不同的整页交叉切换（引导的步骤）：新页等这么久再进来——旧页 menuOut（100 ms，ease.exit）此刻已淡到
   * 一半以下，两层文字只在低不透明度区间重叠（不等的话会同时以 40–80% 叠在一起约 100 ms）
   */
  stepCrossDelay: 0.06,
  /** 光标 / 骨架屏 / 录制中的脉冲周期（1 Hz） */
  pulse: 1,
  /** Tooltip 出现前的延迟 */
  tooltipDelay: 0.5,
  /** Popover / HoverPreview 出现前的延迟 */
  hoverDelay: 0.3,
  /** InlineToast「已复制」的停留 */
  toastHold: 1.5,
  /** 设置 / 引导窗口底部轻提示的停留（一句完整的说明，比「已复制」长；悬停暂停、错误不自动消失） */
  noticeHold: 3,
  /** 面板里的错误轻提示（剪贴板「置顶失败，请再试一次」）的停留：要读得完，悬停时暂停 */
  statusErrorHold: 4,
  /**
   * 带原因说明的 toast 视图：停留按字数加长（每字 toastViewPerChar），夹在 [min, max] 之间——两三行说明 1.6 s 读不完，
   * 可点按的也来不及把指针移上去；只有「已复制」一行时用 toastViewHold
   */
  toastViewDetailMin: 4,
  toastViewDetailMax: 5,
  toastViewPerChar: 0.05,
  /** toast 视图的停留 */
  toastViewHold: 1.6,
  /** 复制按钮显示对勾的时长 */
  copiedHold: 1.2,
  /** InlineConfirm 的有效期 */
  confirmHold: 2,
  /** 列表打开的 stagger 间隔 */
  stagger: 0.012,
  /** 菜单首字母跳转：连续输入的间隔上限 */
  typeahead: 0.5,
  /** Writer：空闲多久后合并 span */
  writerIdleMerge: 0.5,
  /** 退出兜底：动画回调没有到达时，在动画时长之外最多再等这么久就照样隐藏 */
  exitSlack: 0.12,
  /** 隐藏多久后释放图片与位图 */
  releaseAfterHidden: 120,
} as const

/** 曲线令牌 → CSS / WAAPI 的 easing 字符串（`el.animate(kf, { easing: cssEase(ease.standard) })`） */
export function cssEase(curve: readonly number[]): string {
  return `cubic-bezier(${curve.join(', ')})`
}

/** 秒 → 毫秒（给 setTimeout 等用） */
export function ms(seconds: number): number {
  return Math.round(seconds * 1000)
}
