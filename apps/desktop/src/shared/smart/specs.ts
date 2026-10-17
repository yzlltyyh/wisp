/**
 * 智能识别的展示注册表：片段种类（`EntityKind`）与「粘贴为…」变换（`TransformId`）在界面上的图标、文案与顺序。
 * 识别与变换本身都在 Rust（wisp-clipboard 的 smart 模块），这里只负责怎么显示；`satisfies Record<…>` 让 Rust 新增
 * 一种识别 / 变换时，前端在编译期就必须补上它的展示。剪贴板的芯片条、预览里的芯片、「提取」与「粘贴为…」子页，
 * 以及 OCR 视图的芯片条都从这里取，不各自写一份。
 */
import {
  Banknote,
  Braces,
  CalendarClock,
  CaseLower,
  CaseSensitive,
  CaseUpper,
  CreditCard,
  Expand,
  FoldVertical,
  IdCard,
  KeyRound,
  Link,
  Mail,
  Minimize2,
  Network,
  Package,
  Phone,
  Pilcrow,
  Receipt,
  Scissors,
  Shrink,
  Unlink,
  WrapText,
  type LucideIcon,
} from 'lucide-react'
import type { ClipInspect, ClipKind, Entity, EntityKind, TransformId } from '../ipc/types'
import type { MessageKey } from '../i18n/i18n'
import { loadSmartMessages } from './i18n'

// 下面的文案 key 都在 `smart` 命名空间：用到展示注册表的模块（剪贴板、OCR 的芯片条）随它一起拿到文案
loadSmartMessages()

export interface EntitySpec {
  readonly icon: LucideIcon
  /** 种类名（芯片的无障碍名称、子页里的说明） */
  readonly label: MessageKey
  /** 同一条目里多种片段的排列顺序（越小越靠前）：验证码最常用，放最前 */
  readonly order: number
  /** 在这些类型的条目里不显示（链接条目本身就是那个网址，再给一个网址芯片是重复的） */
  readonly hideIn?: readonly ClipKind[]
}

export const ENTITY_SPECS = {
  otp: { icon: KeyRound, label: 'smart.entity.otp', order: 0 },
  phone: { icon: Phone, label: 'smart.entity.phone', order: 1 },
  email: { icon: Mail, label: 'smart.entity.email', order: 2 },
  url: { icon: Link, label: 'smart.entity.url', order: 3, hideIn: ['link'] },
  tracking: { icon: Package, label: 'smart.entity.tracking', order: 4 },
  order: { icon: Receipt, label: 'smart.entity.order', order: 5 },
  amount: { icon: Banknote, label: 'smart.entity.amount', order: 6 },
  datetime: { icon: CalendarClock, label: 'smart.entity.datetime', order: 7 },
  ip: { icon: Network, label: 'smart.entity.ip', order: 8 },
  id_card: { icon: IdCard, label: 'smart.entity.id_card', order: 9 },
  bank_card: { icon: CreditCard, label: 'smart.entity.bank_card', order: 10 },
} as const satisfies Record<EntityKind, EntitySpec>

export interface TransformSpec {
  readonly icon: LucideIcon
  readonly label: MessageKey
  /** 子页里的顺序：结构（合并为一行 / 合并段内换行 / 去空行 / 去空白）→ 大小写 → 全半角 → JSON → 链接 */
  readonly order: number
}

export const TRANSFORM_SPECS = {
  join_lines: { icon: WrapText, label: 'smart.transform.join_lines', order: 0 },
  reflow: { icon: Pilcrow, label: 'smart.transform.reflow', order: 1 },
  remove_blank_lines: {
    icon: FoldVertical,
    label: 'smart.transform.remove_blank_lines',
    order: 2,
  },
  trim: { icon: Scissors, label: 'smart.transform.trim', order: 3 },
  upper: { icon: CaseUpper, label: 'smart.transform.upper', order: 4 },
  lower: { icon: CaseLower, label: 'smart.transform.lower', order: 5 },
  title: { icon: CaseSensitive, label: 'smart.transform.title', order: 6 },
  half_width: { icon: Shrink, label: 'smart.transform.half_width', order: 7 },
  full_width: { icon: Expand, label: 'smart.transform.full_width', order: 8 },
  json_pretty: { icon: Braces, label: 'smart.transform.json_pretty', order: 9 },
  json_minify: { icon: Minimize2, label: 'smart.transform.json_minify', order: 10 },
  strip_tracking: { icon: Unlink, label: 'smart.transform.strip_tracking', order: 11 },
} as const satisfies Record<TransformId, TransformSpec>

/**
 * 要显示的片段：去掉在这类条目里重复的（`hideIn`），按种类的顺序排列（同种按原文位置）；下标与芯片条、「提取」子页
 * 一致。`kind` 省略时（OCR 的纯文字）不去掉任何种类
 */
export function orderEntities(entities: readonly Entity[], kind?: ClipKind): Entity[] {
  return entities
    .filter((e) => !kind || !(ENTITY_SPECS[e.kind] as EntitySpec).hideIn?.includes(kind))
    .map((e, i) => ({ e, i }))
    .sort((a, b) => ENTITY_SPECS[a.e.kind].order - ENTITY_SPECS[b.e.kind].order || a.i - b.i)
    .map(({ e }) => e)
}

/** 变换按子页的顺序排列 */
export function orderTransforms(ids: readonly TransformId[]): TransformId[] {
  return [...ids].sort((a, b) => TRANSFORM_SPECS[a].order - TRANSFORM_SPECS[b].order)
}

/** 没有识别结果（图片、文件，或检视失败 / 结果缺失）：芯片条不出现、「粘贴为…」不列出 */
export const NO_INSPECT: ClipInspect = { entities: [], transforms: [] }

/**
 * 检视结果进入视图前的兜底：传输层给的结果可能缺失（浏览器 mock、测试替身或别的窗口的 harness 没实现这条命令时是
 * undefined）。缺失的部分一律当作「没有片段 / 没有变换」，后续的排序与落 store 不会因为读 undefined 的字段抛错——
 * 那种错误发生在成功回调里，不会流进失败回调，只会变成没人处理的 Promise 拒绝
 */
export function inspectOrEmpty(result: ClipInspect | null | undefined): ClipInspect {
  if (!result) return NO_INSPECT
  return {
    entities: Array.isArray(result.entities) ? result.entities : [],
    transforms: Array.isArray(result.transforms) ? result.transforms : [],
  }
}

/**
 * 芯片上显示的值：证件号、银行卡号只露出首 4 位与末 4 位（中间用 • 代替），粘贴时仍是完整的值。读屏同样只读遮罩后的
 * 写法——旁边有人时不会被念出来
 */
export function entityText(e: Entity): string {
  if (!e.sensitive || e.value.length <= 8) return e.value
  return `${e.value.slice(0, 4)} •••• ${e.value.slice(-4)}`
}
