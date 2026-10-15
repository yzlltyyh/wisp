/**
 * 当前平台：随 `appearance:changed` 更新（mock 切换平台时同样生效）。
 * 平台差异只允许出现在这几处：快捷键显示、修饰键映射、材质、托盘、权限引导。
 */
import type { OsKind } from '../ipc/types'
import { useAppearance } from './useAppearance'

export function usePlatform(): OsKind {
  return useAppearance().os
}
