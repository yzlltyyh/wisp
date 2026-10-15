/**
 * 设置写入的小工具：RFC 7396 JSON Merge Patch（与 Rust `settings_patch` 同语义，数组整体替换），
 * 以及从「点路径 + 值」构造补丁。前端只用它做乐观更新，权威结果以 Rust 返回的快照为准。
 */

export type Patch = Record<string, unknown>

function isPlainObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v)
}

/**
 * 乐观更新用的合并：对象逐键合并，数组与标量整体替换。
 *
 * 与 RFC 7396 唯一的差别：`null` 在这里写成 null 而不是删除键。Rust 侧删除键后由宽松反序列化补缺省值，
 * 前端只对「缺省值就是 null」的可选字段（`clipboard.max_age_days`、`defaults.ocr`、`double_tap.key`……）发送 null，
 * 两边的结果一致，而界面读到的是 null 而不是 undefined。
 */
export function mergePatch<T>(target: T, patch: unknown): T {
  if (!isPlainObject(patch)) return structuredClone(patch) as T
  const out: Record<string, unknown> = isPlainObject(target) ? { ...target } : {}
  for (const [key, value] of Object.entries(patch)) {
    out[key] = value === null ? null : mergePatch(out[key], value)
  }
  return out as T
}

/**
 * `at('translate.anchor', 'cursor')` → `{ translate: { anchor: 'cursor' } }`。
 * 注意：Merge Patch 里 `null` 表示删除键——要把可选字段设为 null（例如 `clipboard.max_age_days`），
 * Rust 侧的宽松反序列化会把缺失的可选字段读成默认值 null，语义一致。
 */
export function at(path: string, value: unknown): Patch {
  const keys = path.split('.')
  let out: unknown = value
  for (let i = keys.length - 1; i >= 0; i--) out = { [keys[i] as string]: out }
  return out as Patch
}

/**
 * 从 `base` 里去掉 `remove` 写到的路径（叶子、数组或整个对象），返回剩下的补丁；什么都不剩时返回 null。
 * 用于：直接写入某个路径时，同一路径上还在排队的旧值不能在之后发出、把新值盖回去。
 */
export function subtractPatch(base: Patch, remove: Patch): Patch | null {
  const out: Patch = {}
  for (const [key, value] of Object.entries(base)) {
    if (!(key in remove)) {
      out[key] = value
      continue
    }
    const r = remove[key]
    if (isPlainObject(value) && isPlainObject(r)) {
      const rest = subtractPatch(value, r)
      if (rest) out[key] = rest
    }
    // 其余情况（`remove` 写了叶子 / 数组 / null，或 `base` 这里是叶子）：整个去掉
  }
  return Object.keys(out).length > 0 ? out : null
}
