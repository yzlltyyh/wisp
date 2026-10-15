/**
 * 设置窗口顶部的全局提示：
 * - 配置文件读取异常（`RuntimeInfo.settings_issue`）：`invalid_json` / `reset_to_default` → `settings.invalid_file`
 *   （另附「已保留上一次的设置」或「已备份并恢复默认」）；`newer_version` → 「配置来自更新的版本」；
 * - 本次会话里可以关掉（记在 store 里：切换分组不会重新出现），问题变化（时间戳不同）时重新出现。
 * 只和某个分组相关的提示（孤立 Key、钥匙串状态）放在该分组页里。
 */
import { FileWarning } from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useT } from '../../shared/i18n/useT'
import { api } from '../../shared/ipc'
import { usePreset } from '../../shared/motion/presets'
import { Banner, Button } from '../../shared/ui'
import { dismissIssue, useRuntime, useSettingsStore } from '../store'

/**
 * 路径只在分隔符之后换行：空格换成不换行空格（「Application Support」不会被拆成两行、看起来像两条路径），
 * 每个「/」「\\」后面插入 <wbr>
 */
export function breakablePath(path: string) {
  const parts = path.replace(/ /g, '\u00a0').split(/(?<=[/\\])/)
  return parts.map((part, i) => (
    // oxlint-disable-next-line react/no-array-index-key -- 片段按位置排列，没有稳定 id
    <span key={i}>
      {part}
      {i < parts.length - 1 ? <wbr /> : null}
    </span>
  ))
}

export function IssueBanners() {
  const t = useT()
  const runtime = useRuntime()
  const dismissed = useSettingsStore((s) => s.dismissedIssueAt)
  const motion = usePreset('rowInsert')
  const issue = runtime?.settings_issue ?? null
  const visible = issue !== null && issue.at_ms !== dismissed
  return (
    <AnimatePresence initial={false}>
      {visible && issue ? (
        <m.div key={issue.at_ms} className="st-global-banner" {...motion}>
          <Banner
            tone="warning"
            icon={FileWarning}
            className="st-banner-top"
            data-testid="settings-issue-banner"
            onDismiss={() => dismissIssue(issue.at_ms)}
            action={
              <Button size="sm" variant="ghost" onClick={() => void api.openPath('config')}>
                {t('settings.issue.open_folder')}
              </Button>
            }
          >
            {issue.kind === 'newer_version' ? (
              <span className="st-issue">
                <b>{t('settings.issue.newer_title')}</b>
                <span>{t('settings.issue.newer_version', { file: issue.file })}</span>
              </span>
            ) : (
              <span className="st-issue">
                <b>{t('settings.invalid_file', { file: issue.file })}</b>
                <span>
                  {issue.kind === 'reset_to_default'
                    ? t('settings.issue.reset_to_default')
                    : t('settings.issue.invalid_json')}
                </span>
                {issue.kind === 'reset_to_default' && issue.backup_path ? (
                  <span className="st-path">{breakablePath(issue.backup_path)}</span>
                ) : null}
              </span>
            )}
          </Banner>
        </m.div>
      ) : null}
    </AnimatePresence>
  )
}
