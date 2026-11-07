/**
 * 菜单的浏览器级用例（真实引擎里的滚动、排版与指针——jsdom 复现不了）。运行方式见 apps/desktop/playwright.config.ts。
 * - 放不下、在菜单里滚动的菜单：滚轮、直接设 scrollTop（等同拖动滚动条）之后不弹回顶部；End 把最后一项滚进视野；
 *   悬停改变高亮不滚动菜单。设置 › OCR 的识别模型下拉（Menu）与面板 OCR「视觉模型 ›」页（PagedMenu）各一条；
 * - 分页菜单换页的过程中与换页之后都不出横向滚动条；
 * - 指针停在「›」项上片刻就进入子页（剪贴板「粘贴为」、OCR「视觉模型」），快速经过不进入；悬停进入之后在原处点击、
 *   抖动几个像素再点击都被吞掉（不粘贴、不重新识别、菜单仍开着），离开原处之后照常选择；
 * - 菜单开着时，用来关闭菜单的那次点外点击只关菜单，不会同时粘贴下面那一行（分页菜单与 ⋯ 菜单）。
 */
import { expect, test, type Locator, type Page } from '@playwright/test'

test.use({ viewport: { width: 1280, height: 800 } })

type Call = [string, unknown]

/** 打开页面并记录 mock 的 IPC 调用（`ipc_log=1` 时 mock 路由逐条 console.debug） */
async function open(
  page: Page,
  path: string,
  params: Record<string, string> = {},
): Promise<Call[]> {
  const calls: Call[] = []
  page.on('console', (msg) => {
    if (!msg.text().startsWith('[mock ipc]')) return
    void Promise.all(msg.args().map((a) => a.jsonValue())).then(
      (args: unknown[]) => calls.push([String(args[1]), args[2]]),
      // 用例结束后才到的日志：忽略
      () => undefined,
    )
  })
  const q = new URLSearchParams({ os: 'macos', lang: 'zh-CN', hud: '0', ipc_log: '1', ...params })
  await page.goto(`${path}${path.includes('?') ? '&' : '?'}${q.toString()}`)
  await page.waitForSelector('html[data-settled]')
  return calls
}

const count = (calls: Call[], ...cmds: string[]) => calls.filter(([c]) => cmds.includes(c)).length

/** 等几帧（rAF）：浮层的重新定位排在滚动之后的下一帧 */
async function frames(page: Page, n = 3): Promise<void> {
  await page.evaluate(
    (left) =>
      new Promise<void>((resolve) => {
        let rest = left
        const tick = () => (--rest <= 0 ? resolve() : requestAnimationFrame(tick))
        requestAnimationFrame(tick)
      }),
    n,
  )
}

async function center(el: Locator): Promise<{ x: number; y: number }> {
  const b = await el.boundingBox()
  if (!b) throw new Error('元素不可见')
  return { x: b.x + Math.min(40, b.width / 2), y: b.y + b.height / 2 }
}

/**
 * 溢出的菜单可以滚动：滚轮、设 scrollTop 之后不弹回；悬停改变高亮不滚动菜单；End 把最后一项滚进视野。
 * `items`：当前页的菜单项（分页菜单换页时旧页还在淡出，只取当前页）
 */
