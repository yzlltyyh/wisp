/**
 * 面板内「展开阅读」的浏览器级用例（真实引擎里的两栏排版、滚动、焦点与按键，jsdom 都复现不了）：
 * - 翻译：点头部的展开按钮 → 卡片变宽、「原文 | 译文」左右两栏、两栏各自可以滚动且按比例同步；Esc 先收起、再按一次才
 *   关闭；⌘⇧E 展开 / 收起；减少动效下同样可用；
 * - OCR：高截图左右并排（截图在左、识别文本在右，截图栏内滚动）；宽截图上下堆叠（截图 1:1 在上、放不下时横向滚动）。
 * 运行方式见 apps/desktop/playwright.config.ts。
 */
import { expect, test, type Page } from '@playwright/test'

test.use({ viewport: { width: 1280, height: 800 } })

async function open(page: Page, scenario: string, extra = ''): Promise<void> {
  await page.goto(`/panel.html?scenario=${scenario}&os=macos&lang=zh-CN&hud=0${extra}`)
  await page.waitForSelector('html[data-settled]')
}

async function widthOf(page: Page, testId: string): Promise<number> {
  const box = await page.getByTestId(testId).boundingBox()
  return Math.round(box?.width ?? 0)
}

/** 两个元素的位置关系（布局盒，不受动画中的 opacity 影响） */
async function boxes(page: Page, a: string, b: string) {
  const [ba, bb] = await Promise.all([
    page.getByTestId(a).boundingBox(),
    page.getByTestId(b).boundingBox(),
  ])
  if (!ba || !bb) throw new Error(`${a} / ${b} 不可见`)
  return [ba, bb] as const
}

/** 滚动容器的滚动比例与是否溢出 */
async function scrollState(page: Page, testId: string) {
  return page.getByTestId(testId).evaluate((el) => {
    const range = el.scrollHeight - el.clientHeight
    return { overflow: range > 4, ratio: range > 0 ? el.scrollTop / range : 0 }
  })
}

/** 指针移出面板（不点击）：头部按钮上的 tooltip 不挡住随后的按键 */
async function moveAway(page: Page): Promise<void> {
  await page.mouse.move(4, 790)
}

test.describe('翻译', () => {
  test('点展开：卡片宽 760、原文 | 译文两栏且各自可滚动；Esc 先收起、再按一次关闭', async ({
    page,
  }) => {
    await open(page, 'translate.long-output')
    const view = page.getByTestId('view-translate')
    await expect(view).toHaveAttribute('data-status', 'done')
    const before = await widthOf(page, 'panel-card')
    await page.getByTestId('tv-reading').click()
    await moveAway(page)
    await expect(view).toHaveAttribute('data-reading', 'columns')
    await expect.poll(() => widthOf(page, 'panel-card')).toBe(760)
    const [source, result] = await boxes(page, 'tv-source', 'tv-stream')
    expect(source.x + source.width).toBeLessThanOrEqual(result.x + 1)
    expect(Math.abs(source.y - result.y)).toBeLessThan(4)
    // 两栏各自滚动（长原文、长译文都超出了栏高）
    await expect.poll(async () => (await scrollState(page, 'tv-source')).overflow).toBe(true)
    await expect.poll(async () => (await scrollState(page, 'tv-stream')).overflow).toBe(true)
    // 按钮换成「收起」，⇄ 隐藏
    await expect(page.getByTestId('tv-reading')).toHaveAccessibleName('收起')
    await expect(page.getByTestId('tv-source-toggle')).toHaveCount(0)

    await page.keyboard.press('Escape')
    await expect(view).not.toHaveAttribute('data-reading')
    await expect.poll(() => widthOf(page, 'panel-card')).toBe(before)
    await expect(page.locator('#root')).toHaveAttribute('data-mock-window', 'shown')
    await page.keyboard.press('Escape')
    await expect(page.locator('#root')).toHaveAttribute('data-mock-window', 'hidden')
  })

  test('两栏按比例同步滚动：滚原文，译文跟到同样的比例；滚译文，原文跟着走', async ({ page }) => {
    await open(page, 'translate.long-output')
    await expect(page.getByTestId('view-translate')).toHaveAttribute('data-status', 'done')
    await page.getByTestId('tv-reading').click()
    await expect(page.getByTestId('view-translate')).toHaveAttribute('data-reading', 'columns')
    await expect.poll(() => widthOf(page, 'panel-card')).toBe(760)
    await expect.poll(async () => (await scrollState(page, 'tv-source')).overflow).toBe(true)
    await expect.poll(async () => (await scrollState(page, 'tv-stream')).overflow).toBe(true)
    // 流式结束时译文回到了开头：两栏都从顶部开始
    await expect.poll(async () => (await scrollState(page, 'tv-stream')).ratio).toBeLessThan(0.02)

    await page.getByTestId('tv-source').hover()
    await page.mouse.wheel(0, 240)
    await expect
      .poll(async () => (await scrollState(page, 'tv-source')).ratio)
      .toBeGreaterThan(0.05)
    await expect
      .poll(async () => {
        const [s, r] = await Promise.all([
          scrollState(page, 'tv-source'),
          scrollState(page, 'tv-stream'),
        ])
        return Math.abs(s.ratio - r.ratio)
      })
      .toBeLessThan(0.03)

    await page.getByTestId('tv-stream').hover()
    await page.mouse.wheel(0, 5000)
    await expect
      .poll(async () => (await scrollState(page, 'tv-stream')).ratio)
      .toBeGreaterThan(0.98)
    await expect
      .poll(async () => (await scrollState(page, 'tv-source')).ratio)
      .toBeGreaterThan(0.97)
  })

  test('⌘⇧E 展开 / 收起（面板取得键盘焦点之后）', async ({ page }) => {
    await open(page, 'translate.long-output')
    const view = page.getByTestId('view-translate')
    await expect(view).toHaveAttribute('data-status', 'done')
    // 非激活面板收不到键盘（与真机一致）：先点一下正文，面板成为 key
    await page.getByTestId('tv-stream-body').click()
    await page.keyboard.press('Meta+Shift+KeyE')
    await expect(view).toHaveAttribute('data-reading', 'columns')
    await expect.poll(() => widthOf(page, 'panel-card')).toBe(760)
    await page.keyboard.press('Meta+Shift+KeyE')
    await expect(view).not.toHaveAttribute('data-reading')
  })

  test('减少动效下同样可用', async ({ page }) => {
    await open(page, 'translate.long-output', '&reduced=1')
    const view = page.getByTestId('view-translate')
    await expect(view).toHaveAttribute('data-status', 'done')
    await page.getByTestId('tv-reading').click()
    await moveAway(page)
    await expect(view).toHaveAttribute('data-reading', 'columns')
    await expect.poll(() => widthOf(page, 'panel-card')).toBe(760)
    // 淡入结束后视图完全不透明（内联 opacity 已去掉）
    await expect
      .poll(() => page.locator('.panel-viewswitch').evaluate((el) => getComputedStyle(el).opacity))
      .toBe('1')
    await page.keyboard.press('Escape')
    await expect(view).not.toHaveAttribute('data-reading')
  })

  test('固定窗口宽 760：开着原文自动两栏、撑满窗口；关掉原文回到单栏', async ({ page }) => {
    await page.goto(
      '/pinned.html?label=pinned-mock&scenario=translate.pinned-reading&os=macos&lang=zh-CN&hud=0',
    )
    await page.waitForSelector('html[data-settled]')
    const view = page.getByTestId('view-translate')
    await expect(view).toHaveAttribute('data-reading', 'columns')
    const [source, result] = await boxes(page, 'tv-source', 'tv-stream')
    expect(source.x + source.width).toBeLessThanOrEqual(result.x + 1)
    // 两栏一样高（撑满窗口里底栏以上的高度），都在各自的栏里滚动
    expect(Math.abs(source.height - result.height)).toBeLessThan(2)
    await expect.poll(async () => (await scrollState(page, 'tv-source')).overflow).toBe(true)
    await page.getByTestId('tv-source-toggle').click()
    await expect(view).not.toHaveAttribute('data-reading')
    await expect(page.getByTestId('tv-source')).toHaveCount(0)
  })
})

