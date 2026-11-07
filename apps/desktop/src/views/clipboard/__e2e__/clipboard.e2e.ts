/**
 * 剪贴板视图的浏览器级用例：jsdom 不排版、没有真实的指针速度与按键默认行为，这些只能在真实引擎里验证——
 * - 芯片小条：以正常速度（约 1 px / ms）从选中行上横着移过去点芯片，粘贴的是片段；小条出现在静止的指针下时单击
 *   仍是「点这一行」；触屏轻点与读屏 / 键盘的合成点击照样是点芯片；从第 1 行竖直扫到第 4 行，选中跟手、没有页面错误；
 * - 底部停靠的确认条：有提示条时键盘移到最后一条再删除，确认条停在这一行下面；
 * - Space：正在输入搜索词时是空格，↓ 之后是预览。
 * 运行方式见 apps/desktop/playwright.config.ts。
 */
import { expect, test, type Locator, type Page } from '@playwright/test'

test.use({ viewport: { width: 1280, height: 800 } })

type Call = [string, unknown]

/** 打开场景并记录 mock 的 IPC 调用（`ipc_log=1` 时 mock 路由逐条 console.debug）与页面错误 */
async function open(page: Page, scenario: string): Promise<{ calls: Call[]; errors: string[] }> {
  const calls: Call[] = []
  const errors: string[] = []
  page.on('pageerror', (e) => errors.push(e.message))
  page.on('console', (msg) => {
    if (!msg.text().startsWith('[mock ipc]')) return
    void Promise.all(msg.args().map((a) => a.jsonValue())).then(
      (args: unknown[]) => calls.push([String(args[1]), args[2]]),
      // 用例结束后才到的日志：忽略
      () => undefined,
    )
  })
  await page.goto(`/panel.html?scenario=${scenario}&os=macos&lang=zh-CN&hud=0&ipc_log=1`)
  await page.waitForSelector('html[data-settled]')
  return { calls, errors }
}

const called = (calls: Call[], cmd: string) => calls.filter(([c]) => c === cmd).map(([, a]) => a)

/** 第 n 行（从 0 起）的中心与左右边 */
async function rowBox(page: Page, n: number) {
  const box = await page.getByTestId('clip-row').nth(n).boundingBox()
  if (!box) throw new Error(`no row ${n}`)
  return { left: box.x, right: box.x + box.width, y: box.y + box.height / 2 }
}

/**
 * 以约 1 px / ms 的速度把指针从 (x0, y0) 移到 (x1, y1)：每 16 ms 一步、每步 16 px（正常移动鼠标的速度；Playwright 的
 * `steps` 不等待，一次就到）
 */
async function glide(page: Page, x0: number, y0: number, x1: number, y1: number): Promise<void> {
  const steps = Math.max(1, Math.round(Math.hypot(x1 - x0, y1 - y0) / 16))
  for (let i = 1; i <= steps; i++) {
    // oxlint-disable-next-line no-await-in-loop -- 指针事件依次发出，中间隔一帧
    await page.mouse.move(x0 + ((x1 - x0) * i) / steps, y0 + ((y1 - y0) * i) / steps)
    // oxlint-disable-next-line no-await-in-loop -- 同上
    await page.waitForTimeout(16)
  }
}

/** 选中行右侧的芯片小条：可点（指示器已到位、打开动效结束） */
async function liveStrip(page: Page) {
  const strip = page.getByTestId('clip-chips')
  await expect(strip).toBeVisible()
  await expect(
    page.locator('.clip-rowchips-slot:not([data-hold]):not([data-exiting])'),
  ).toHaveCount(1)
  return strip
}

/** 布局停下来之后的位置与尺寸（确认条从下往上展开、列表滚动都有过渡）：连续两次读到的相同才算 */
async function settledBox(page: Page, locator: Locator) {
  let prev: Awaited<ReturnType<Locator['boundingBox']>> = null
  for (let i = 0; i < 40; i++) {
    // oxlint-disable-next-line no-await-in-loop -- 逐次读取，比较前后两次
    const box = await locator.boundingBox()
    if (box && prev && box.y === prev.y && box.height === prev.height) return box
    prev = box
    // oxlint-disable-next-line no-await-in-loop -- 同上
    await page.waitForTimeout(50)
  }
  throw new Error('layout did not settle')
}

test('芯片小条：以正常速度从选中行上横着移过去点芯片，粘贴片段（不是整条）', async ({ page }) => {
  const { calls, errors } = await open(page, 'clipboard.chips')
  const row = await rowBox(page, 0)
  // 指针先落在第一行（光标所在的验证码条目）的左半边
  await page.mouse.move(row.left + 40, row.y)
  await glide(page, row.left + 40, row.y, row.left + 60, row.y)
  const strip = await liveStrip(page)
  const chip = await strip.getByTestId('clip-chip').first().boundingBox()
  if (!chip) throw new Error('no chip')
  const cx = chip.x + chip.width / 2
  const cy = chip.y + chip.height / 2
  await glide(page, row.left + 60, row.y, cx, cy)
  await expect(page.getByTestId('clip-search')).toHaveAttribute(
    'aria-activedescendant',
    (await page.getByTestId('clip-row').first().getAttribute('id')) ?? '',
  )
  await page.mouse.down()
  await page.mouse.up()
  await expect
    .poll(() => called(calls, 'clipboard_paste_text'))
    .toEqual([expect.objectContaining({ text: '482913', mode: 'normal' })])
  expect(called(calls, 'clipboard_paste')).toEqual([])
  expect(errors).toEqual([])
})

