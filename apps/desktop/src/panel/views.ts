/**
 * 面板视图注册表。四个主视图与 toast 视图都**静态导入**，保证首屏没有请求瀑布；
 * 各视图内部的非首屏部分（例如剪贴板预览）自行 `lazy()`。
 */
import type { PanelView } from '../shared/ipc/types'
import { ClipboardView } from '../views/clipboard'
import { InputView } from '../views/input'
import { OcrView } from '../views/ocr'
import { ToastView } from '../views/toast'
import { TranslateView } from '../views/translate'
import type { ViewDefinition, ViewRegistry } from './types'

export const views: ViewRegistry = {
  translate: {
    view: 'translate',
    Component: TranslateView,
    estimatedHeight: 120,
    takesFocus: false,
  },
  ocr: { view: 'ocr', Component: OcrView, estimatedHeight: 220, takesFocus: false },
  // 剪贴板高度在打开时取 clamp(内容, 240, min(560, 工作区 × 0.6)) 并锁定；这里给下限
  clipboard: {
    view: 'clipboard',
    Component: ClipboardView,
    estimatedHeight: 240,
    takesFocus: true,
  },
  input: { view: 'input', Component: InputView, estimatedHeight: 168, takesFocus: true },
  toast: { view: 'toast', Component: ToastView, estimatedHeight: 40, takesFocus: false },
}

export function viewOf<V extends PanelView>(view: V): ViewDefinition<V> {
  return views[view] as ViewDefinition<V>
}
