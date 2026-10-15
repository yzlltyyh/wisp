/**
 * 动效预设。各视图的 `initial` / `animate` / `exit` / `transition`
 * **只能**引用这里的导出项（check-contract 检查），数值全部来自 `./tokens`。
 *
 * 用法：
 * - 直接展开：`<m.div {...usePreset('cardIn')}>`（自动按减少动效改写，见 `reducedPolicy`）；
 * - 带参数的预设：`adaptPreset(presets.stepSwitch(dir), 'fade', reduced)`；
 * - 只取过渡：`transition={presets.crossfade}`。
 *
 * 约束：只动 transform 与 opacity；唯一例外是高度（confirmIn / sectionExpand，容器设 contain）与
 * 风格切换正文的 2 px 模糊（Linux 与减少动效时去掉）。
 */
import type { TargetAndTransition, Transition } from 'motion/react'
import type { FrameMode, Placement } from '../ipc/types'
import { useReducedMotionPref } from './useReducedMotionPref'
import { duration, ease, spring, timing } from './tokens'

/** 可以直接展开到 `m.*` 上的属性子集 */
export interface PresetProps {
  initial?: TargetAndTransition | false
  animate?: TargetAndTransition
  exit?: TargetAndTransition
  transition?: Transition
}

const fade: Transition = { duration: timing.reducedFade, ease: ease.standard }
const standard = (seconds: number): Transition => ({ duration: seconds, ease: ease.standard })
const exiting = (seconds: number): Transition => ({ duration: seconds, ease: ease.exit })

/** 面板进入的位移幅度（y 6→0，在锚点上方时 −6→0） */
const ENTER_Y = 6
/** 面板进入的初始缩放 */
const ENTER_SCALE = 0.96
/** 次要浮层（菜单 / popover）的初始缩放 */
const MENU_SCALE = 0.98
/** 卡片 / toast / 行出现时的位移 */
const SHIFT = 4
/** 引导步骤切换的水平位移 */
const STEP_X = 8
/** 复制按钮等图标替换的初始缩放 */
const ICON_SCALE = 0.8
/** 「变淡」的透明度：风格切换时的旧正文、设置分组切换时新页的起点 */
const DIMMED = 0.3

