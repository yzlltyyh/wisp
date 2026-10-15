/**
 * Overlay 的动效令牌：直接读 tokens.css 的 `--dur-*` / `--ease-*`（与 shared/motion/tokens.ts 同值，
 * 并已随 `<html data-reduced-motion>` 改写为减少动效的取值）。Overlay 用 WAAPI，不引入 Motion。
 */
export const token = (name: string): string =>
  getComputedStyle(document.documentElement).getPropertyValue(name).trim()

/**
 * CSS 时长 → 毫秒。注意单位：生产构建的 CSS 压缩会把 `220ms` 改写成 `.22s`，不能直接 parseFloat；
 * 取不到（测试环境没有样式表）时为 0
 */
export function toMs(value: string): number {
  const v = value.trim()
  const n = parseFloat(v)
  if (!Number.isFinite(n)) return 0
  return v.endsWith('s') && !v.endsWith('ms') ? n * 1000 : n
}

/** `--dur-*` → 毫秒 */
export const tokenMs = (name: string): number => toMs(token(name))
