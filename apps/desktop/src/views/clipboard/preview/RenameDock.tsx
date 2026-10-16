/**
 * 收藏的命名条：停靠在卡片底部（与确认条同一个停靠位、同一套 confirmIn 展开），输入框自动获得焦点。↩ 保存、Esc 取消
 * 由注册表的 rename 键位层处理（模态：列表的快捷键在输入期间不生效），按钮同样执行这两个操作。名称留空 = 去掉名称；
 * 命名即收藏（Rust 同时设为收藏）。草稿放在 store 里，↩ 的操作读它提交。
 */
import { AnimatePresence, m } from 'motion/react'
import { useEffect, useRef } from 'react'
import { useStore } from 'zustand'
import { useT } from '../../../shared/i18n/useT'
import { usePreset } from '../../../shared/motion/presets'
import { Button, TextField } from '../../../shared/ui'
import type { ClipRunner } from '../actions'
import type { ClipCommands } from '../useClipboard'
import type { ClipStore } from '../store'

/** 与 Rust 的名称上限一致（`wisp_clipboard::MAX_NAME_CHARS`）：输入框直接截断，提交时不会被拒 */
const MAX_NAME_CHARS = 64

export function RenameDock({
  store,
  cmd,
  runner,
}: {
  store: ClipStore
  cmd: ClipCommands
  runner: ClipRunner
}) {
  const t = useT()
  const motion = usePreset('confirmIn')
  const rename = useStore(store, (s) => s.rename)
  const input = useRef<HTMLInputElement>(null)
  const open = rename !== null
  const id = rename?.id ?? null
  // 打开（或换了一条）时聚焦并全选：直接打字就是新名称
  useEffect(() => {
    if (id === null) return
    const el = input.current
    el?.focus({ preventScroll: true })
    el?.select()
  }, [id])
  return (
    <AnimatePresence initial={false}>
      {open ? (
        <m.div
          key="rename"
          className="ui-confirm-clip clip-rename"
          data-testid="clip-rename"
          {...motion}
        >
          <div className="clip-rename-row">
            <TextField
              ref={input}
              value={rename.draft}
              maxLength={MAX_NAME_CHARS}
              onChange={cmd.setRenameDraft}
              placeholder={t('clipboard.rename.placeholder')}
              aria-label={t('clipboard.a11y.rename')}
              aria-description={t('clipboard.rename.hint')}
              data-testid="clip-rename-input"
            />
            <Button
              size="sm"
              variant="ghost"
              onClick={() => runner.run('rename_cancel', runner.ctx('button'))}
            >
              {t('common.action.cancel')}
            </Button>
            <Button
              size="sm"
              variant="primary"
              onClick={() => runner.run('rename_commit', runner.ctx('button'))}
              data-testid="clip-rename-save"
            >
              {t('common.action.save')}
            </Button>
          </div>
        </m.div>
      ) : null}
    </AnimatePresence>
  )
}
