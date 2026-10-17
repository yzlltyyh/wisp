/**
 * OCR 视图的浏览器级用例：jsdom 不排版、没有真实焦点与样式，这些只能在真实引擎里验证——
 * - 键盘打开每个菜单后焦点进入菜单（浮层定位前不可见、focus() 不生效），↓ + Enter 选中并发出请求；
 * - 识别菜单换页：同一个浮层（不卸载、不重播进入动效），锚定的底边不动；
 * - 分体按钮两半各自的焦点标识；面板里 Tab 在视图内循环；
 * - 展开 / 收起译文区时逐帧采样：内容放得下时底栏不动；卡片已到上限时底栏只朝一个方向平滑移动、文本框与底栏之间
 *   没有空白、卡片高度不变；
 * - 窄面板没有「ResizeObserver loop」之类的页面错误；
 * - 排版按钮：360 px 宽、中英文、三种模式下底栏放得下、各控件点得到；切换排版后正文更新、剪贴板跟随；
 *   「合并段内换行」只在逐行模式；点排版按钮不让面板成为 key；⌘↩ 把当前的文字粘贴回原应用。
 * 运行方式见 apps/desktop/playwright.config.ts。
 */
import { expect, test, type Page } from '@playwright/test'
import { starved } from '../../../shared/ui/__e2e__/frames'

// 与 qa:shots 同一视口（卡片上限随视口而定）
test.use({ viewport: { width: 1280, height: 800 } })

/** 一帧的时长（60 Hz） */
const FRAME_MS = 1000 / 60
/**
 * 逐帧采样的单帧位移上限（px）。两次 rAF 之间按帧时间戳隔了 2–3 帧（丢帧）时上限按帧数放宽，但**最多 2 倍**——布局突变
 * 往往就发生在计算很重、拖长了的那一帧，不能因为帧长就无限放宽；隔了 3 帧以上、位移又超出单帧上限的一对采样分不清是丢帧
 * 还是跳变，记为无效，整轮重采（最多 3 轮）。帧数取「这一帧与上一帧的间隔」中较长的那个：同一帧里采样回调可能排在动画
 * 回调之前，读到的是上一帧动画更新后的布局，两次采样之间的位移对应的是上一帧的间隔（高负载下实测：超出 30 px 的那几对
 * 都是本帧 3–17 ms、上一帧 26–73 ms）
 */
const STEP_PX = 30
const MAX_STEP_FRAMES = 2
const MAX_GAP_FRAMES = 3
const SAMPLE_ROUNDS = 3

type Call = [string, unknown]

/** 打开场景并记录 mock 的 IPC 调用（`ipc_log=1` 时 mock 路由逐条 console.debug） */
async function open(page: Page, scenario: string, extra = ''): Promise<Call[]> {
  const calls: Call[] = []
  page.on('console', (msg) => {
    if (!msg.text().startsWith('[mock ipc]')) return
    void Promise.all(msg.args().map((a) => a.jsonValue())).then(
      (args: unknown[]) => calls.push([String(args[1]), args[2]]),
      // 用例结束后才到的日志：忽略
      () => undefined,
    )
  })
  await page.goto(`/panel.html?scenario=${scenario}&os=macos&lang=zh-CN&hud=0&ipc_log=1${extra}`)
  await page.waitForSelector('html[data-settled]')
  // 非激活面板收不到键盘（与真机一致）：先点一下头部的说明文字，面板成为 key（panel_activate）
  await page.getByTestId('ocr-meta-status').click()
  return calls
}

const lastReq = (calls: Call[], cmd: string) =>
  (calls.findLast(([c]) => c === cmd)?.[1] as { req?: Record<string, unknown> } | undefined)?.req

/** 连按几次同一个键（按键必须依次发出） */
async function press(page: Page, key: string, times: number): Promise<void> {
  for (let i = 0; i < times; i++) {
    // oxlint-disable-next-line no-await-in-loop -- 按键依次发出
    await page.keyboard.press(key)
  }
}

