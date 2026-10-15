/**
 * 浏览器级逐帧用例的公共判断。
 *
 * CI 的 e2e 作业跑在 Playwright 官方容器里：没有 GPU，headless WebKit 走软件合成，动效期间帧间隔常在 4 帧以上
 * （同一批用例在 macOS 的 WebKit 与容器里的 Chromium 都稳定通过）。这种环境渲染不了 60 Hz 的动效，「相邻两帧
 * 速度比」「多少毫秒内结束」「卡片长高期间不掉帧」这类逐帧时序断言在那里没有意义——用例按**动效进行期间的帧间隔**
 * 判断环境是否「饿帧」，饿帧时跳过时序断言并写明原因；布局一致性断言（外框与内容同高、不裁切、不留白、只朝一个
 * 方向）照常判。能跑满帧率的环境里判定不变。
 */
import { test } from '@playwright/test'

/** 一帧（60 Hz） */
export const FRAME_MS = 1000 / 60
/** 超过这么多帧的间隔算长帧 */
const LONG_FRAMES = 2
/** 长帧占比超过这个值：环境饿帧 */
const STARVED_RATIO = 0.2
/** 出现一个超过这么多帧的间隔就算饿帧（动效的时长、单帧位移都已无从判断） */
const STALL_FRAMES = 3

/** 一串帧时间戳（毫秒，递增）里长帧间隔的占比；少于 3 帧时为 0 */
export function longFrameRatio(stamps: readonly number[]): number {
  if (stamps.length < 3) return 0
  let long = 0
  for (let i = 1; i < stamps.length; i++)
    if ((stamps[i] ?? 0) - (stamps[i - 1] ?? 0) > LONG_FRAMES * FRAME_MS) long++
  return long / (stamps.length - 1)
}

/** 最长的帧间隔（帧数） */
function longestGap(stamps: readonly number[]): number {
  let max = 0
  for (let i = 1; i < stamps.length; i++)
    max = Math.max(max, ((stamps[i] ?? 0) - (stamps[i - 1] ?? 0)) / FRAME_MS)
  return max
}

/**
 * 这段采样（调用方截取动效进行的那段时间）是否饿帧：有一个间隔超过 3 帧，或超过 20% 的间隔长于 2 帧。主线程在动效
 * 开始 / 进行时被软件合成拖住，动效的结束时刻与逐帧位移都会被拉长、合并，时序断言没有意义
 */
export function starved(stamps: readonly number[]): boolean {
  return longestGap(stamps) > STALL_FRAMES || longFrameRatio(stamps) > STARVED_RATIO
}

/** 饿帧的说明（注解 / skip 的原因） */
function why(stamps: readonly number[]): string {
  return `最长帧间隔 ${longestGap(stamps).toFixed(1)} 帧、${Math.round(longFrameRatio(stamps) * 100)}% 的间隔长于 ${LONG_FRAMES} 帧（无 GPU 的容器里 headless 引擎走软件合成）`
}

/**
 * 饿帧时跳过用例剩下的逐帧时序断言（Playwright 的运行时 skip，结果记为 skipped 并带上原因）。`what` 写明跳过的是哪类
 * 断言
 */
export function skipIfStarved(stamps: readonly number[], what: string): void {
  test.skip(starved(stamps), `${why(stamps)}：跳过${what}；本地与能跑满帧率的环境照常判`)
}

/**
 * 饿帧时不判某一类时序断言、但用例其余断言照常（记一条注解说明没判什么）。返回是否饿帧：调用方据此决定判不判
 */
export function noteStarved(stamps: readonly number[], what: string): boolean {
  if (!starved(stamps)) return false
  test.info().annotations.push({
    type: 'starved-frames',
    description: `${why(stamps)}：没有判${what}（布局断言照常）`,
  })
  return true
}

/**
 * rAF 回调里读到的状态对应的时刻：长帧里采样回调可能排在动画回调之前，读到的是上一帧动画更新后的状态，所以第 i 个
 * 采样的状态最早属于第 i − 1 个采样的时刻
 */
export function stateTime(stamps: readonly number[], i: number): number {
  return i > 0 ? (stamps[i - 1] ?? 0) : (stamps[0] ?? 0)
}
