/**
 * 引导窗口里的全局动作：引导窗口在前台时，来自输入设备的**每个**动作（热键、浮标、双击修饰键、
 * 鼠标键）都被 Rust 转成 `onboarding:hotkey`、不再执行，由页面自己 `trigger_action(action, origin: onboarding, text)`：
 * 划词翻译用 `window.getSelection()` 的文本（Wisp 自己的窗口不走系统取词），其他动作原样转发。
 *
 * 订阅放在窗口级（`startOnboardingActions`，OnboardingApp 挂载时启动）：不论停在哪一步，按快捷键都有反应；
 * 「试一试」步骤订阅执行结果来打勾。
 *
 * 转发来的划词翻译**没有选中文字**时不能发 `text: null`：Rust 会把它当成普通热键，对 Wisp 自己的引导窗口做系统取词
 * （正是要避免的路径），结果多半是空取词再落到输入模式。所以：「试一试」这一步登记的示例段落优先（与「直接试试」
 * 一致），其他步骤改为打开输入翻译。
 */
import { api, on } from '../shared/ipc'
import type { HotkeyAction } from '../shared/ipc/types'
import { ipcErrorText } from '../settings/lib/errors'
import { notify } from '../settings/store'

type Listener = (action: HotkeyAction) => void
const listeners = new Set<Listener>()
/** 「试一试」步骤登记的示例段落：转发来的划词翻译没有选中文字时用它 */
let sampleText: string | null = null

/** 登记 / 清除示例段落（「试一试」挂载与卸载时调用） */
export function setOnboardingSample(text: string | null): void {
  sampleText = text
}

/** 当前窗口里选中的文字 */
function selectionText(): string | null {
  const text = window.getSelection()?.toString().trim() ?? ''
  return text ? text : null
}

/**
 * 执行一个动作；`fallbackText`：划词翻译且没有选中任何文字时用它（「直接试试」按钮传示例段落）。
 * 成功后通知订阅者，返回是否成功
 */
export async function runOnboardingAction(
  action: HotkeyAction,
  fallbackText: string | null = null,
): Promise<boolean> {
  let run: HotkeyAction = action
  let text: string | null = null
  if (action === 'translate_selection') {
    text = selectionText() ?? fallbackText ?? sampleText
    // 没有任何文字可译：打开输入翻译，而不是让 Rust 对引导窗口自己做系统取词
    if (text === null) run = 'translate_input'
  }
  try {
    await api.triggerAction(run, 'onboarding', text)
  } catch (error) {
    notify(ipcErrorText(error, 'trigger_action'), 'danger')
    return false
  }
  for (const l of listeners) l(run)
  return true
}

/** 订阅执行成功的动作（「试一试」打勾） */
export function onOnboardingAction(listener: Listener): () => void {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

/** 窗口级订阅 `onboarding:hotkey`；返回取消函数（可在订阅完成前调用） */
export function startOnboardingActions(): () => void {
  let disposed = false
  let unlisten: (() => void) | null = null
  void on('onboarding:hotkey', (p) => void runOnboardingAction(p.action)).then((un) => {
    if (disposed) un()
    else unlisten = un
  })
  return () => {
    disposed = true
    unlisten?.()
  }
}