test.describe('键盘打开菜单：焦点进入菜单，↓ + Enter 选中', () => {
  test('底栏 [翻译 | ▾] → 风格菜单 → 第二个风格：展开译文区并翻译', async ({ page }) => {
    const calls = await open(page, 'ocr.done')
    await page.getByTestId('ocr-translate-menu').focus()
    await page.keyboard.press('Enter')
    const menu = page.getByTestId('ocr-style-list')
    await expect(menu).toBeFocused()
    await page.keyboard.press('ArrowDown')
    await page.keyboard.press('Enter')
    await expect(page.getByTestId('ocr-translation')).toBeVisible()
    await expect.poll(() => lastReq(calls, 'translate_start')?.style_id).toBe('technical')
  })

  test('底栏 [重新识别 | ▾] → 模式菜单 → 段落：用同一张截图重新识别', async ({ page }) => {
    const calls = await open(page, 'ocr.done')
    await page.getByTestId('ocr-recognize-menu').focus()
    await page.keyboard.press('Enter')
    await expect(page.getByTestId('ocr-recognize-list')).toBeFocused()
    await page.keyboard.press('ArrowDown')
    await page.keyboard.press('Enter')
    await expect.poll(() => lastReq(calls, 'ocr_start')?.mode).toBe('paragraphs')
    await expect(page.getByTestId('view-ocr')).toHaveAttribute('data-mode', 'paragraphs')
  })

  test('「视觉模型 ›」：→ 进入模型页、← 返回、Enter 选中模型', async ({ page }) => {
    const calls = await open(page, 'ocr.done')
    await page.getByTestId('ocr-recognize-menu').focus()
    await page.keyboard.press('Enter')
    await expect(page.getByTestId('ocr-recognize-list')).toBeFocused()
    // 纯文本（当前）→ 段落 → 结构化 → 视觉模型
    await press(page, 'ArrowDown', 3)
    await page.keyboard.press('ArrowRight')
    const models = page.getByTestId('ocr-model-menu')
    await expect(models).toBeFocused()
    await page.keyboard.press('ArrowLeft')
    await expect(page.getByTestId('ocr-recognize-list')).toBeFocused()
    await press(page, 'ArrowDown', 3)
    await page.keyboard.press('Enter')
    await expect(models).toBeFocused()
    // 当前模型（qwen3-vl-plus）→ qwen3-vl-flash
    await page.keyboard.press('ArrowDown')
    await page.keyboard.press('Enter')
    await expect
      .poll(() => lastReq(calls, 'ocr_start')?.model_ref)
      .toEqual({ provider_id: 'qwen', model_id: 'qwen3-vl-flash' })
  })

  test('译文区的风格 chip：Enter 打开、↓ + Enter 换风格即重新翻译', async ({ page }) => {
    const calls = await open(page, 'ocr.translate')
    await page.getByTestId('ocr-style-chip').focus()
    await page.keyboard.press('Enter')
    await expect(page.getByTestId('ocr-section-style-list')).toBeFocused()
    await page.keyboard.press('ArrowDown')
    await page.keyboard.press('Enter')
    await expect.poll(() => lastReq(calls, 'translate_start')?.style_id).toBe('technical')
  })

  test('no_vision 卡片的 [选择模型]：键盘打开模型菜单并选中，带 model_ref 重新识别', async ({
    page,
  }) => {
    const calls = await open(page, 'ocr.error-no-vision')
    await page.getByRole('button', { name: '选择模型' }).focus()
    await page.keyboard.press('Enter')
    const models = page.getByTestId('ocr-model-menu')
    await expect(models).toBeFocused()
    await page.keyboard.press('ArrowDown')
    await page.keyboard.press('Enter')
    await expect
      .poll(() => lastReq(calls, 'ocr_start')?.model_ref)
      .toEqual({ provider_id: 'qwen', model_id: 'qwen3-vl-flash' })
  })
})

/** 第 i 对采样（run[i - 1] → run[i]）对应的动画经过了几帧：本帧与上一帧的帧时间戳间隔取较长者（见 STEP_PX） */
function gapFrames(run: Array<Record<string, number>>, i: number) {
  const interval = (k: number) => (k >= 1 ? (run[k]?.f ?? 0) - (run[k - 1]?.f ?? 0) : 0)
  return Math.round(Math.max(interval(i), interval(i - 1)) / FRAME_MS)
}

/**
 * 每帧记录元素的位置 / 高度，持续 ms 毫秒。`f` 是 rAF 回调参数里的帧时间戳（同一帧的回调相同，按显示帧对齐），帧间隔
 * 用它算；`t` 是回调实际执行的时刻，只用来控制采样时长
 */
