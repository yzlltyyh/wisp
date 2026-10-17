/**
 * OCR 结果里识别出的片段（电话、验证码、单号…）：在底栏上方排成一行芯片，与剪贴板的芯片条同一个部件与同一份展示
 * 注册表（shared/smart 的 EntityChip.tsx / specs.ts）。识别在 Rust（`text_inspect`），只在识别完成、有文字时进行；
 * 编辑文字之后停下来再重新识别（不逐键请求）。单击粘贴片段、⌥ 单击只复制，由视图决定怎么做。片段的值不写日志。
 * 识别文本的可用变换交给视图（`onInspect`）：排版菜单据此置灰不适用的整理方式，不再为它另请求一次。
 */
import { useEffect, useRef, useState } from 'react'
import { api } from '../../shared/ipc'
import type { Entity, OsKind, TransformId } from '../../shared/ipc/types'
import { useT } from '../../shared/i18n/useT'
import { EntityChip, useChipTip, type ChipMode } from '../../shared/smart/EntityChip'
import { inspectOrEmpty, orderEntities } from '../../shared/smart/specs'
import { warnCode } from './useRun'

/** 编辑停下来多久之后重新识别（识别本身在 Rust 里很快，这里只是不逐键请求） */
const EDIT_SETTLE_MS = 300
/** 更长的文字不识别（OCR 的结果通常只有几百字；与剪贴板一样只看开头的一段没有意义） */
const MAX_INSPECT_CHARS = 16_384

export function OcrChips({
  text,
  raw,
  editing,
  os,
  onPick,
  onInspect,
}: {
  /**
   * 要检视的 OCR 文字，即正文显示的文字：编辑过的以编辑后的为准，否则是排版结果——「合并为一行」会还原断词、把跨行的
   * 片段连起来，芯片粘贴的要与眼前的一致
   */
  text: string
  /**
   * 识别文本：排版菜单要的是它的可用变换（显示的已是某种排版的结果时，那种排版对它往往不再适用，不能因此置灰正在用的
   * 选项）。显示的文字与它不同时另外检视它一次，每次结果只一次
   */
  raw: string
  /** 正文是用户编辑过的文字：停下来再检视；切换排版、放弃修改立即检视 */
  editing: boolean
  os: OsKind
  onPick(entity: Entity, mode: ChipMode): void
  /** 每次检视成功（文字已经变了的那次也报，带着它检视的文字） */
  onInspect?(text: string, transforms: readonly TransformId[]): void
}) {
  const t = useT()
  const tip = useChipTip(os)
  const [found, setFound] = useState<{ text: string; entities: Entity[] } | null>(null)
  /** 已经为它请求过检视的识别文本 */
  const asked = useRef<string | null>(null)
  useEffect(() => {
    /** 检视一段文字、报告可用变换，返回排好序的片段；太长不检视、失败只记错误码，都按没有片段处理 */
    const look = (s: string): Promise<Entity[]> =>
      !s.trim() || s.length > MAX_INSPECT_CHARS
        ? Promise.resolve([])
        : // 用 .then().catch() 而不是 .then(成功, 失败)：成功回调里的错误（结果缺失、排序出错）也要落到同一个失败分支，
          // 不能变成没人处理的拒绝。结果缺失（mock / 别的窗口的测试替身没实现 text_inspect）按没有片段处理
          api
            .textInspect(s)
            .then((r) => {
              const result = inspectOrEmpty(r)
              onInspect?.(s, result.transforms)
              return orderEntities(result.entities)
            })
            .catch((error: unknown) => {
              // 识别失败不打扰用户（芯片只是不出现）
              warnCode('text_inspect')(error)
              return []
            })
    let live = true
    // 第一次（识别刚完成）与切换排版立即请求；编辑停下来再请求
    const id = setTimeout(
      () => {
        if (asked.current !== raw && raw !== text) void look(raw)
        asked.current = raw
        void look(text).then((entities) => live && setFound({ text, entities }))
      },
      found !== null && editing ? EDIT_SETTLE_MS : 0,
    )
    return () => {
      live = false
      clearTimeout(id)
    }
    // found：只用来区分第一次与编辑；raw 只在检视时读取（重新识别期间芯片条不渲染，换了一次结果就是重新挂载）
    // oxlint-disable-next-line react/exhaustive-deps
  }, [text])
  // 编辑中文字变了、新结果还没到：先收起旧的芯片（不让旧片段对着改过的文字）；切换排版只是同一段文字换了换行，旧芯片
  // 留到新结果到达，不先收起再出现
  const entities = found && (found.text === text || !editing) ? found.entities : []
  if (entities.length === 0) return null
  return (
    <div
      className="clip-chips ocr-chips"
      role="toolbar"
      aria-label={t('smart.a11y.chips')}
      // 单击芯片就把片段粘贴回原应用、不需要键盘：按下时面板不成为 key（见 OcrView）
      data-keep-focus=""
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
