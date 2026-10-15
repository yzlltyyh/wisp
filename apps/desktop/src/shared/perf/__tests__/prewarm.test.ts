/**
 * 视图预热队列：
 * - 窗口隐藏之后按优先级逐个执行，每个一个宏任务；
 * - 队列没做完时窗口又显示了（例如空闲卸载后冷唤出、第一次隐藏后马上又唤出）：剩下的不在显示期间执行，等下一次
 *   隐藏再接着做；
 * - 已经开始过之后、窗口显示着时登记的预热：等下一次隐藏执行（原来永远不会执行）。
 */
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { boot } from '../../ipc'
import { resetPrewarmForTest, schedulePrewarm } from '../prewarm'

const html = document.documentElement
const savedLabel = boot.label

/** 等若干个宏任务（队列每一步一个 MessageChannel 任务） */
async function tasks(n: number): Promise<void> {
  for (let i = 0; i < n; i++) {
    // oxlint-disable-next-line no-await-in-loop -- 依次让出
    await new Promise<void>((resolve) => setTimeout(resolve, 0))
  }
}

beforeEach(() => {
  resetPrewarmForTest()
  ;(boot as { label: string }).label = 'panel'
  html.removeAttribute('data-hidden')
})

afterEach(() => {
  resetPrewarmForTest()
  ;(boot as { label: string }).label = savedLabel
  html.removeAttribute('data-hidden')
})

describe('预热队列', () => {
  it('窗口隐藏之后按优先级逐个执行（与登记顺序无关）', async () => {
    const ran: string[] = []
    schedulePrewarm(() => ran.push('clipboard'), 2)
    schedulePrewarm(() => ran.push('input'), 0)
    schedulePrewarm(() => ran.push('translate'), 1)
    await tasks(3)
    expect(ran).toEqual([])
    html.setAttribute('data-hidden', '')
    await tasks(8)
    expect(ran).toEqual(['input', 'translate', 'clipboard'])
  })

  it('队列中途窗口又显示了（prepare）：剩下的不执行，等下一次隐藏再接着做', async () => {
    const ran: string[] = []
    html.setAttribute('data-hidden', '')
    schedulePrewarm(() => {
      ran.push('input')
      // 这一项执行期间 prepare 到达：外壳去掉 data-hidden，窗口即将显示
      html.removeAttribute('data-hidden')
    }, 0)
    schedulePrewarm(() => ran.push('translate'), 1)
    schedulePrewarm(() => ran.push('clipboard'), 2)
    await tasks(8)
    // 显示期间没有任何预热渲染
    expect(ran).toEqual(['input'])
    html.setAttribute('data-hidden', '')
    await tasks(8)
    expect(ran).toEqual(['input', 'translate', 'clipboard'])
  })

  it('已经开始过之后、窗口显示着时登记的预热：等下一次隐藏执行', async () => {
    const ran: string[] = []
    html.setAttribute('data-hidden', '')
    schedulePrewarm(() => ran.push('input'), 0)
    await tasks(4)
    expect(ran).toEqual(['input'])
    html.removeAttribute('data-hidden')
    schedulePrewarm(() => ran.push('late'), 1)
    await tasks(4)
    expect(ran).toEqual(['input'])
    html.setAttribute('data-hidden', '')
    await tasks(4)
    expect(ran).toEqual(['input', 'late'])
  })

  it('不是面板窗口：不登记', async () => {
    ;(boot as { label: string }).label = 'settings'
    const ran: string[] = []
    html.setAttribute('data-hidden', '')
    schedulePrewarm(() => ran.push('x'), 0)
    await tasks(4)
    expect(ran).toEqual([])
  })
})