async function sample(page: Page, selectors: Record<string, string>, ms: number) {
  return page.evaluate(
    ([sel, duration]) =>
      new Promise<Array<Record<string, number>>>((resolve) => {
        const out: Array<Record<string, number>> = []
        const t0 = performance.now()
        const tick = (frameTs: number) => {
          const t = performance.now() - t0
          const row: Record<string, number> = { t, f: frameTs }
          // 位置相对视图顶边（面板窗口随高度变化可能整体移动，那不是跳变）
          const top =
            document.querySelector('[data-testid="view-ocr"]')?.getBoundingClientRect().top ?? 0
          for (const [k, s] of Object.entries(sel)) {
            const el = document.querySelector(s)
            const r = el?.getBoundingClientRect()
            row[`${k}.y`] = r ? r.top - top : -1
            row[`${k}.h`] = r ? r.height : -1
          }
          out.push(row)
          if (t < duration) requestAnimationFrame(tick)
          else resolve(out)
        }
        requestAnimationFrame(tick)
      }),
    [selectors, ms] as const,
  )
}

test.describe('展开 / 收起译文区：底栏不跳、OCR 文本行数不变（内容放得下时）', () => {
  for (const frame of ['css', 'native']) {
    test(`frame=${frame}`, async ({ page }) => {
      await open(page, 'ocr.done', `&frame=${frame}`)
      const sel = { footer: '[data-testid="ocr-footer"]', text: '[data-testid="ocr-text"]' }
      const before = await sample(page, sel, 50)
      const y0 = before.at(-1)?.['footer.y'] ?? 0
      const h0 = before.at(-1)?.['text.h'] ?? 0
      const openRun = sample(page, sel, 900)
      await page.getByTestId('ocr-translate-toggle').click()
      const opened = await openRun
      const closeRun = sample(page, sel, 900)
      await page.getByTestId('ocr-translate-toggle').click()
      const closed = await closeRun
      for (const row of [...opened, ...closed]) {
        expect(Math.abs((row['footer.y'] ?? 0) - y0)).toBeLessThanOrEqual(1)
        expect(Math.abs((row['text.h'] ?? 0) - h0)).toBeLessThanOrEqual(1)
      }
    })
  }
})