test('芯片小条出现在静止的指针下：单击按「点这一行」处理（粘贴整条）', async ({ page }) => {
  const { calls } = await open(page, 'clipboard.chips')
  const strip = await liveStrip(page)
  const chip = await strip.getByTestId('clip-chip').first().boundingBox()
  if (!chip) throw new Error('no chip')
  const cx = chip.x + chip.width / 2
  const first = await rowBox(page, 0)
  const second = await rowBox(page, 1)
  // 指针停到第二行的右侧（没有片段：小条随之撤掉），再一步跳回第一行、停在小条将要出现的位置
  await page.mouse.move(cx, second.y)
  await glide(page, cx, second.y, cx + 4, second.y)
  await expect(page.getByTestId('clip-chips')).toHaveCount(0)
  await page.mouse.move(cx, first.y)
  await liveStrip(page)
  await page.mouse.down()
  await page.mouse.up()
  const id = Number(await page.getByTestId('clip-row').first().getAttribute('data-clip-id'))
  await expect
    .poll(() => called(calls, 'clipboard_paste'))
    .toEqual([expect.objectContaining({ id, mode: 'normal' })])
  expect(called(calls, 'clipboard_paste_text')).toEqual([])
})

test('读屏 / 键盘激活芯片（没有指针事件的合成点击）：粘贴片段', async ({ page }) => {
  const { calls } = await open(page, 'clipboard.chips')
  const strip = await liveStrip(page)
  // VoiceOver 的 AXPress、NVDA 的激活与 HTMLElement.click() 一样：detail 为 0，之前没有 pointermove
  await strip
    .getByTestId('clip-chip')
    .first()
    .evaluate((el) => (el as HTMLElement).click())
  await expect
    .poll(() => called(calls, 'clipboard_paste_text'))
    .toEqual([expect.objectContaining({ text: '482913', mode: 'normal' })])
  expect(called(calls, 'clipboard_paste')).toEqual([])
})

test.describe('触屏', () => {
  test.use({ hasTouch: true })

  test('轻点芯片（只有 pointerdown / up / click）：粘贴片段，不粘贴整条', async ({ page }) => {
    const { calls } = await open(page, 'clipboard.chips')
    const strip = await liveStrip(page)
    const chip = await strip.getByTestId('clip-chip').first().boundingBox()
    if (!chip) throw new Error('no chip')
    await page.touchscreen.tap(chip.x + chip.width / 2, chip.y + chip.height / 2)
    await expect
      .poll(() => called(calls, 'clipboard_paste_text'))
      .toEqual([expect.objectContaining({ text: '482913', mode: 'normal' })])
    expect(called(calls, 'clipboard_paste')).toEqual([])
  })
})

test('有提示条时用键盘移到最后一条再删除：确认条停在这一行下面，不压住它', async ({ page }) => {
  await open(page, 'clipboard.favorites-view')
  await expect(page.getByTestId('clip-show-favorites')).toHaveCount(0)
  await expect(page.locator('.clip-hints')).toBeVisible()
  const search = page.getByTestId('clip-search')
  await search.click()
  await page.keyboard.press('Meta+ArrowDown')
  const id = await search.getAttribute('aria-activedescendant')
  const last = page.locator(`#${id}`)
  await expect(last).toHaveAttribute('data-clip-id', /\d+/)
  await page.keyboard.press('Backspace')
  const dock = page.getByTestId('clip-confirm-delete')
  await expect(dock).toBeVisible()
  const dockAt = await settledBox(page, dock)
  const rowAt = await settledBox(page, last)
  // 整行（含选中背景）都在确认条上沿之上：标题不被压住，也不落进确认条上沿的渐隐遮罩
  expect(dockAt.y).toBeGreaterThanOrEqual(rowAt.y + rowAt.height - 0.5)
})

test('从第 1 行竖直扫到第 4 行：选中项跟着指针走，没有页面错误', async ({ page }) => {
  const { errors } = await open(page, 'clipboard.chips')
  await liveStrip(page)
  const first = await rowBox(page, 0)
  const fourth = await rowBox(page, 3)
  await page.mouse.move(first.left + 40, first.y)
  await glide(page, first.left + 40, first.y, first.left + 40, fourth.y)
  const id = await page.getByTestId('clip-row').nth(3).getAttribute('id')
  await expect(page.getByTestId('clip-search')).toHaveAttribute('aria-activedescendant', id ?? '')
  expect(errors).toEqual([])
})

test('Space：正在输入搜索词时是空格；↓ 之后是预览', async ({ page }) => {
  await open(page, 'clipboard.list')
  const search = page.getByTestId('clip-search')
  // 非激活面板收不到键盘（与真机一致）：先点一下搜索框，面板成为 key
  await search.click()
  await page.keyboard.type('e')
  await expect(page.locator('.clip-group')).toHaveCount(0)
  await page.keyboard.press('Space')
  await expect(search).toHaveValue('e ')
  await expect(page.getByTestId('clip-preview')).toHaveCount(0)
  await page.keyboard.press('Backspace')
  await page.keyboard.press('ArrowDown')
  await page.keyboard.press('Space')
  await expect(page.getByTestId('clip-preview')).toBeVisible()
  await expect(search).toHaveValue('e')
})
