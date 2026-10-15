/**
 * Overlay 的浏览器级用例：用真实鼠标（page.mouse）发组合按键。左键按住时再按右键，Chromium 与 WebKit 都只发
 * pointermove（button = 2），不发 pointerdown（DOM 测试合成的 pointerdown(button = 2) 是假阳性）。
 * 运行方式见 apps/desktop/playwright.config.ts。
 */
import { chromium, expect, test, type Page } from '@playwright/test'

async function open(page: Page): Promise<void> {
  await page.goto('/overlay.html?scenario=overlay.enter&os=windows&hud=0')
  // Overlay 的内容都是 fixed 定位，<html> 本身没有尺寸：只等属性出现
  await page.waitForSelector('html[data-settled]', { state: 'attached' })
  // 交互模块已加载
  await page.waitForSelector('.ov-sel', { state: 'attached' })
}

const committed = (page: Page) => page.locator('[data-overlay][data-done]').count()
/** 选区是否在拖拽（清掉后回到悬停：可能显示光标下窗口的高亮，那不是选区） */
const dragging = (page: Page) =>
  page
    .locator('.ov-sel')
    .evaluate((el) => !el.hasAttribute('data-off') && (el as HTMLElement).dataset.k === 'drag')

test('拖拽中按右键：清掉选区，松开左键不提交（先松右键）', async ({ page }) => {
  await open(page)
  await page.mouse.move(200, 200)
  await page.mouse.down({ button: 'left' })
  await page.mouse.move(420, 360, { steps: 8 })
  await expect.poll(() => dragging(page)).toBe(true)
  await page.mouse.down({ button: 'right' })
  await expect.poll(() => dragging(page)).toBe(false)
  await page.mouse.up({ button: 'right' })
  await page.mouse.move(480, 400, { steps: 4 })
  await page.mouse.up({ button: 'left' })
  await page.waitForTimeout(300)
  expect(await committed(page)).toBe(0)
})

test('拖拽中按右键、先松左键再松右键：不卡在拖拽，之后照常框选并提交', async ({ page }) => {
  await open(page)
  await page.mouse.move(200, 200)
  await page.mouse.down({ button: 'left' })
  await page.mouse.move(420, 360, { steps: 8 })
  await page.mouse.down({ button: 'right' })
  await page.mouse.up({ button: 'left' })
  await page.mouse.move(600, 500, { steps: 4 })
  await page.mouse.up({ button: 'right' })
  await page.waitForTimeout(200)
  expect(await committed(page)).toBe(0)
  // 再拖一次：照常提交
  await page.mouse.move(150, 150)
  await page.mouse.down()
  await page.mouse.move(400, 300, { steps: 8 })
  await page.mouse.up()
  await expect.poll(() => committed(page)).toBe(1)
})

test('纯竖直拖拽（选区太细）：松手后不卡在拖拽状态、不提交；之后照常框选', async ({ page }) => {
  await open(page)
  // 在桌面上（场景里左侧 x < 77 没有窗口；落在窗口上时按单击处理，会选中那个窗口）
  await page.mouse.move(30, 200)
  await page.mouse.down()
  await page.mouse.move(30, 420, { steps: 8 })
  await page.mouse.up()
  // 没有按键按着地移动：选区不再跟着指针走（回到悬停：可能高亮光标下的窗口，那不是拖拽的选区）
  await page.mouse.move(700, 600, { steps: 6 })
  expect(await dragging(page)).toBe(false)
  await page.waitForTimeout(200)
  expect(await committed(page)).toBe(0)
  // 之后照常框选
  await page.mouse.move(150, 150)
  await page.mouse.down()
  await page.mouse.move(400, 300, { steps: 8 })
  await page.mouse.up()
  await expect.poll(() => committed(page)).toBe(1)
})

test('极小的选区（< 8 × 8 逻辑 px）：不提交', async ({ page }) => {
  await open(page)
  await page.mouse.move(40, 40)
  await page.mouse.down()
  await page.mouse.move(46, 45, { steps: 3 })
  await page.mouse.up()
  await page.waitForTimeout(250)
  expect(await committed(page)).toBe(0)
})

