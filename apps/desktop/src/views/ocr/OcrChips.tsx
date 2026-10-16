/**
 * OCR 结果里识别出的片段（电话、验证码、单号…）：在底栏上方排成一行芯片，与剪贴板的芯片条同一个部件与同一份展示
 * 注册表（shared/smart 的 EntityChip.tsx / specs.ts）。识别在 Rust（`text_inspect`），只在识别完成、有文字时进行；
 * 编辑文字之后停下来再重新识别（不逐键请求）。单击粘贴片段、⌥ 单击只复制，由视图决定怎么做。片段的值不写日志。
 * 检视结果里的可用变换交给视图（`onInspect`）：排版菜单据此置灰不适用的整理方式，不再为它另请求一次。
 */
import { useEffect, useState } from 'react'
import { api, WispIpcError } from '../../shared/ipc'
import type { Entity, OsKind, TransformId } from '../../shared/ipc/types'
import { useT } from '../../shared/i18n/useT'
import { EntityChip, useChipTip, type ChipMode } from '../../shared/smart/EntityChip'
import { inspectOrEmpty, orderEntities } from '../../shared/smart/specs'

/** 编辑停下来多久之后重新识别（识别本身在 Rust 里很快，这里只是不逐键请求） */
const EDIT_SETTLE_MS = 300
/** 更长的文字不识别（OCR 的结果通常只有几百字；与剪贴板一样只看开头的一段没有意义） */
const MAX_INSPECT_CHARS = 16_384

export function OcrChips({
  text,
  os,
  onPick,
  onInspect,
}: {
  /**
   * 要检视的 OCR 文字：编辑过的以编辑后的为准，否则是识别文本——排版只改空白，片段不随排版变化（切换排版时芯片不会
   * 先收起再出现），排版菜单要的也正是识别文本的可用变换
   */
  text: string
  os: OsKind
  onPick(entity: Entity, mode: ChipMode): void
  /** 每次检视成功（文字已经变了的那次也报，带着它检视的文字） */
  onInspect?(text: string, transforms: readonly TransformId[]): void
}) {
  const t = useT()
  const tip = useChipTip(os)
  const [found, setFound] = useState<{ text: string; entities: Entity[] } | null>(null)
  useEffect(() => {
    if (text.trim() === '' || text.length > MAX_INSPECT_CHARS) return
    let live = true
    // 第一次（识别刚完成）立即请求；之后的变化是编辑，停下来再请求
    const delay = found === null ? 0 : EDIT_SETTLE_MS
    const id = setTimeout(() => {
      // 用 .then().catch() 而不是 .then(成功, 失败)：成功回调里的错误（结果缺失、排序出错）也要落到同一个失败分支，
      // 不能变成没人处理的拒绝。结果缺失（mock / 别的窗口的测试替身没实现 text_inspect）按没有片段处理
      api
        .textInspect(text)
        .then((r) => {
          const result = inspectOrEmpty(r)
          onInspect?.(text, result.transforms)
          if (live) setFound({ text, entities: orderEntities(result.entities) })
        })
        .catch((error: unknown) => {
          // 识别失败不打扰用户（芯片只是不出现）；只记错误码
          console.warn(
            `[ocr] text_inspect failed: ${WispIpcError.from('text_inspect', error).code}`,
          )
        })
    }, delay)
    return () => {
      live = false
      clearTimeout(id)
    }
    // found：只用来区分第一次与编辑
    // oxlint-disable-next-line react/exhaustive-deps
  }, [text])
  // 文字变了、新结果还没到：先收起旧的芯片（不让旧片段对着新文字）
  const entities = found && found.text === text ? found.entities : []
  if (entities.length === 0) return null
  return (
    <div
      className="clip-chips ocr-chips"
      role="toolbar"
      aria-label={t('smart.a11y.chips')}
      data-testid="ocr-chips"
    >
      {entities.map((e) => (
        <EntityChip
          key={`${e.kind}-${e.start}`}
          entity={e}
          tip={tip}
          onPick={(mode) => onPick(e, mode)}
        />
      ))}
    </div>
  )
}
