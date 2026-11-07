/**
 * 头部 ✕ 左侧的「展开阅读 / 收起」按钮（翻译 / OCR 视图，面板宿主、这次显示能展开时）：Maximize2 / Minimize2，tooltip
 * 带快捷键。点下即由外壳淡出内容、在透明帧切换版式（见 PanelShell）；展开时面板取得键盘焦点，所以 OCR 视图里它不带
 * `data-keep-focus`
 */
import { Maximize2, Minimize2 } from 'lucide-react'
import { useT } from '../shared/i18n/useT'
import { IconButton } from '../shared/ui'
import { READING_SHORTCUT } from './reading'

export function ReadingButton({
  reading,
  onToggle,
  'data-testid': testId,
}: {
  /** 正在展开阅读 */
  reading: boolean
  onToggle: () => void
  'data-testid'?: string
}) {
  const t = useT()
  return (
    <IconButton
      icon={reading ? Minimize2 : Maximize2}
      label={t(reading ? 'common.action.exit_reading_view' : 'common.action.reading_view')}
      shortcut={READING_SHORTCUT}
      onClick={onToggle}
      data-reading={reading || undefined}
      data-testid={testId}
    />
  )
}