async function expectScrollable(page: Page, menu: Locator, items: string): Promise<void> {
  const top = () => menu.evaluate((el) => el.scrollTop)
  expect(await menu.evaluate((el) => el.scrollHeight - el.clientHeight)).toBeGreaterThan(40)
  // 滚轮：滚动之后的下一帧不会被夹回顶部
  await menu.evaluate((el) => {
    el.scrollTop = 0
  })
  const box = await menu.boundingBox()
  if (!box) throw new Error('菜单不可见')
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2, { steps: 2 })
  await page.mouse.wheel(0, 120)
  await expect.poll(top).toBeGreaterThan(0)
  await frames(page)
  await page.waitForTimeout(250)
  expect(await top()).toBeGreaterThan(0)
  // 直接设 scrollTop（与拖动滚动条同一条路径）：不弹回
  await menu.evaluate((el) => {
    el.scrollTop = 30
  })
  await frames(page)
  await page.waitForTimeout(150)
  expect(await top()).toBe(30)

  // 悬停改变高亮不滚动菜单：指针移到底部只露出一点的那一项上
  const pt = await menu.evaluate((el, sel) => {
    el.scrollTop = 0
    const m = el.getBoundingClientRect()
    const list = [...el.querySelectorAll<HTMLElement>(sel)]
    const it = list.find((n) => n.getBoundingClientRect().bottom > m.bottom - 1)
    if (!it) return null
    // 露出的部分太少（上一项恰好铺到底）：往下滚一点，让它露出 10 px
    const r0 = it.getBoundingClientRect()
    if (r0.top > m.bottom - 10) el.scrollTop = Math.ceil(r0.top - m.bottom + 10)
    const r = it.getBoundingClientRect()
    return { x: r.left + Math.min(40, r.width / 2), y: r.top + 4, id: it.id, scroll: el.scrollTop }
  }, items)
  expect(pt).not.toBeNull()
  if (!pt) return
  await frames(page)
  await page.mouse.move(pt.x, pt.y - 1)
  await page.mouse.move(pt.x, pt.y, { steps: 2 })
  await expect(menu.locator(`[id="${pt.id}"]`)).toHaveAttribute('data-active', 'true')
  await frames(page)
  await page.waitForTimeout(150)
  expect(await top()).toBe(pt.scroll)

  // End：最后一项完整地滚进视野
  await page.keyboard.press('End')
  await frames(page)
  await page.waitForTimeout(150)
  const last = await menu.evaluate((el, sel) => {
    const m = el.getBoundingClientRect()
    const r = [...el.querySelectorAll<HTMLElement>(sel)].at(-1)?.getBoundingClientRect()
    return r ? { above: r.top - m.top, below: m.bottom - r.bottom } : null
  }, items)
  expect(last).not.toBeNull()
  expect(last?.above ?? -1).toBeGreaterThanOrEqual(-1)
  expect(last?.below ?? -1).toBeGreaterThanOrEqual(-1)
}

test('设置 › OCR 的识别模型下拉（很多模型）：滚轮 / 拖动滚动条之后不弹回顶部，End 后最后一项可见，悬停高亮不滚动菜单', async ({
  page,
}) => {
  // 设置窗口的最小尺寸附近：下拉放不下，要在菜单里滚动
  await page.setViewportSize({ width: 760, height: 480 })
  await open(page, '/settings.html?scenario=settings.ocr-many-models', { os: 'windows' })
  await page.getByTestId('ocr-model-select').click()
  const menu = page.getByTestId('ocr-model-select-menu')
  await expect(menu).toBeFocused()
  await page.waitForTimeout(300)
  await expectScrollable(page, menu, '[role^="menuitem"]')
})

test('设置 › OCR 的识别模型下拉：键盘停在最后一项、Esc 关闭之后再打开，勾选的模型仍在可视区里', async ({
  page,
}) => {
  await page.setViewportSize({ width: 760, height: 480 })
  await open(page, '/settings.html?scenario=settings.ocr-many-models', { os: 'windows' })
  const trigger = page.getByTestId('ocr-model-select')
  const menu = page.getByTestId('ocr-model-select-menu')
  /** 勾选项与高亮项是否完整地在菜单的可视区里 */
  const shown = () =>
    menu.evaluate((el) => {
      const m = el.getBoundingClientRect()
      const inView = (n: Element | null) => {
        if (!n) return false
        const r = n.getBoundingClientRect()
        return r.top >= m.top - 1 && r.bottom <= m.bottom + 1
      }
      const checked = el.querySelector('[aria-checked="true"]')
      const active = el.querySelector('[data-active]')
      return { checked: inView(checked), active: inView(active), same: checked === active }
    })
  await trigger.click()
  await expect(menu).toBeFocused()
  await page.waitForTimeout(300)
  expect(await shown()).toEqual({ checked: true, active: true, same: true })
  await page.keyboard.press('End')
  await frames(page)
  expect(await menu.evaluate((el) => el.scrollTop)).toBeGreaterThan(0)
  await page.keyboard.press('Escape')
  await expect(menu).toHaveCount(0)
  await trigger.click()
  await expect(menu).toBeFocused()
  await frames(page)
  expect(await shown()).toEqual({ checked: true, active: true, same: true })
})