test('确认后飞入：选中的那块像素本身飞向面板（独立合成层），暗化层的洞不动', async ({ page }) => {
  await open(page)
  await page.mouse.move(200, 200)
  await page.mouse.down()
  await page.mouse.move(520, 380, { steps: 8 })
  // 选区的渲染合并在 rAF 里：等它画完最后一次移动
  await page.waitForTimeout(80)
  const band0 = await page
    .locator('.ov-band')
    .first()
    .evaluate((el) => el.style.transform)
  await page.mouse.up()
  await expect.poll(() => page.locator('.ov-piece').count(), { timeout: 2000 }).toBe(1)
  expect(
    await page
      .locator('.ov-band')
      .first()
      .evaluate((el) => el.style.transform),
  ).toBe(band0)
})

// ---- 分数缩放（125% / 150%）下的选框几何 ----------------------------------------------------------------------------

/**
 * 用户真机（Windows 150%）：选框的四条边与暗化层的挖空都画错了尺寸——四个把手与尺寸标签的位置是对的，上下两条边从左上角
 * 向右多出约 1/3、左右两条边向下多出约 1/3（932 物理像素宽的选框，上边画了约 1244），挖空同样不对。这里着色后按设备像素
 * 逐像素判定：冻结帧与桌面替身隐藏、底色纯白（= 挖空），暗化带改成不透明的纯蓝，描边纯绿、强调色纯红，把手、尺寸标签与
 * 放大镜隐藏。
 *
 * Chromium 的 `deviceScaleFactor` 仿真不经过合成器的真实设备缩放，复现不了（实测 1.25 / 1.5 下几何都正确）：Chromium 另起
 * 一个带 `--force-device-scale-factor` 的浏览器（与 WebView2 在分数缩放显示器上同一条路径）；WebKit 的 deviceScaleFactor
 * 仿真即可复现
 */
const SCALES = [1.25, 1.5]
/** 拖拽的起点与终点（逻辑 px，4 的倍数：125% / 150% 下都落在整设备像素上，吸附不改变它们） */
const FROM = { x: 200, y: 200 }
const TO = { x: 520, y: 380 }

interface Box {
  x0: number
  y0: number
  x1: number
  y1: number
}
interface Verdict {
  /** 强调色（红）与描边（绿）的包围盒（设备像素，右 / 下开区间）；没有时为 null */
  accent: Box | null
  ring: Box | null
  /** 选框外扩 `slack` 之外的边线像素（应为 0：边线不越出选框） */
  strayEdge: number
  /** 选框之外的亮像素（应为 0：挖空不大于选框） */
  brightOutside: number
  /** 选框内缩 `slack` 之内不亮的像素（应为 0：挖空不小于选框，也没有被暗化带盖住） */
  holeMissing: number
  /** 选框外扩 `slack` 之外不是暗化色的像素（应为 0：其余地方都暗化了） */
  dimMissing: number
}

const COLORS = `
canvas { visibility: hidden !important; }
html, body { background: #fff !important; }
.ov-sel i, .ov-size, .ov-mag { visibility: hidden !important; }
.ov-band { background: rgb(0 0 255) !important; }
.ov-sel, .ov-sel > * { --ring: rgb(0 255 0) !important; --accent: rgb(255 0 0) !important; }
`

/** 截图（PNG 的 base64，设备像素）在一个空白页里解码并逐像素判定。`sel` 是选框（设备像素） */
async function judge(page: Page, png: string, sel: Box, slack: number): Promise<Verdict> {
  const blank = await page.context().newPage()
  try {
    return await blank.evaluate(
      async ({ b64, box, pad }) => {
        const blob = await (await fetch(`data:image/png;base64,${b64}`)).blob()
        const bmp = await createImageBitmap(blob, {
          colorSpaceConversion: 'none',
          premultiplyAlpha: 'none',
        })
        const c = new OffscreenCanvas(bmp.width, bmp.height)
        const g = c.getContext('2d')
        if (!g) throw new Error('no 2d context')
        g.drawImage(bmp, 0, 0)
        const d = g.getImageData(0, 0, bmp.width, bmp.height).data
        const grow = (b: typeof box, k: number) => ({
          x0: b.x0 - k,
          y0: b.y0 - k,
          x1: b.x1 + k,
          y1: b.y1 + k,
        })
        const inside = (b: typeof box, x: number, y: number) =>
          x >= b.x0 && x < b.x1 && y >= b.y0 && y < b.y1
        const outer = grow(box, pad)
        const inner = grow(box, -pad)
        let accent: typeof box | null = null
        let ring: typeof box | null = null
        const add = (b: typeof box | null, x: number, y: number) =>
          b
            ? {
                x0: Math.min(b.x0, x),
                y0: Math.min(b.y0, y),
                x1: Math.max(b.x1, x + 1),
                y1: Math.max(b.y1, y + 1),
              }
            : { x0: x, y0: y, x1: x + 1, y1: y + 1 }
        let strayEdge = 0
        let brightOutside = 0
        let holeMissing = 0
        let dimMissing = 0
        for (let y = 0; y < bmp.height; y++) {
          for (let x = 0; x < bmp.width; x++) {
            const i = (y * bmp.width + x) * 4
            const r = d[i] ?? 0
            const gg = d[i + 1] ?? 0
            const b = d[i + 2] ?? 0
            const red = r > 200 && gg < 60 && b < 60
            const green = gg > 200 && r < 60 && b < 60
            const white = r > 245 && gg > 245 && b > 245
            const blue = b > 200 && r < 60 && gg < 60
            if (red) accent = add(accent, x, y)
            if (green) ring = add(ring, x, y)
            const out = !inside(outer, x, y)
            if ((red || green) && out) strayEdge++
            if (white && !inside(box, x, y)) brightOutside++
            if (!white && inside(inner, x, y)) holeMissing++
            if (!blue && out) dimMissing++
          }
        }
        bmp.close()
        return { accent, ring, strayEdge, brightOutside, holeMissing, dimMissing }
      },
      { b64: png, box: sel, pad: slack },
    )
  } finally {
    await blank.close()
  }
}

