/**
 * 数值显示：耗时、token 数、字节数。StatusLine、错误详情与设置页共用，保证三端一致。
 */
import { getLocale, t } from './i18n'
import { loadCoreMessages } from './messages'

loadCoreMessages()

const SECOND = 1000
/** 少于这么多毫秒用 ms 显示 */
const MS_BELOW = SECOND

const numberFormats = new Map<string, Intl.NumberFormat>()

function numberFormat(key: string, options: Intl.NumberFormatOptions): Intl.NumberFormat {
  const id = `${getLocale()}|${key}`
  let f = numberFormats.get(id)
  if (!f) {
    f = new Intl.NumberFormat(getLocale(), options)
    numberFormats.set(id, f)
  }
  return f
}

/** 1234 → "1,234" */
export function formatNumber(n: number): string {
  return numberFormat('int', { maximumFractionDigits: 0 }).format(n)
}

/** 耗时：< 1 s 显示「850 ms」，否则「1.2 s」 */
export function formatDuration(ms: number): string {
  if (ms < MS_BELOW) return t('common.unit.ms', { n: Math.max(0, Math.round(ms)) })
  const seconds = numberFormat('s', { minimumFractionDigits: 1, maximumFractionDigits: 1 }).format(
    ms / SECOND,
  )
  return t('common.unit.s', { n: seconds })
}

/** token 数（带复数） */
export function formatTokens(count: number): string {
  return t('common.unit.tokens', { count, n: formatNumber(count) })
}

const BYTE_UNITS = ['B', 'KB', 'MB', 'GB'] as const
const KIB = 1024

/** 字数（带复数） */
export function formatChars(count: number): string {
  return t('common.unit.chars', { count, n: formatNumber(count) })
}

/** 条目数（带复数） */
export function formatItems(count: number): string {
  return t('common.unit.items', { count, n: formatNumber(count) })
}

/** 字节数：1536 → "1.5 KB" */
export function formatBytes(bytes: number): string {
  let value = bytes
  let unit = 0
  while (value >= KIB && unit < BYTE_UNITS.length - 1) {
    value /= KIB
    unit++
  }
  const digits = unit === 0 || value >= 10 ? 0 : 1
  const n = numberFormat(`b${digits}`, { maximumFractionDigits: digits }).format(value)
  return `${n} ${BYTE_UNITS[unit]}`
}