test.describe('长文本（卡片已到上限）：两区之间的重新分配平滑、只朝一个方向、没有空白，卡片高度不变', () => {
  for (const frame of ['css', 'native']) {
    test(`frame=${frame}`, async ({ page }) => {
      await open(page, 'ocr.long-text', `&frame=${frame}`)
      // 看得见的高度：Css 帧模式是卡片；Native 是窗口（mock 里的 #root）
      const sel = {
        footer: '[data-testid="ocr-footer"]',
        text: '[data-testid="ocr-text"]',
        card: frame === 'css' ? '[data-testid="panel-card"]' : '#root',
      }
      const gap = (row: Record<string, number>) =>
        (row['footer.y'] ?? 0) - ((row['text.y'] ?? 0) + (row['text.h'] ?? 0))
      // 一轮：展开、收起各采一段（每轮都从收起状态开始、以收起结束）。有分不清丢帧与跳变的长帧时重采
      let before: Array<Record<string, number>> = []
      let opened: Array<Record<string, number>> = []
      let closed: Array<Record<string, number>> = []
      let stalls: Array<{ text: string; frames: number; step: number }> = []
      /* oxlint-disable no-await-in-loop -- 采样轮次必须串行：每轮都从收起状态开始、以收起结束 */
      for (let round = 1; round <= SAMPLE_ROUNDS; round++) {
        before = await sample(page, sel, 50)
        const openRun = sample(page, sel, 1200)
        await page.getByTestId('ocr-translate-toggle').click()
        opened = await openRun
        const closeRun = sample(page, sel, 900)
        await page.getByTestId('ocr-translate-toggle').click()
        closed = await closeRun
        stalls = [opened, closed].flatMap((run) =>
          run.slice(1).flatMap((row, k) => {
            const frames = gapFrames(run, k + 1)
            const step = Math.abs((row['footer.y'] ?? 0) - (run[k]?.['footer.y'] ?? 0))
            return frames > MAX_GAP_FRAMES && step > STEP_PX
              ? [
                  {
                    text: `第 ${k + 1} 帧：隔了 ${frames} 帧、位移 ${step.toFixed(1)} px`,
                    frames,
                    step,
                  },
                ]
              : []
          }),
        )
        if (stalls.length === 0) break
      }
      /* oxlint-enable no-await-in-loop */
      // 3 轮都在动效期间出现超过 3 帧的长帧（无 GPU 的容器里 headless WebKit 走软件合成；macOS 与 Chromium 不会）：位移按
      // 经过的帧数折算后仍在单帧上限内的，是丢帧而不是跳变——记一条注解，之后长帧的位移按经过的帧数放宽；折算后仍超出的
      // 才是跳变，失败
      const explained = stalls.length > 0 && stalls.every((x) => x.step <= STEP_PX * x.frames)
      if (explained)
        test.info().annotations.push({
          type: 'starved-frames',
          description: `连续 ${SAMPLE_ROUNDS} 轮都有长帧：${stalls.map((x) => x.text).join('；')}——按经过的帧数折算都在单帧上限内（丢帧）`,
        })
      expect(
        explained ? [] : stalls.map((x) => x.text),
        `连续 ${SAMPLE_ROUNDS} 轮采样都有超过 ${MAX_GAP_FRAMES} 帧的长帧、且位移按经过的帧数折算后仍超出单帧上限（跳变）`,
      ).toEqual([])
      const card0 = before.at(-1)?.['card.h'] ?? 0
      const gap0 = gap(before.at(-1) ?? {})
      for (const [run, dir] of [
        [opened, -1],
        [closed, 1],
      ] as const) {
        const hungry = starved(run.map((r) => r.f ?? 0))
        if (hungry)
          test.info().annotations.push({
            type: 'starved-frames',
            description: `${dir < 0 ? '展开' : '收起'}的采样饿帧：长帧的位移按经过的帧数折算（每帧 ${STEP_PX} px）`,
          })
        for (let i = 1; i < run.length; i++) {
          const a = run[i - 1]?.['footer.y'] ?? 0
          const b = run[i]?.['footer.y'] ?? 0
          // 底栏随分配平滑移动：没有一帧之内的大跳变（弹簧的单帧位移、流式的一行都远小于此）。按帧时间戳算动画经过了几帧
          // （gapFrames）：1 帧 30 px；2–3 帧（headless WebKit 在高负载 / 软件合成下丢帧）放宽、最多 2 倍
          // （CI 稳定性）；3 帧以上只按单帧上限判（超出的那几对已在上面整轮重采）
          const frames = gapFrames(run, i)
          // 3 帧以上：已确认是丢帧（explained）时按经过的帧数放宽，否则只按单帧上限判。这一段采样整体饿帧（frames.ts：
          // 无 GPU 的容器里 headless WebKit 走软件合成，macOS 与 Chromium 不会）时 2–3 帧的间隔同样按经过的帧数折算
          const mult =
            explained || hungry
              ? Math.max(frames, 1)
              : frames > MAX_GAP_FRAMES
                ? 1
                : Math.min(Math.max(frames, 1), MAX_STEP_FRAMES)
          const df = (run[i]?.f ?? 0) - (run[i - 1]?.f ?? 0)
          const dfPrev = i > 1 ? (run[i - 1]?.f ?? 0) - (run[i - 2]?.f ?? 0) : 0
          expect(
            Math.abs(b - a),
            `${dir < 0 ? '展开' : '收起'}第 ${i} 帧：底栏 ${a.toFixed(1)} → ${b.toFixed(1)}，帧间隔 ${df.toFixed(1)} ms、上一帧 ${dfPrev.toFixed(1)} ms（按 ${frames} 帧计）`,
          ).toBeLessThanOrEqual(STEP_PX * mult)
          // 同一次操作只朝一个方向：展开时只上移，收起时只下移
          expect((b - a) * dir).toBeGreaterThanOrEqual(-1)
        }
        for (const row of run) {
          // 卡片停在上限附近（不先缩再长）
          expect(Math.abs((row['card.h'] ?? 0) - card0)).toBeLessThanOrEqual(12)
          // 文本框的可视高度跟着外框走：文本与底栏之间不露出空白
          expect(Math.abs(gap(row) - gap0)).toBeLessThanOrEqual(2)
        }
      }
    })
  }
})

