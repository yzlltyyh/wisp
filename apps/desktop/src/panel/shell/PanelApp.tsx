/**
 * 面板应用根：MotionRoot + PanelShell。
 * 浏览器 mock 下，场景可以整页替换为组件展示页 / 设置与引导的视觉稿（devHost.setPageOverride），
 * 这一分支在生产包里被 `__WISP_MOCK__` 整段删除。
 */
import { MotionRoot } from '../../shared/motion/MotionRoot'
import { pageOverride } from './devHost'
import { PanelShell } from './PanelShell'

export function PanelApp() {
  const page = __WISP_MOCK__ ? pageOverride() : null
  return <MotionRoot>{page ?? <PanelShell />}</MotionRoot>
}
