/** className 拼接：忽略 false / null / undefined / 空串 */
export function cx(...parts: Array<string | false | null | undefined>): string {
  let out = ''
  for (const p of parts) {
    if (!p) continue
    out = out ? `${out} ${p}` : p
  }
  return out
}
