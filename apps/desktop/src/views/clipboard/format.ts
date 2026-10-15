/**
 * 剪贴板的时间格式。行尾一律是相对时间（刚刚 / N 分钟 / N 小时 / N 天 / 超过 7 天显示日期，同一种表达
 * 贯穿置顶与最近两组）；全文预览的元信息给出完整时间。
 */
import { getLocale, t } from '../../shared/i18n/i18n'
import { relativeTime } from '../../shared/i18n/relativeTime'
import { timeGroup } from './model'

type Style = 'time' | 'weekday' | 'date' | 'date-year'

const formats = new Map<string, Intl.DateTimeFormat>()

function format(style: Style): Intl.DateTimeFormat {
  const locale = getLocale()
  const key = `${locale}|${style}`
  let f = formats.get(key)
  if (!f) {
    const options: Intl.DateTimeFormatOptions =
      style === 'time'
        ? { hour: '2-digit', minute: '2-digit', hourCycle: 'h23' }
        : style === 'weekday'
          ? { weekday: 'short' }
          : style === 'date'
            ? { month: 'short', day: 'numeric' }
            : { year: 'numeric', month: 'short', day: 'numeric' }
    f = new Intl.DateTimeFormat(locale, options)
    formats.set(key, f)
  }
  return f
}

/** 行尾时间（紧凑写法） */
export function rowTime(ts: number, now: number): string {
  return relativeTime(ts, now, 'short')
}

/**
 * 完整的时间（全文预览的元信息）：今天 →「4 分钟前 · 14:32」；昨天 →「昨天 14:32」；近 7 天 →「周三 14:32」；
 * 更早 →「10月3日 14:32」（跨年带年份）
 */
export function fullTime(ts: number, now: number): string {
  const clock = format('time').format(ts)
  switch (timeGroup(ts, now)) {
    case 'today':
      return `${relativeTime(ts, now, 'long')} · ${clock}`
    case 'yesterday':
      return `${t('common.time.yesterday')} ${clock}`
    case 'week':
      return `${format('weekday').format(ts)} ${clock}`
    case 'earlier': {
      const sameYear = new Date(ts).getFullYear() === new Date(now).getFullYear()
      return `${format(sameYear ? 'date' : 'date-year').format(ts)} ${clock}`
    }
  }
}

/**
 * 行尾与预览用到的 Intl 格式器：面板常驻、启动后空闲时先构造好（第一次构造要加载 ICU 数据，约几毫秒），
 * 第一次唤出剪贴板时不把它带进 prepare 的同步渲染。relativeTime 的日期分支（超过 7 天、跨年）也在这里预热
 */
export function warmRowTimeFormats(now = Date.now()): void {
  const day = 86_400_000
  for (const ago of [30 * day, 400 * day]) rowTime(now - ago, now)
  fullTime(now - 3 * day, now)
}
