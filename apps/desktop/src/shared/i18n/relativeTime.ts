/**
 * 相对时间：刚刚 / N 分钟 / N 小时 / N 天 / 日期。自写，不依赖 Intl.RelativeTimeFormat 的措辞，
 * 以便中英文都有紧凑写法（剪贴板行尾用 `short`）。超过 7 天显示日期（同年省略年份）。
 */
import { getLocale, t } from './i18n'
import { loadCoreMessages } from './messages'

loadCoreMessages()

const MINUTE = 60_000
const HOUR = 60 * MINUTE
const DAY = 24 * HOUR
/** 少于这么久都算「刚刚」 */
const JUST_NOW = MINUTE
/** 超过这么多天显示日期 */
const DATE_AFTER_DAYS = 7

export type RelativeStyle = 'long' | 'short'

const dateFormats = new Map<string, Intl.DateTimeFormat>()

function dateFormat(locale: string, withYear: boolean): Intl.DateTimeFormat {
  const key = `${locale}|${withYear}`
  let f = dateFormats.get(key)
  if (!f) {
    f = new Intl.DateTimeFormat(locale, {
      ...(withYear ? { year: 'numeric' } : null),
      month: 'short',
      day: 'numeric',
    })
    dateFormats.set(key, f)
  }
  return f
}

export function relativeTime(
  timestampMs: number,
  nowMs: number = Date.now(),
  style: RelativeStyle = 'long',
): string {
  const diff = Math.max(0, nowMs - timestampMs)
  if (diff < JUST_NOW) return t('common.time.just_now')
  const short = style === 'short'
  if (diff < HOUR) {
    const count = Math.floor(diff / MINUTE)
    return short ? t('common.time.minutes_short', { count }) : t('common.time.minutes', { count })
  }
  if (diff < DAY) {
    const count = Math.floor(diff / HOUR)
    return short ? t('common.time.hours_short', { count }) : t('common.time.hours', { count })
  }
  const days = Math.floor(diff / DAY)
  if (days < DATE_AFTER_DAYS) {
    return short
      ? t('common.time.days_short', { count: days })
      : t('common.time.days', { count: days })
  }
  const then = new Date(timestampMs)
  const sameYear = then.getFullYear() === new Date(nowMs).getFullYear()
  return dateFormat(getLocale(), !sameYear).format(then)
}
