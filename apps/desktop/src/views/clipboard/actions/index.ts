/**
 * 剪贴板的操作注册表（见 registry.ts 的文件头）。新增一个操作 = 在 `ActionArgs` 里加 id、在注册表里写一项；
 * 菜单、键位表、提示条、预览按钮与悬浮窗工具栏随之出现，不需要另外接线。
 */
export {
  actionEntries,
  applies,
  displayCombo,
  displayCombos,
  hintItems,
  KEY_SCOPES,
  keyBindings,
  menuEntries,
  nthShortcut,
  subPage,
  toolbarButtons,
  type PageChrome,
} from './derive'
export { ACTIONS, actionById } from './registry'
export { createClipRunner, useActionKeys, useClipRunner, type ClipRunner } from './runner'
export type {
  ActionArgs,
  ActionCtx,
  ActionEntry,
  ActionEnv,
  ActionSource,
  ClipAction,
  ClipActionId,
  ClipKeyLayer,
  ClipViewUi,
  HintItem,
  HintMode,
  KeySpec,
  MenuSurface,
  PageItem,
  PreviewLocal,
} from './types'
export { pageItem } from './types'