test('面板 OCR「视觉模型 ›」页（很多模型）：可以滚动、不弹回，换页时与换页后都没有横向滚动条', async ({
  page,
}) => {
  await open(page, '/panel.html?scenario=ocr.many-models', { os: 'windows' })
  // 非激活面板收不到键盘（与真机一致）：先点一下头部的说明文字，面板成为 key
  await page.getByTestId('ocr-meta-status').click()
  await page.getByTestId('ocr-recognize-menu').click()
  await expect(page.getByTestId('ocr-recognize-list')).toBeVisible()
  await page.waitForTimeout(300)
  // 换页过程逐帧记下菜单本体的横向溢出：溢出时必须是 overflow-x: hidden（否则出现横向滚动条）
  const sampling = page.evaluate(
    () =>
      new Promise<Array<{ ox: string; over: number }>>((resolve) => {
        const out: Array<{ ox: string; over: number }> = []
        const t0 = performance.now()
        const tick = () => {
          const el = document.querySelector<HTMLElement>('.ui-pmenu')
          if (el)
            out.push({ ox: getComputedStyle(el).overflowX, over: el.scrollWidth - el.clientWidth })
          if (performance.now() - t0 < 800) requestAnimationFrame(tick)
          else resolve(out)
        }
        requestAnimationFrame(tick)
      }),
  )
  await page.getByTestId('ocr-recognize-list-item-models').click()
  const log = await sampling
  expect(log.length).toBeGreaterThan(5)
  for (const f of log) expect(f.ox === 'hidden' || f.over <= 0, JSON.stringify(f)).toBe(true)
  const menu = page.getByTestId('ocr-model-menu')
  await expect(menu).toBeFocused()
  await expectScrollable(page, menu, '[data-page="models"] [role^="menuitem"]')
})

test('设置 › 剪贴板 › 排除应用的选择器（滚动的是浮层里的候选列表）：滚轮、设 scrollTop、窗口尺寸变化之后都不弹回', async ({
  page,
}) => {
  // 矮窗口：浮层按可用空间限高，候选列表在浮层里滚动
  await page.setViewportSize({ width: 760, height: 480 })
  await open(page, '/settings.html?scenario=settings.clipboard', { os: 'windows' })
  await page.getByTestId('clip-excluded-add').click()
  const picker = page.getByTestId('clip-app-picker')
  await expect(picker).toBeVisible()
  // 等「正在运行」读回（占位行换成真实的应用）
  await expect(picker.locator('[aria-busy="true"]')).toHaveCount(0)
  await page.waitForTimeout(400)
  const list = picker.locator('.st-app-picker-list')
  const top = () => list.evaluate((el) => el.scrollTop)
  expect(await list.evaluate((el) => el.scrollHeight - el.clientHeight)).toBeGreaterThan(40)
  const box = await list.boundingBox()
  if (!box) throw new Error('列表不可见')
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2, { steps: 2 })
  await page.mouse.wheel(0, 80)
  await expect.poll(top).toBeGreaterThan(0)
  await frames(page)
  await page.waitForTimeout(200)
  expect(await top()).toBeGreaterThan(0)
  await list.evaluate((el) => {
    el.scrollTop = 30
  })
  await frames(page)
  expect(await top()).toBe(30)
  // 窗口尺寸变化：浮层重新定位（测量不改样式，候选列表的滚动位置不受影响）
  await page.setViewportSize({ width: 760, height: 470 })
  await frames(page)
  await page.waitForTimeout(150)
  expect(await top()).toBe(30)
})