for (const dpr of SCALES) {
  test(`${dpr * 100}% 缩放：拖出的选框，边线与挖空都正好是选框（不越出、不缩进）`, async ({
    browser,
    browserName,
    baseURL,
  }) => {
    // Chromium：真实设备缩放（见上）；WebKit：deviceScaleFactor 仿真
    const own =
      browserName === 'chromium'
        ? await chromium.launch({
            args: [`--force-device-scale-factor=${dpr}`, '--window-size=1000,700'],
          })
        : null
    const context = own
      ? await own.newContext({ baseURL, viewport: null, deviceScaleFactor: undefined })
      : await browser.newContext({
          baseURL,
          viewport: { width: 1000, height: 700 },
          deviceScaleFactor: dpr,
        })
    try {
      const page = await context.newPage()
      await open(page)
      expect(await page.evaluate(() => devicePixelRatio)).toBe(dpr)
      await page.addStyleTag({ content: COLORS })
      // 暗化层渐入结束
      await page.waitForFunction(() => {
        const dim = document.querySelector('.ov-dim')
        return !!dim && dim.getAnimations().length === 0 && getComputedStyle(dim).opacity === '1'
      })
      await page.mouse.move(FROM.x, FROM.y)
      await page.mouse.down()
      await page.mouse.move(TO.x, TO.y, { steps: 8 })
      await expect.poll(() => dragging(page)).toBe(true)
      // 选区的渲染合并在 rAF 里：再等两帧
      await page.evaluate(
        () => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))),
      )
      const sel = {
        x0: Math.round(FROM.x * dpr),
        y0: Math.round(FROM.y * dpr),
        x1: Math.round(TO.x * dpr),
        y1: Math.round(TO.y * dpr),
      }
      // 描边最粗 round(dpr) 个设备像素；判定越界 / 缩进时再留 1 个设备像素
      const slack = Math.ceil(dpr) + 1
      const shot = await page.screenshot()
      const v = await judge(page, shot.toString('base64'), sel, slack)
      // 边线不越出选框（修复前：上 / 下边向右、左 / 右边向下多出 round(dpr) / dpr - 1）
      expect(v.strayEdge, JSON.stringify(v)).toBe(0)
      // 挖空正好是选框：选框外没有亮像素，选框内（除边线外）全亮，其余地方都暗化
      expect(v.brightOutside, JSON.stringify(v)).toBe(0)
      expect(v.holeMissing, JSON.stringify(v)).toBe(0)
      expect(v.dimMissing, JSON.stringify(v)).toBe(0)
      // 边线清晰：强调色正好落在选框最外一圈设备像素上，描边在它外侧、四边一样粗（整设备像素，没有抗锯齿的半像素）
      expect(v.accent).toEqual(sel)
      const g = v.ring ? sel.x0 - v.ring.x0 : 0
      expect(g).toBeGreaterThanOrEqual(1)
      expect(v.ring).toEqual({ x0: sel.x0 - g, y0: sel.y0 - g, x1: sel.x1 + g, y1: sel.y1 + g })
      await page.mouse.up()
    } finally {
      await context.close()
      await own?.close()
    }
  })
}