test('长译文（卡片已到上限）：展开结束后译文在框内滚动，区底状态行不被裁掉', async ({ page }) => {
  await open(page, 'ocr.long-text-translate')
  const section = page.getByTestId('ocr-translation')
  // 展开过渡结束后区要撤掉 data-sizing：撤不掉时内层按自然高度排版、被区裁住，下半截既看不到也滚不动
  await expect.poll(() => section.evaluate((el) => el.hasAttribute('data-sizing'))).toBe(false)
  const box = section.locator('.ocr-box')
  expect(await box.evaluate((el) => el.scrollHeight - el.clientHeight)).toBeGreaterThan(100)
  const clipped = await section.evaluate((el) => {
    const foot = el.querySelector('[data-testid="ocr-translation-foot"]')
    return foot ? foot.getBoundingClientRect().bottom - el.getBoundingClientRect().bottom : null
  })
  expect(clipped).not.toBeNull()
  expect(clipped ?? 1).toBeLessThanOrEqual(0.5)
  await box.hover()
  await page.mouse.wheel(0, 400)
  await expect.poll(() => box.evaluate((el) => el.scrollTop)).toBeGreaterThan(0)
})

test('识别菜单换页：同一个浮层，不重播进入动效，锚定的底边不动', async ({ page }) => {
  await open(page, 'ocr.done')
  await page.getByTestId('ocr-recognize-menu').click()
  const menu = page.getByRole('menu')
  await expect(menu).toBeVisible()
  await page.waitForTimeout(300)
  const handle = await menu.elementHandle()
  const box0 = await menu.boundingBox()
  const run = page.evaluate(
    (el) =>
      new Promise<Array<{ bottom: number; opacity: number; connected: boolean }>>((resolve) => {
        const out: Array<{ bottom: number; opacity: number; connected: boolean }> = []
        const t0 = performance.now()
        const tick = () => {
          const pos = (el as HTMLElement).parentElement as HTMLElement
          out.push({
            bottom: (el as HTMLElement).getBoundingClientRect().bottom,
            opacity: Number(getComputedStyle(pos).opacity),
            connected: el?.isConnected ?? false,
          })
          if (performance.now() - t0 < 600) requestAnimationFrame(tick)
          else resolve(out)
        }
        requestAnimationFrame(tick)
      }),
    handle,
  )
  await page.getByTestId('ocr-recognize-list-item-models').click()
  const frames = await run
  await expect(page.getByTestId('ocr-model-menu')).toBeVisible()
  for (const f of frames) {
    expect(f.connected).toBe(true)
    expect(f.opacity).toBeGreaterThan(0.99)
    // 亚像素取整（两个引擎不同）之外不动
    expect(Math.abs(f.bottom - (box0?.y ?? 0) - (box0?.height ?? 0))).toBeLessThanOrEqual(1.5)
  }
  // 返回行：‹ 在勾选列，标题与条目文字左对齐，无障碍名称是「返回识别菜单」
  const back = page.getByTestId('ocr-model-menu-item-back')
  await expect(back).toHaveAccessibleName('返回识别菜单')
  const backText = await back.locator('.ui-menu-text').boundingBox()
  const itemText = await page
    .locator('[data-page="models"] [role="menuitemcheckbox"] .ui-menu-text')
    .first()
    .boundingBox()
  expect(Math.abs((backText?.x ?? 0) - (itemText?.x ?? 0))).toBeLessThanOrEqual(1)
})