export const presets = {
  /**
   * 面板进入。
   * - css：卡片 opacity 0→1（base + standard）、scale 0.96→1、y ±6→0（spring.snappy），变换原点在锚点一侧；
   * - native：窗口 alpha 由 Rust 做，这里只让卡片**内容**做 y（scale 恒为 1）。
   * transform-origin：Below → `${originX}px top`，Above → `${originX}px bottom`，Center → center。
   */
  panelEnter: (placement: Placement, originX: number, frame: FrameMode) => {
    const transformOrigin =
      placement === 'below'
        ? `${originX}px top`
        : placement === 'above'
          ? `${originX}px bottom`
          : 'center'
    const y = placement === 'above' ? -ENTER_Y : ENTER_Y
    if (frame === 'native') {
      return {
        initial: { y },
        animate: { y: 0 },
        transition: spring.snappy,
        transformOrigin,
      } as const
    }
    return {
      initial: { opacity: 0, scale: ENTER_SCALE, y },
      animate: { opacity: 1, scale: 1, y: 0 },
      transition: { ...spring.snappy, opacity: standard(duration.base) },
      transformOrigin,
    } as const
  },
  /**
   * 面板进入 · 窗口不能透明（Css 帧模式且 shadow_margin = 0）：卡片是铺满窗口的实色，不做透明度 / 缩放 / 位移
   * （否则露出 WebView 的默认底色）；只有卡片里的内容 opacity 0→1（fast + standard）
   */
  panelEnterOpaque: {
    initial: { opacity: 0 },
    animate: { opacity: 1 },
    transition: standard(duration.fast),
  },
  /**
   * 面板退出：opacity 1→0 + scale 1→0.98，fast + ease.exit，原地不动；Native 帧模式下 opacity 由窗口 alpha
   * 承担，前端不播。**显式写 y: 0**：Motion 会把 animate 里去掉的键退回 initial（进入时的 y ±6），退出时卡片就会往下沉
   * 5 px 左右（Chrome / WebKit 逐帧一致）
   */
  panelExit: {
    opacity: 0,
    scale: 0.98,
    y: 0,
    transition: exiting(duration.fast),
  },
  /** 视图切换：交叉淡入 120 ms */
  crossfade: standard(timing.crossfade),
  /** 流式文字块（CSS 动画 animate-chunk-in，时长同此） */
  chunkIn: { duration: timing.chunkIn },
  /** 按下（whileTap） */
  press: { scale: 0.98, transition: { duration: duration.instant } },
  /** 列表行进入（80 ms，ease.standard） */
  rowIn: {
    initial: { opacity: 0, y: SHIFT },
    animate: { opacity: 1, y: 0 },
    transition: standard(duration.instant),
  },
  /** 只作用于前 8 行；最后一行在 84 ms 开始、整体 ≤ 170 ms */
  listStagger: { staggerChildren: timing.stagger, maxItems: 8 },
  /** 风格切换：正文变淡 + 2 px 模糊（fast）；Linux / 减少动效时用 styleSwitchOutPlain */
  // 变暗是出场（ease.exit），恢复是入场（ease.standard）——与其他动效同一手感
  styleSwitchOut: { opacity: DIMMED, filter: 'blur(2px)', transition: exiting(duration.fast) },
  styleSwitchIn: { opacity: 1, filter: 'blur(0px)', transition: standard(duration.base) },
  styleSwitchOutPlain: { opacity: DIMMED, transition: exiting(duration.fast) },
  styleSwitchInPlain: { opacity: 1, transition: standard(duration.base) },
  /** Overlay 暗化层 0 → 0.45（颜色取 --dim-overlay） */
  overlayDim: standard(duration.slow),
  /** Overlay 选区飞入面板 */
  flyTo: standard(duration.base),
  /** 固定后「浮起」：卡片 y −2→0；阴影用带 --shadow-pinned 的 ::after 层 opacity 0→1 */
  pinLift: { y: [-2, 0], transition: spring.gentle },
  reduced: { fade },

  // ---- 次要动效 ----

  /** 弹出层进入：opacity 0→1、scale 0.98→1，变换原点在触发器一侧（由组件设 transformOrigin） */
  menuIn: {
    initial: { opacity: 0, scale: MENU_SCALE },
    animate: { opacity: 1, scale: 1, transition: standard(duration.fast) },
  },
  /** 弹出层退出：opacity→0，100 ms，ease.exit */
  menuOut: { opacity: 0, transition: exiting(timing.menuOut) },
  /** Tooltip（500 ms 延迟后）：80 ms 纯淡入 / 淡出 */
  tooltip: {
    initial: { opacity: 0 },
    animate: { opacity: 1, transition: standard(duration.instant) },
    exit: { opacity: 0, transition: exiting(duration.instant) },
  },
  /** Popover / HoverPreview（300 ms 后）：同 menuIn */
  popover: {
    initial: { opacity: 0, scale: MENU_SCALE },
    animate: { opacity: 1, scale: 1, transition: standard(duration.fast) },
    exit: { opacity: 0, transition: exiting(timing.menuOut) },
  },
  /** Space 全文预览：popover + y 4→0（base） */
  previewIn: {
    initial: { opacity: 0, scale: MENU_SCALE, y: SHIFT },
    animate: { opacity: 1, scale: 1, y: 0, transition: standard(duration.base) },
    exit: { opacity: 0, transition: exiting(timing.menuOut) },
  },
  /** InlineToast / toast 视图：y 4→0 + 淡入（base），停留后 fast 淡出 */
  toast: {
    initial: { opacity: 0, y: SHIFT },
    animate: { opacity: 1, y: 0, transition: standard(duration.base) },
    exit: { opacity: 0, transition: exiting(duration.fast) },
  },
  /** InlineConfirm / 清空确认条：高度 spring.gentle 展开 + 内容交叉淡入 120 ms */
  confirmIn: {
    initial: { height: 0, opacity: 0 },
    animate: {
      height: 'auto',
      opacity: 1,
      transition: { height: spring.gentle, opacity: standard(timing.crossfade) },
    },
    exit: {
      height: 0,
      opacity: 0,
      transition: { height: spring.gentle, opacity: exiting(timing.crossfade) },
    },
  },
  /** ErrorCard / NotReadyCard / LongTextPrompt / EmptyState 出现：opacity + y 4→0（base） */
  cardIn: {
    initial: { opacity: 0, y: SHIFT },
    animate: { opacity: 1, y: 0, transition: standard(duration.base) },
  },
  /** 完成后的 Footer：fast 淡入 */
  footerIn: {
    initial: { opacity: 0 },
    animate: { opacity: 1, transition: standard(duration.fast) },
  },
  /** 图标替换（复制 → 对勾）：交叉淡入 + scale 0.8→1（instant） */
  iconSwap: {
    initial: { opacity: 0, scale: ICON_SCALE },
    animate: { opacity: 1, scale: 1, transition: standard(duration.instant) },
    exit: { opacity: 0, scale: ICON_SCALE, transition: exiting(duration.instant) },
  },
  /** OCR 译文区展开、原文显示 / 隐藏：高度 spring.gentle + 内容交叉淡入 120 ms */
  sectionExpand: {
    initial: { height: 0, opacity: 0 },
    animate: {
      height: 'auto',
      opacity: 1,
      transition: { height: spring.gentle, opacity: standard(timing.crossfade) },
    },
    exit: {
      height: 0,
      opacity: 0,
      transition: { height: spring.gentle, opacity: exiting(timing.crossfade) },
    },
  },
  /**
   * 设置分组切换：交叉淡入 120 ms，不位移。旧页随 key 立即卸载、不叠放，所以新页不从 0 开始（那样有一整帧内容区全空，
   * 长页面之间快速切换时像闪了一下），而是从风格切换同样的「变淡」状态（0.3）回到 1
   */
  pageSwitch: {
    initial: { opacity: DIMMED },
    animate: { opacity: 1, transition: standard(timing.crossfade) },
    exit: { opacity: 0, transition: exiting(timing.crossfade) },
  },
  /**
   * 收起（可折叠区块、列表里被删除的一行）：高度与淡出同在 fast + ease.exit 内结束——不留「内容已经淡没、高度还在
   * 慢慢收」的空白；展开仍用 sectionExpand 的弹簧
   */
  collapse: {
    exit: { height: 0, opacity: 0, transition: exiting(duration.fast) },
  },
  /**
   * 引导步骤切换：新步骤 x ±8→0 + 淡入（base），旧步骤原地 fast 淡出（交叉进出，不留空白帧）；direction = 1 前进，
   * -1 后退。exit 显式写 x: 0（不写的话 Motion 把 x 退回 initial，旧步骤会边淡出边滑 8 px）
   */
  stepSwitch: (direction: 1 | -1): PresetProps => ({
    initial: { opacity: 0, x: STEP_X * direction },
    animate: { opacity: 1, x: 0, transition: standard(duration.base) },
    exit: { opacity: 0, x: 0, transition: exiting(duration.fast) },
  }),
  /**
   * 整页交叉切换（引导的步骤：新旧两页排版完全不同、同时在场）：旧页 menuOut（100 ms，ease.exit）原地淡出；新页延后
   * stepCrossDelay（60 ms）再 x ±8→0 + 淡入（base）——两层只在低不透明度区间重叠，不会叠成两层都看得清的文字。
   * 只有新页进入动画的场合（设置的子视图、菜单分页）仍用 stepSwitch（没有旧页可等，不延后）
   */
  stepCross: (direction: 1 | -1): PresetProps => ({
    initial: { opacity: 0, x: STEP_X * direction },
    animate: {
      opacity: 1,
      x: 0,
      transition: { ...standard(duration.base), delay: timing.stepCrossDelay },
    },
    exit: { opacity: 0, x: 0, transition: exiting(timing.menuOut) },
  }),
  /** 随整页交叉切换一起换的外框部件（引导的步骤条、底栏按钮组）：与新页同时淡入（fast，同样延后），旧的 menuOut 淡出 */
  stepChrome: {
    initial: { opacity: 0 },
    animate: {
      opacity: 1,
      transition: { ...standard(duration.fast), delay: timing.stepCrossDelay },
    },
    exit: { opacity: 0, transition: exiting(timing.menuOut) },
  },
  /**
   * 单一选中指示器在行间滑动（设置侧栏的选中背景、剪贴板列表的选中条）：spring.snappy；减少动效时直接到位。
   * 只给过渡：位置由组件按行的偏移算出
   */
  indicatorSlide: spring.snappy,
  /** 状态变绿 / 录制中：图标 scale 0.9→1（spring.snappy）；录制脉冲用 CSS（.ui-recording） */
  statusPulse: {
    initial: { scale: 0.9 },
    animate: { scale: 1, transition: spring.snappy },
  },
  /** 剪贴板实时插入的新条目、Banner 出现：y −4→0 + 淡入（base） */
  rowInsert: {
    initial: { opacity: 0, y: -SHIFT },
    animate: { opacity: 1, y: 0, transition: standard(duration.base) },
  },
} as const

