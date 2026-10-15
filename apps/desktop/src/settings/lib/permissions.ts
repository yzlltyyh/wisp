/**
 * 去授权：第一次请求时由系统弹出授权提示（并把 Wisp 加进系统设置的列表）；已经被拒绝过、系统不再弹窗时，
 * 直接打开系统设置的对应面板（x-apple.systempreferences:…）。缺少权限时只给去授权的入口，不报系统错误。
 */
import { api } from '../../shared/ipc'
import type { PermissionKind, PermissionSet } from '../../shared/ipc/types'
import { setPermissions } from '../store'

export async function grantPermission(
  kind: PermissionKind,
  current: PermissionSet | null,
): Promise<void> {
  const state = current?.[kind]
  if (state === 'granted' || state === 'not_applicable') return
  if (state === 'not_determined') {
    try {
      const next = await api.permissionsRequest(kind)
      if (next) setPermissions(next)
      return
    } catch {
      // 请求失败：退回打开系统设置
    }
  }
  await api.permissionsOpen(kind).catch(() => undefined)
}