test.describe('OCR', () => {
  test('高截图左右并排：截图在左、识别文本在右，截图栏比视图高时在栏内滚动', async ({ page }) => {
    await open(page, 'ocr.tall-image')
    await page.getByTestId('ocr-meta-status').click()
    await page.getByTestId('ocr-reading').click()
    const view = page.getByTestId('view-ocr')
    await expect(view).toHaveAttribute('data-reading', 'columns')
    await expect.poll(() => widthOf(page, 'panel-card')).toBe(880)
    await expect(page.getByTestId('ocr-thumbnail')).toHaveCount(0)
    const [shot, text] = await boxes(page, 'ocr-shot', 'ocr-text')
    expect(shot.x + shot.width).toBeLessThanOrEqual(text.x + 1)
    await expect.poll(async () => (await scrollState(page, 'ocr-shot')).overflow).toBe(true)
    await moveAway(page)
    await page.keyboard.press('Escape')
    await expect(view).not.toHaveAttribute('data-reading')
    await expect(page.getByTestId('ocr-thumbnail')).toHaveCount(1)
  })

  test('宽截图上下堆叠：截图 1:1 在上、放不下时横向滚动，识别文本在下', async ({ page }) => {
    await open(page, 'ocr.wide-image')
    await page.getByTestId('ocr-meta-status').click()
    await page.getByTestId('ocr-reading').click()
    const view = page.getByTestId('view-ocr')
    await expect(view).toHaveAttribute('data-reading', 'stacked')
    await expect.poll(() => widthOf(page, 'panel-card')).toBe(880)
    const [shot, text] = await boxes(page, 'ocr-shot', 'ocr-text')
    expect(shot.y + shot.height).toBeLessThanOrEqual(text.y + 1)
    // 截图按逻辑尺寸 1:1（3200 物理像素宽，比卡片宽）：框内横向滚动
    const shotState = await page.getByTestId('ocr-shot').evaluate((el) => {
      const img = el.querySelector('img') as HTMLImageElement
      return {
        scrolls: el.scrollWidth > el.clientWidth + 4,
        width: Math.round(img.getBoundingClientRect().width),
        expected: Math.round(3200 / window.devicePixelRatio),
      }
    })
    expect(shotState.scrolls).toBe(true)
    expect(shotState.width).toBe(shotState.expected)
  })
})