test('分体按钮：键盘焦点只在被聚焦的那一半画 2 px 焦点环，不改填充色（填充只表示「已展开」）', async ({
  page,
}) => {
  // Tab 由外壳接管（两种引擎、macOS 默认「键盘导航」关闭时同样经过按钮）
  await open(page, 'ocr.done')
  await page.getByTestId('ocr-copy').focus()
  await page.keyboard.press('Tab')
  const chip = page.getByTestId('ocr-translate-toggle')
  const caret = page.getByTestId('ocr-translate-menu')
  await expect(chip).toBeFocused()
  const style = (el: typeof chip) =>
    el.evaluate((n) => {
      const cs = getComputedStyle(n)
      return { outline: `${cs.outlineStyle} ${cs.outlineWidth}`, bg: cs.backgroundColor }
    })
  const chipFocused = await style(chip)
  const caretIdle = await style(caret)
  await page.keyboard.press('Tab')
  await expect(caret).toBeFocused()
  const chipIdle = await style(chip)
  const caretFocused = await style(caret)
  // 焦点环跟着换半边，另一半没有
  expect(chipFocused.outline).toBe('solid 2px')
  expect(caretIdle.outline).not.toBe('solid 2px')
  expect(caretFocused.outline).toBe('solid 2px')
  expect(chipIdle.outline).not.toBe('solid 2px')
  // 聚焦不改填充色
  expect(chipFocused.bg).toBe(chipIdle.bg)
  expect(caretFocused.bg).toBe(caretIdle.bg)
  // 外框（整组）不画焦点环
  const group = await chip.evaluate(
    (n) => getComputedStyle(n.parentElement as HTMLElement).outlineStyle,
  )
  expect(group).toBe('none')
})

test('面板里 Tab 在视图内循环：最后一个控件之后回到第一个，焦点不落到 body', async ({ page }) => {
  await open(page, 'ocr.done')
  await page.getByTestId('ocr-pin').focus()
  await page.keyboard.press('Tab')
  await expect(page.getByTestId('ocr-thumbnail')).toBeFocused()
  await page.keyboard.press('Shift+Tab')
  await expect(page.getByTestId('ocr-pin')).toBeFocused()
})

test('窄面板：没有页面错误（ResizeObserver loop 等）', async ({ page }) => {
  const errors: string[] = []
  page.on('pageerror', (e) => errors.push(e.message))
  await open(page, 'ocr.narrow')
  await page.waitForTimeout(1200)
  expect(errors).toEqual([])
})

test.describe('排版按钮：360 px 宽的底栏放得下、各控件点得到（中英文 × 逐行 / 段落 / 结构化）', () => {
  for (const lang of ['zh-CN', 'en-US'] as const) {
    for (const mode of ['lines', 'paragraphs', 'markdown'] as const) {
      test(`${lang} · ${mode}`, async ({ page }) => {
        await page.goto(`/panel.html?scenario=ocr.layout&mode=${mode}&os=macos&lang=${lang}&hud=0`)
        await page.waitForSelector('html[data-settled]')
        const footer = page.getByTestId('ocr-footer')
        // 底栏没有溢出；状态行里的「已复制」（场景在识别完成后 0.3 s 就绪，它还在）完整可见——状态行先按段隐藏，
        // 仍放不下时「翻译」只留图标
        expect(await footer.evaluate((el) => el.scrollWidth - el.clientWidth)).toBeLessThanOrEqual(
          0,
        )
        const status = footer.locator('.ocr-status')
        expect(await status.evaluate((el) => el.scrollWidth - el.clientWidth)).toBeLessThanOrEqual(
          0,
        )
        // 结构化在同一位置放「源码 / 预览」，没有排版按钮
        await expect(page.getByTestId('ocr-layout')).toHaveCount(mode === 'markdown' ? 0 : 1)
        // 逐行的样例文字没有空行：左半（去掉空行）不适用、aria-disabled，只核对它在底栏里看得见
        if (mode === 'lines') await expect(page.getByTestId('ocr-layout-toggle')).toBeVisible()
        const own =
          mode === 'markdown'
            ? ['ocr-source-toggle']
            : mode === 'lines'
              ? ['ocr-layout-caret']
              : ['ocr-layout-toggle', 'ocr-layout-caret']
        for (const id of [
          ...own,
          'ocr-copy',
          'ocr-translate-toggle',
          'ocr-translate-menu',
          'ocr-recognize-again',
          'ocr-recognize-menu',
          'ocr-pin',
        ]) {
          // trial：只核对可点性（可见、稳定、没有被别的元素盖住），不真的点
          // oxlint-disable-next-line no-await-in-loop -- 逐个核对
          await page.getByTestId(id).click({ trial: true })
        }
      })
    }
  }
})