test.describe('悬停进入子页', () => {
  /** 右键一条文本，返回菜单与「粘贴为」及其相邻的一项 */
  async function contextMenu(page: Page) {
    await page.waitForTimeout(300)
    const row = page.getByTestId('clip-row').nth(0)
    const r = await row.boundingBox()
    if (!r) throw new Error('no row')
    await page.mouse.click(r.x + 60, r.y + r.height / 2, { button: 'right' })
    const menu = page.getByTestId('clip-context-menu')
    await expect(menu).toBeVisible()
    await page.waitForTimeout(300)
    const items = menu.locator('[data-page="root"] [role^="menuitem"]')
    const ids = await items.evaluateAll((els) => els.map((e) => e.getAttribute('data-testid')))
    const at = ids.indexOf('clip-context-menu-item-paste_as')
    expect(at).toBeGreaterThan(0)
    return {
      menu,
      before: items.nth(at - 1),
      target: items.nth(at),
      after: items.nth(at + 1),
    }
  }

  test('剪贴板：指针停在「粘贴为」上进入子页（同一个浮层）；在原处点击、抖动后点击都被吞掉，离开原处后照常粘贴', async ({
    page,
  }) => {
    const calls = await open(page, '/panel.html?scenario=clipboard.list')
    const { menu, before, target } = await contextMenu(page)
    const pos = await menu.evaluateHandle((el) => el.parentElement)
    const from = await center(before)
    const to = await center(target)
    await page.mouse.move(from.x, from.y, { steps: 3 })
    await page.mouse.move(to.x, to.y, { steps: 4 })
    const sub = page.getByTestId('clip-menu-paste_as')
    await expect(sub).toBeVisible()
    // 同一个浮层换页（不是新开一个菜单）
    expect(await pos.evaluate((el) => (el as HTMLElement).isConnected)).toBe(true)
    expect(await sub.evaluate((el, p) => el.parentElement === p, pos)).toBe(true)
    // 悬停进入的子页不预先高亮
    await expect(sub).not.toHaveAttribute('aria-activedescendant', /./)
    await page.waitForTimeout(250)
    // 原处点击：不选择、不关菜单
    await page.mouse.down()
    await page.mouse.up()
    // 抖动 2 px 再点击：同样吞掉
    await page.mouse.move(to.x + 2, to.y + 1)
    await page.mouse.down()
    await page.mouse.up()
    await page.waitForTimeout(300)
    expect(count(calls, 'clipboard_paste', 'clipboard_paste_text')).toBe(0)
    await expect(sub).toBeVisible()
    // 离开原处之后照常选择：点子页里的第一种变换
    await sub.locator('[data-page="paste_as"] [role="menuitem"]').nth(1).click()
    await expect.poll(() => count(calls, 'clipboard_paste')).toBe(1)
  })

  test('剪贴板：快速经过「粘贴为」不进入子页', async ({ page }) => {
    await open(page, '/panel.html?scenario=clipboard.list')
    const { before, after } = await contextMenu(page)
    const from = await center(before)
    const to = await center(after)
    await page.mouse.move(from.x, from.y, { steps: 2 })
    await page.mouse.move(to.x, to.y, { steps: 3 })
    await page.waitForTimeout(400)
    await expect(page.getByTestId('clip-menu-paste_as')).toHaveCount(0)
    await expect(page.getByTestId('clip-context-menu')).toBeVisible()
  })

  test('OCR：指针停在「视觉模型」上进入模型页；在原处点击不会用指针下的模型重新识别', async ({
    page,
  }) => {
    const calls = await open(page, '/panel.html?scenario=ocr.done')
    await page.getByTestId('ocr-recognize-menu').click()
    await expect(page.getByTestId('ocr-recognize-list')).toBeVisible()
    await page.waitForTimeout(300)
    // 从触发器（菜单外）移进「视觉模型」
    const to = await center(page.getByTestId('ocr-recognize-list-item-models'))
    await page.mouse.move(to.x, to.y, { steps: 6 })
    const models = page.getByTestId('ocr-model-menu')
    await expect(models).toBeVisible()
    await page.waitForTimeout(250)
    const before = count(calls, 'ocr_start')
    await page.mouse.down()
    await page.mouse.up()
    await page.waitForTimeout(300)
    expect(count(calls, 'ocr_start')).toBe(before)
    await expect(models).toBeVisible()
  })
})

test.describe('点外面关闭菜单：只关菜单', () => {
  test('剪贴板右键菜单开着时点另一行：菜单关闭、不粘贴；再点一次才粘贴', async ({ page }) => {
    const calls = await open(page, '/panel.html?scenario=clipboard.list')
    await page.waitForTimeout(300)
    const rows = page.getByTestId('clip-row')
    const r1 = await rows.nth(1).boundingBox()
    const r0 = await rows.nth(0).boundingBox()
    if (!r1 || !r0) throw new Error('no row')
    await page.mouse.click(r1.x + 60, r1.y + r1.height / 2, { button: 'right' })
    await expect(page.getByTestId('clip-context-menu')).toBeVisible()
    await page.waitForTimeout(300)
    // 第一条在锚点（第二条上的指针位置）上方，不被菜单盖住
    await page.mouse.click(r0.x + 60, r0.y + r0.height / 2)
    await expect(page.getByTestId('clip-context-menu')).toHaveCount(0)
    await page.waitForTimeout(300)
    expect(count(calls, 'clipboard_paste')).toBe(0)
    await page.mouse.click(r0.x + 60, r0.y + r0.height / 2)
    await expect.poll(() => count(calls, 'clipboard_paste')).toBe(1)
  })

  test('剪贴板 ⋯ 菜单开着时点一行：菜单关闭、不粘贴', async ({ page }) => {
    const calls = await open(page, '/panel.html?scenario=clipboard.list')
    await page.waitForTimeout(300)
    await page.getByTestId('clip-more').click()
    await expect(page.getByTestId('clip-more-menu')).toBeVisible()
    await page.waitForTimeout(300)
    // ⋯ 菜单靠右展开：点一行的左侧（不被菜单盖住）
    const row = await page.getByTestId('clip-row').nth(2).boundingBox()
    if (!row) throw new Error('no row')
    await page.mouse.click(row.x + 30, row.y + row.height / 2)
    await expect(page.getByTestId('clip-more-menu')).toHaveCount(0)
    await page.waitForTimeout(300)
    expect(count(calls, 'clipboard_paste')).toBe(0)
  })
})