/**
 * 减少动效时的改写策略：
 * - `fade`：只保留 opacity，过渡改为 ≤ 100 ms 的淡入淡出；
 * - `none`：直接出现 / 直接到位（initial = false，exit 立即完成）；
 * - `keep`：不变（本身就只有短淡入淡出）。
 */
export type ReducedPolicy = 'fade' | 'none' | 'keep'

export const reducedPolicy = {
  menuIn: 'fade',
  tooltip: 'keep',
  popover: 'fade',
  previewIn: 'fade',
  toast: 'fade',
  confirmIn: 'none',
  cardIn: 'fade',
  footerIn: 'keep',
  stepChrome: 'fade',
  iconSwap: 'none',
  sectionExpand: 'none',
  collapse: 'none',
  // 减少动效时 ≤ 100 ms 纯淡入淡出（不用 keep：120 ms 交叉淡入超出上限）
  pageSwitch: 'fade',
  statusPulse: 'none',
  rowInsert: 'none',
  rowIn: 'none',
} as const satisfies Record<string, ReducedPolicy>

export type PresetName = keyof typeof reducedPolicy

/** 只保留 opacity（其余属性丢弃） */
function opacityOnly(t: TargetAndTransition | undefined): TargetAndTransition | undefined {
  if (!t || t.opacity === undefined) return undefined
  return { opacity: t.opacity, transition: fade }
}

/** 按策略改写一组预设属性 */
export function adaptPreset(p: PresetProps, policy: ReducedPolicy, reduced: boolean): PresetProps {
  if (!reduced || policy === 'keep') return p
  if (policy === 'none') {
    const instant: Transition = { duration: 0 }
    return {
      initial: false,
      ...(p.animate ? { animate: { ...p.animate, transition: instant } } : null),
      ...(p.exit ? { exit: { opacity: 0, transition: instant } } : null),
      transition: instant,
    }
  }
  const initial = p.initial === false ? false : opacityOnly(p.initial)
  const animate = opacityOnly(p.animate)
  const exit = opacityOnly(p.exit)
  return {
    ...(initial !== undefined ? { initial } : null),
    ...(animate ? { animate } : null),
    ...(exit ? { exit } : null),
    transition: fade,
  }
}

/** 取一个无参预设，按当前减少动效偏好改写；可直接展开到 `m.*` 上 */
export function usePreset(name: PresetName): PresetProps {
  const reduced = useReducedMotionPref()
  return adaptPreset(presets[name], reducedPolicy[name], reduced)
}