test('切换排版：左半去掉空行，▾ 上按 ↓ 打开菜单选「合并为一行」，⌘L 回到原样；正文随之更新、剪贴板跟随', async ({
  page,
}) => {
  const calls = await open(page, 'ocr.layout', '&mode=paragraphs')
  const text = page.getByTestId('ocr-text')
  const toggle = page.getByTestId('ocr-layout-toggle')
  await expect(text).toHaveValue('第一段，被\n硬换行的中文。\n\n第二段。')
  await toggle.click()
  await expect(text).toHaveValue('第一段，被\n硬换行的中文。\n第二段。')
  await expect(toggle).toHaveAttribute('aria-pressed', 'true')
  await expect.poll(() => calls.filter(([c]) => c === 'ocr_recopy').length).toBe(1)
  await page.getByTestId('ocr-layout-caret').focus()
  await page.keyboard.press('ArrowDown')
  await expect(page.getByTestId('ocr-layout-menu')).toBeFocused()
  // 打开时高亮当前项（去掉空行），再 ↓ 是「合并为一行」
  await page.keyboard.press('ArrowDown')
  await page.keyboard.press('Enter')
  await expect(text).toHaveValue('第一段，被硬换行的中文。第二段。')
  await expect(toggle).toHaveAccessibleName('合并为一行')
  await page.keyboard.press('Meta+KeyL')
  await expect(text).toHaveValue('第一段，被\n硬换行的中文。\n\n第二段。')
  await expect(toggle).toHaveAttribute('aria-pressed', 'false')
})

test('逐行：没有空行时「去掉空行」置灰并写明原因，左半按不动；菜单里选「合并为一行」', async ({
  page,
}) => {
  await open(page, 'ocr.layout', '&mode=lines')
  const text = page.getByTestId('ocr-text')
  await expect(page.getByTestId('ocr-layout-toggle')).toHaveAttribute('aria-disabled', 'true')
  await page.getByTestId('ocr-layout-caret').click()
  const remove = page.getByTestId('ocr-layout-menu-item-remove_blank_lines')
  await expect(remove).toHaveAttribute('aria-disabled', 'true')
  await expect(remove).toContainText('文字里没有空行')
  await page.getByTestId('ocr-layout-menu-item-join_lines').click()
  await expect(text).toHaveValue('第一行第二行第三行')
})

test('逐行：菜单里的「合并段内换行」把段内换行连起来（列表保持原样），点排版按钮不让面板成为 key；⌘↩ 把当前的文字粘贴回原应用', async ({
  page,
}) => {
  const calls = await open(page, 'ocr.layout-wrapped')
  const count = (cmd: string) => calls.filter(([c]) => c === cmd).length
  const text = page.getByTestId('ocr-text')
  const toggle = page.getByTestId('ocr-layout-toggle')
  const raw =
    'Wisp stays in the background and wakes up only\nwhen you press a hotkey. Recognized text keeps\nits line breaks.\n\n- Lists stay as they are'
  const joined =
    'Wisp stays in the background and wakes up only when you press a hotkey. Recognized text keeps its line breaks.\n\n- Lists stay as they are'
  await expect(text).toHaveValue(raw)
  await page.getByTestId('ocr-layout-caret').click()
  const reflow = page.getByTestId('ocr-layout-menu-item-reflow')
  await expect(reflow).toContainText('把 PDF 里的硬换行连成段落')
  await reflow.click()
  await expect(text).toHaveValue(joined)
  await expect(toggle).toHaveAccessibleName('合并段内换行')
  await expect(toggle).toHaveAttribute('aria-pressed', 'true')
  // 左半与复制点一下就完成：按下时不发 panel_activate（文本框照旧发）
  await expect.poll(() => count('ocr_recopy')).toBe(1)
  const activations = count('panel_activate')
  await toggle.click()
  await expect(text).toHaveValue(raw)
  await page.getByTestId('ocr-copy').click()
  await expect.poll(() => count('clipboard_copy_text')).toBe(1)
  expect(count('panel_activate')).toBe(activations)
  await toggle.click()
  await expect(text).toHaveValue(joined)
  await text.click()
  await expect.poll(() => count('panel_activate')).toBe(activations + 1)
  // ⌘↩（焦点在文本框里）：片段粘贴当前显示的文字，不在文本框里插入换行（mock 与 Rust 一样随之收起面板）
  await page.keyboard.press('Meta+Enter')
  await expect
    .poll(() => calls.findLast(([c]) => c === 'clipboard_paste_text')?.[1])
    .toEqual({ text: joined, from: null, mode: 'normal' })
})
