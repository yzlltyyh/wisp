/**
 * 颜色写法的换算（外观页的自定义取色器只接受 `#rrggbb`）。
 */
const HEX = /^#[0-9a-f]{6}$/i
const HEX_RADIX = 16
const BYTE = 255

/** `#rgb` / `#rrggbb` / `rgb(r g b)` / `rgb(r, g, b)` → `#rrggbb`（原生取色器只接受这种写法）；认不出时返回空串 */
export function hexColor(value: string): string {
  const v = value.trim()
  if (HEX.test(v)) return v
  const short = /^#([0-9a-f])([0-9a-f])([0-9a-f])$/i.exec(v)
  if (short) return `#${short[1]}${short[1]}${short[2]}${short[2]}${short[3]}${short[3]}`
  const rgb = /^rgba?\(\s*(\d+)[\s,]+(\d+)[\s,]+(\d+)/i.exec(v)
  if (!rgb) return ''
  const hex = rgb
    .slice(1, 4)
    .map((n) => Math.min(BYTE, Number(n)).toString(HEX_RADIX).padStart(2, '0'))
    .join('')
  return `#${hex}`
}
