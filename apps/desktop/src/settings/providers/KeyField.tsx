/**
 * API Key 输入：只写不读——界面只显示是否已保存。状态与写入规则在 keyEntry.ts：
 * - 已保存的服务：回车或「保存」写入；已保存时可以更换或移除；
 * - 新建草稿：没有「保存」按钮，Key 随「添加服务 / 保存并继续」一起提交（说明文字写明）；回车 = 测试连接。
 * 凭据存储只能本次会话使用 / 不可用时给出说明（KeychainState）；测试结果是「Key 无效」时就地提示：
 * - 已保存的 Key 被拒绝：保存状态写成「已保存在钥匙串 · 被拒绝」（红色），「更换」变成主要操作；
 * - 还没有 Key 就被拒绝：说明「需要 API Key」，而不是「拒绝了这个 Key」（输入框是空的，看不出指的是哪个 Key）；
 * - 被拒绝时把**整个** Key 字段（标签、输入框、提示）滚入视野，而不是下方的报告。
 */
import { Check, KeyRound } from 'lucide-react'
import { m } from 'motion/react'
import { useEffect, useRef } from 'react'
import { usePlatform } from '../../shared/hooks/usePlatform'
import { useT } from '../../shared/i18n/useT'
import type { KeychainState } from '../../shared/ipc/types'
import { usePreset } from '../../shared/motion/presets'
import { useReducedMotionPref } from '../../shared/motion/useReducedMotionPref'
import { Button, Icon, TextField } from '../../shared/ui'
import type { KeyEntry } from './keyEntry'

export function KeyField({
  entry,
  required,
  keychain,
  focusOnMount = false,
  draftHint,
  onSubmit,
  rejected = false,
  copy,
}: {
  entry: KeyEntry
  required: boolean
  keychain: KeychainState
  /** 新建服务时直接把焦点放进 Key 输入框 */
  focusOnMount?: boolean
  /** 草稿：Key 什么时候保存（「添加服务时…」） */
  draftHint?: string
  /** 草稿里按回车（通常是测试连接） */
  onSubmit?: () => void
  /** 最近一次测试被服务拒绝（401 / 403）：在输入框下就地提示 */
  rejected?: boolean
  /** 换掉「API Key」的叫法（PaddleOCR 叫访问令牌）：输入框的无障碍名称、占位、被拒绝 / 缺少时的提示 */
  copy?: { label: string; placeholder: string; rejected: string; required: string }
}) {
  const t = useT()
  const os = usePlatform()
  const swap = usePreset('iconSwap')
  const reduced = useReducedMotionPref()
  const root = useRef<HTMLDivElement>(null)
  const savedText = t(`settings.providers.key_saved.${os}`)
  /** 被拒绝时有没有一个 Key：已保存的，或输入框里填了的 */
  const hasSome = entry.present || entry.value.trim() !== ''

  // 被拒绝：把整个 Key 字段（标签 + 输入框 + 提示）滚入视野
  useEffect(() => {
    if (!rejected) return
    const field = root.current?.closest<HTMLElement>('.ui-fieldrow') ?? root.current
    field?.scrollIntoView?.({ block: 'nearest', behavior: reduced ? 'auto' : 'smooth' })
  }, [rejected, reduced])

  const hint = entry.error ? (
    <p className="st-field-error" role="alert">
      {entry.error}
    </p>
  ) : rejected ? (
    <p className="st-field-hint" data-tone="danger" data-testid="key-rejected">
      {hasSome
        ? (copy?.rejected ?? t('settings.providers.key_rejected'))
        : (copy?.required ?? t('settings.providers.key_required'))}
    </p>
  ) : keychain !== 'available' ? (
    <p className="st-field-hint" data-tone="warning">
      {t(`settings.providers.keychain_${keychain}`)}
    </p>
  ) : entry.draft && draftHint ? (
    <p className="st-field-hint" data-testid="key-draft-hint">
      {draftHint}
    </p>
  ) : null

  return (
    <div
      ref={root}
      className="st-key"
      data-testid="key-field"
      data-draft={entry.draft || undefined}
      data-rejected={rejected || undefined}
    >
      {entry.editing ? (
        <div className="st-key-edit">
          <TextField
            type="password"
            value={entry.value}
            onChange={entry.setValue}
            onEnter={() => {
              if (entry.draft) onSubmit?.()
              else void entry.commit()
            }}
            placeholder={
              copy?.placeholder ??
              (required
                ? t('settings.providers.key_placeholder')
                : t('settings.providers.key_optional'))
            }
            aria-label={copy?.label ?? t('settings.providers.key')}
            // oxlint-disable-next-line jsx-a11y/no-autofocus -- 刚选了模板新建服务：下一步就是填 Key
            autoFocus={focusOnMount}
            invalid={entry.error !== null || rejected}
            className="st-grow st-mono"
            leading={<Icon icon={KeyRound} size={14} />}
            data-testid="key-input"
          />
          {entry.draft ? null : (
            <>
              <Button
                size="sm"
                variant={entry.value.trim() ? 'primary' : 'secondary'}
                disabled={!entry.value.trim()}
                loading={entry.busy}
                onClick={() => void entry.commit()}
                data-testid="key-save"
              >
                {t('common.action.save')}
              </Button>
              {entry.present ? (
                <Button
                  size="sm"
                  variant="ghost"
                  onClick={() => {
                    entry.setValue('')
                    entry.setEditing(false)
                  }}
                >
                  {t('common.action.cancel')}
                </Button>
              ) : null}
            </>
          )}
        </div>
      ) : (
        <div className="st-key-saved">
          <span className="st-key-dots" aria-hidden="true">
            ••••••••••••
          </span>
          <span className="st-key-status" data-testid="key-saved" title={savedText}>
            {entry.justSaved ? (
              <m.span className="st-key-check" {...swap}>
                <Icon icon={Check} size={12} />
              </m.span>
            ) : null}
            <span className="st-key-status-text">
              {rejected
                ? t('settings.providers.key_saved_rejected', { saved: savedText })
                : savedText}
            </span>
          </span>
          <Button
            size="sm"
            variant={rejected ? 'secondary' : 'ghost'}
            onClick={() => entry.setEditing(true)}
            data-testid="key-change"
          >
            {t('settings.providers.key_change')}
          </Button>
          <Button
            size="sm"
            variant="ghost"
            loading={entry.busy}
            onClick={() => void entry.remove()}
            data-testid="key-remove"
          >
            {t('common.action.remove')}
          </Button>
        </div>
      )}
      {hint}
    </div>
  )
}
