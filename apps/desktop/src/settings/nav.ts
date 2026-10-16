/**
 * 设置分组的导航元数据。分组 id 与顺序只来自 `SETTINGS_SECTIONS`，这里只补图标与文案 key。
 */
import {
  ClipboardList,
  Cloud,
  HardDriveDownload,
  Info,
  Keyboard,
  Languages,
  Palette,
  ScanText,
  Server,
  Settings2,
  type LucideIcon,
} from 'lucide-react'
import { SETTINGS_SECTIONS, type SettingsSection } from '../shared/ipc'

export const SECTION_ICONS: Record<SettingsSection, LucideIcon> = {
  general: Settings2,
  hotkeys: Keyboard,
  providers: Server,
  offline: HardDriveDownload,
  translate: Languages,
  ocr: ScanText,
  clipboard: ClipboardList,
  appearance: Palette,
  sync: Cloud,
  about: Info,
}

export const SECTIONS: readonly SettingsSection[] = SETTINGS_SECTIONS
