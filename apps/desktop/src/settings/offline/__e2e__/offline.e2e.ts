/**
 * 离线模型的浏览器级用例（jsdom 不排版、没有真实的过渡）：
 * - 设置 › 离线模型：点下载后进度条只靠 transform 前进（scaleX 单调不减、不会回退），完成后变成「已下载」并出现「用于识别」；
 *   内存不足时先原地确认，确认条取那一行的高度（布局不跳）；
 * - 面板：结构化结果里的宽表格在表格内横向滚动，不撑宽面板；切到源码再切回预览，面板宽度不变。
 * 运行方式见 apps/desktop/playwright.config.ts。
 */
import { expect, test, type Page } from '@playwright/test'

test.use({ viewport: { width: 1280, height: 800 } })

async function openSettings(page: Page, scenario: string): Promise<void> {
  await page.goto(`/settings.html?scenario=${scenario}&os=macos&lang=zh-CN&hud=0`)
  await page.waitForSelector('html[data-settled]')
}

/** 进度条当前的 scaleX（transform 矩阵的 a 分量） */
async function scaleOf(page: Page, testId: string): Promise<number | null> {
  return page.evaluate((id) => {
    const bar = document.querySelector(`[data-testid="${id}"] .ui-progress-bar`)
    if (!bar) return null
    const m = getComputedStyle(bar).transform
    if (!m || m === 'none') return 0
    return Number(m.slice(m.indexOf('(') + 1).split(',')[0])
  }, testId)
}

test('下载：进度条只靠 transform 单调前进，完成后变成已下载并可以选用', async ({ page }) => {
  await openSettings(page, 'settings.offline')
  await page.getByTestId('offline-model-pp_ocrv6_small-download').click()
  const samples: number[] = []
  // mock 每 250 ms 推一次进度、约 2.5 s 下完：逐帧采样到出现「用于识别」
  for (let i = 0; i < 400; i++) {
    // oxlint-disable-next-line no-await-in-loop -- 逐帧采样
    const v = await scaleOf(page, 'offline-model-pp_ocrv6_small-progress')
    if (v !== null) samples.push(v)
    // oxlint-disable-next-line no-await-in-loop -- 逐帧采样
    if (await page.getByTestId('offline-model-pp_ocrv6_small-use').isVisible()) break
    // oxlint-disable-next-line no-await-in-loop -- 逐帧采样
    await page.evaluate(() => new Promise(requestAnimationFrame))
  }
  await expect(page.getByTestId('offline-model-pp_ocrv6_small-use')).toBeVisible()
  await expect(page.getByTestId('offline-model-pp_ocrv6_small')).toHaveAttribute(
    'data-state',
    'installed',
  )
  const determinate = samples.filter((v) => v > 0)
  expect(determinate.length).toBeGreaterThan(5)
  for (let i = 1; i < determinate.length; i++) {
    expect(determinate[i] ?? 0).toBeGreaterThanOrEqual((determinate[i - 1] ?? 0) - 0.001)
  }
  await page.getByTestId('offline-model-pp_ocrv6_small-use').click()
  await expect(page.getByTestId('offline-model-pp_ocrv6_small-use')).toHaveAttribute(
    'aria-pressed',
    'true',
  )
})

test('内存不足时点下载：原地确认，确认条不改变这一行的高度；Esc 取消后焦点回到下载按钮', async ({
  page,
}) => {
  await openSettings(page, 'settings.offline-low-memory')
  const row = page.getByTestId('offline-model-index_translate_9b')
  const before = (await row.boundingBox())?.height ?? 0
  await page.getByTestId('offline-model-index_translate_9b-download').click()
  const confirm = page.getByTestId('offline-model-index_translate_9b-confirm')
  await expect(confirm).toBeVisible()
  const after = (await confirm.boundingBox())?.height ?? 0
  expect(after).toBeGreaterThanOrEqual(before - 0.5)
  await page.keyboard.press('Escape')
  await expect(page.getByTestId('offline-model-index_translate_9b-download')).toBeFocused()
})

test('结构化结果的宽表格在表格内横向滚动，不撑宽面板；切换源码 / 预览时面板宽度不变', async ({
  page,
}) => {
  await page.goto('/panel.html?scenario=ocr.table&os=macos&lang=zh-CN&hud=0')
  await page.waitForSelector('html[data-settled]')
  const view = page.getByTestId('view-ocr')
  await expect(page.getByTestId('ocr-preview')).toBeVisible()
  const width = (await view.boundingBox())?.width ?? 0
  const overflow = await page.evaluate(() => {
    const wrap = document.querySelector<HTMLElement>('.ocr-table-wrap')
    const preview = document.querySelector<HTMLElement>('[data-testid="ocr-preview"]')
    return wrap && preview ? wrap.offsetWidth <= preview.clientWidth + 1 : false
  })
  expect(overflow).toBe(true)
  await page.getByTestId('ocr-source-toggle').click()
  await expect(page.getByTestId('ocr-text')).toBeVisible()
  expect((await view.boundingBox())?.width ?? 0).toBeCloseTo(width, 0)
  await page.getByTestId('ocr-source-toggle').click()
  await expect(page.getByTestId('ocr-preview')).toBeVisible()
  expect((await view.boundingBox())?.width ?? 0).toBeCloseTo(width, 0)
})
