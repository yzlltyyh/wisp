/**
 * 快捷键「测试」的窗口级状态。Rust 只有一个测试槽位（`hotkey_test_begin` 覆盖上一个），所以同一时刻只有一个
 * 目标在测试：在 B 行开始测试时，A 行立即回到空闲（它的倒计时与订阅一并取消）；`hotkey_test_end` 只由当前持有者发出，
 * A 的计时器不会提前结束 B 的测试。
 *
 * 结果和测试时的组合绑在一起：这一行的绑定改了（重新录制、恢复默认、Rust 改用了备选键），旧结果就不再显示——否则
 * 「已收到」挂在新组合旁边，看上去像新组合已经测过了。「已收到」停留 `timing.toastViewHold` 后自动淡出、回到原来的说明；
 * 「超时」（需要用户换键）留到绑定改变或下一次测试。
 */
import { useSyncExternalStore } from 'react'
import { api, on } from '../../shared/ipc'
import type { HotkeyTarget } from '../../shared/ipc/types'
import { ms, timing } from '../../shared/motion/tokens'

/** 测试按钮的等待时间（10 s；不是动效时长） */
export const TEST_TIMEOUT_MS = 10_000
const SECOND_MS = 1000

export type TestPhase =
  | { phase: 'idle' }
  | { phase: 'waiting'; left: number }
  | { phase: 'received' }
  | { phase: 'timeout' }

interface Current {
  target: HotkeyTarget
  /** 开始测试时这一行生效的组合：绑定变了，结果就作废 */
  combo: string | null
  state: Exclude<TestPhase, { phase: 'idle' }>
}

export function sameTarget(a: HotkeyTarget, b: HotkeyTarget): boolean {
  if (a.kind === 'action' && b.kind === 'action') return a.action === b.action
  if (a.kind === 'style' && b.kind === 'style') return a.style_id === b.style_id
  return false
}

let current: Current | null = null
/** 当前测试的计时器与事件订阅；`round` 让订阅完成之前就被取消的那一轮作废 */
let timers: Array<ReturnType<typeof setTimeout>> = []
let unlisten: (() => void) | null = null
let round = 0
const listeners = new Set<() => void>()

function emit(next: Current | null): void {
  current = next
  for (const l of listeners) l()
}

function clearHandles(): void {
  for (const id of timers) clearTimeout(id)
  timers = []
  unlisten?.()
  unlisten = null
  round += 1
}

/** 结束当前测试（收到 / 超时 / 取消），只有这里调用 hotkey_test_end */
function finish(state: Current['state'] | null): void {
  const was = current
  clearHandles()
  const next = state && was ? { target: was.target, combo: was.combo, state } : null
  emit(next)
  void api.hotkeyTestEnd().catch(() => undefined)
  // 「已收到」只是确认：停留一会儿后回到原来的说明（停留时长来自动效令牌）
  if (next?.state.phase === 'received') {
    timers.push(
      setTimeout(() => {
        if (current === next) emit(null)
      }, ms(timing.toastViewHold)),
    )
  }
}

/** 开始测试某个目标；之前在测的目标回到空闲 */
export async function startHotkeyTest(target: HotkeyTarget, combo: string | null): Promise<void> {
  clearHandles()
  const mine = round
  const total = TEST_TIMEOUT_MS / SECOND_MS
  emit({ target, combo, state: { phase: 'waiting', left: total } })
  const un = await on('hotkey:test_received', (p) => {
    if (current && sameTarget(p.target, target)) finish({ phase: 'received' })
  })
  if (mine !== round) {
    un()
    return
  }
  unlisten = un
  for (let s = 1; s <= total; s++) {
    timers.push(
      setTimeout(() => {
        if (s === total) finish({ phase: 'timeout' })
        else emit({ target, combo, state: { phase: 'waiting', left: total - s } })
      }, s * SECOND_MS),
    )
  }
  try {
    await api.hotkeyTestBegin(target)
  } catch (error) {
    if (mine === round) {
      clearHandles()
      emit(null)
    }
    throw error
  }
}

/** 取消正在进行的测试（只取消这个目标的） */
export function cancelHotkeyTest(target: HotkeyTarget): void {
  if (current && sameTarget(current.target, target) && current.state.phase === 'waiting')
    finish(null)
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

/** 当前绑定已经不是测试时的组合：结束这一轮（等待中的取消、结果清掉） */
export function invalidateHotkeyTest(target: HotkeyTarget, combo: string | null): void {
  if (!current || !sameTarget(current.target, target) || current.combo === combo) return
  if (current.state.phase === 'waiting') finish(null)
  else emit(null)
}

/** 某个目标的测试状态；`combo` 是这一行当前生效的组合，和测试时不同就当作没有测试过 */
export function useHotkeyTestState(target: HotkeyTarget, combo: string | null): TestPhase {
  const snap = useSyncExternalStore(
    subscribe,
    () => current,
    () => current,
  )
  if (!snap || !sameTarget(snap.target, target) || snap.combo !== combo) return { phase: 'idle' }
  return snap.state
}

/** 测试用 */
export function resetHotkeyTest(): void {
  clearHandles()
  current = null
}
