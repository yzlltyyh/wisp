/**
 * 设置窗口的分组 id：`WindowTarget.settings.section` 的全部取值，顺序即导航顺序。
 *
 * 与 Rust 的 `app::sections::ALL` 逐项一致：`fixtures/consts/SettingsSections.json` 由 Rust 生成
 * （`UPDATE_GOLDEN=1 cargo test -p wisp-desktop --lib app::`），`__tests__/sections.test.ts` 比对两边。设置窗口的路由、
 * 「去设置」按钮（NotReadyCard / ErrorCard 的调用方）与托盘深链只用这里的常量，不手写分组名——拼错时编译失败，
 * 而不是静默落到默认分组。
 */
export const SETTINGS_SECTIONS = [
  'general',
  'hotkeys',
  'providers',
  'translate',
  'ocr',
  'clipboard',
  'appearance',
  'about',
] as const

export type SettingsSection = (typeof SETTINGS_SECTIONS)[number]

/**
 * 分组内的锚点（`WindowTarget.settings.anchor`）：全部合法的「分组 → 锚点」组合。与 Rust 的
 * `app::sections::ANCHORS` 逐项一致（`fixtures/consts/SettingsAnchors.json` 由 Rust 生成，`sections.test.ts` 比对）；
 * Rust 拒绝未登记的组合（`invalid_argument`）。`ocr_history`：OCR 面板 ⋯ 菜单的「识别历史」直达设置 › OCR 的历史列表
 */
export const SETTINGS_ANCHORS = {
  ocr: ['ocr_history'],
} as const satisfies Partial<Record<SettingsSection, readonly string[]>>

type AnchorMap = typeof SETTINGS_ANCHORS
export type SettingsAnchor = AnchorMap[keyof AnchorMap][number]

/** 运行期的锚点（`window:navigate`、`boot.route`）是否属于该分组；未知时调用方停在分组顶部 */
export function isSettingsAnchor(
  section: SettingsSection,
  value: unknown,
): value is SettingsAnchor {
  const list = (SETTINGS_ANCHORS as Partial<Record<SettingsSection, readonly string[]>>)[section]
  return typeof value === 'string' && (list ?? []).includes(value)
}

/** 运行期的值（`window:navigate`、`boot.route`）是否是已知分组；未知时调用方退回第一个分组 */
export function isSettingsSection(value: unknown): value is SettingsSection {
  return (SETTINGS_SECTIONS as readonly unknown[]).includes(value)
}
