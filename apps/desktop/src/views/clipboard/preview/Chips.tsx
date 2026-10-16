/**
 * 识别出的片段（芯片）：列表底部的芯片条（光标所在条目）与预览里的芯片组。识别在 Rust，这里只按展示注册表
 * （shared/smart/specs.ts）画出来（芯片本身见 shared/smart/EntityChip.tsx）；点芯片 = 粘贴这个片段（⌥ 点击
 * 只复制），都经操作注册表的 paste_fragment。
 *
 * - 芯片条停靠在卡片底部的浮层里（与确认条、轻提示同一层），不改变行高：列表末尾始终留着它的高度（CHIP_STRIP_INSET），
 *   光标移到最后一行时不会被它盖住；只在光标条目有片段时出现，确认条 / 命名条 / 全文预览打开时让位；
 * - ⌘E 把键盘焦点交给芯片条（焦点仍在搜索框里，←/→ 由芯片条的键位层处理）：当前芯片高亮并滚进可见范围，搜索框的
 *   aria-activedescendant 指向它；↑↓ 换条目即退出；
 * - 片段的值不写日志。
 */
import { AnimatePresence, m } from 'motion/react'
import { useLayoutEffect, useRef } from 'react'
import { useStore } from 'zustand'
import { useT } from '../../../shared/i18n/useT'
import { usePreset } from '../../../shared/motion/presets'
import { EntityChip, useChipTip } from '../../../shared/smart/EntityChip'
import type { ClipRunner } from '../actions'
import type { ClipStore } from '../store'

/** 芯片条里芯片的 DOM id（搜索框的 aria-activedescendant 指向有键盘焦点的那一个） */
export function chipDomId(index: number): string {
  return `clip-chip-${index}`
}

export interface ChipStripProps {
  store: ClipStore
  runner: ClipRunner
  /** 确认条 / 命名条 / 全文预览打开时让位 */
  hidden: boolean
}

/** 列表底部的芯片条（光标所在条目） */
export function ChipStrip({ store, runner, hidden }: ChipStripProps) {
  const t = useT()
  const motion = usePreset('toast')
  const tip = useChipTip(runner.os)
  const inspect = useStore(store, (s) => s.inspect)
  const selectedId = useStore(store, (s) => s.selectedId)
  const chip = useStore(store, (s) => s.chip)
  const entities = inspect && inspect.id === selectedId ? inspect.result.entities : []
  const box = useRef<HTMLDivElement>(null)
  // 键盘移动时把当前芯片滚进可见范围（芯片多时横向滚动）
  useLayoutEffect(() => {
    if (chip === null) return
    box.current
      ?.querySelector<HTMLElement>(`#${chipDomId(chip)}`)
      ?.scrollIntoView?.({ block: 'nearest', inline: 'nearest' })
  }, [chip])
  const show = !hidden && entities.length > 0
  return (
    <div className="clip-chips-dock">
      <AnimatePresence>
        {show ? (
          <m.div
            key="chips"
            ref={box}
            className="clip-chips clip-chips-float"
            role="toolbar"
            aria-label={t('smart.a11y.chips')}
            data-focus={chip !== null || undefined}
            data-testid="clip-chips"
            {...motion}
          >
            {entities.map((e, index) => (
              <EntityChip
                key={`${e.kind}-${e.start}`}
                entity={e}
                id={chipDomId(index)}
                active={chip === index}
                tip={tip}
                onPick={(mode) =>
                  runner.run('paste_fragment', runner.ctx('pointer'), { index, mode })
                }
              />
            ))}
          </m.div>
        ) : null}
      </AnimatePresence>
    </div>
  )
}

/**
 * 预览（悬停卡片 / 全文预览）里的芯片组：作用于预览的那一条。预览的条目就是光标所在的条目（悬停即选中、Space 预览
 * 选中项），片段来自芯片条的同一份检视，不另外请求；粘贴同样经注册表
 */
export function EntityChips({
  store,
  runner,
  id,
}: {
  store: ClipStore
  runner: ClipRunner
  /** 预览的条目 */
  id: number
}) {
  const t = useT()
  const tip = useChipTip(runner.os)
  const inspect = useStore(store, (s) => s.inspect)
  const entities = inspect?.id === id ? inspect.result.entities : []
  if (entities.length === 0) return null
  return (
    <div
      className="clip-chips clip-chips-inline"
      role="toolbar"
      aria-label={t('smart.a11y.chips')}
      data-testid="clip-preview-chips"
    >
      {entities.map((e, index) => (
        <EntityChip
          key={`${e.kind}-${e.start}`}
          entity={e}
          tip={tip}
          onPick={(mode) =>
            runner.run('paste_fragment', runner.ctx('pointer', id), { index, mode })
          }
        />
      ))}
    </div>
  )
}
