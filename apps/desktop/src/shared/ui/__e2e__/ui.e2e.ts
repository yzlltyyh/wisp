/**
 * 组件库与外壳的浏览器级用例（真实引擎里的焦点、键盘与逐帧布局——jsdom 复现不了）。运行方式见
 * apps/desktop/playwright.config.ts：`pnpm -C apps/desktop exec playwright test`
 * - 组件库的 Menu 在第一次打开时焦点就进入菜单、关闭后回到触发器（视图不需要自己补焦点）；
 * - OCR 面板头部 ⋯ 的「识别历史…」直达设置 › OCR 的历史列表，设置窗口按锚点聚焦分组标题；
 * - 外壳统一的 Tab 循环：每次 Tab 都由外壳接管，两种引擎同一顺序、回绕；
 * - 逐帧用例（rAF 采样）：翻译菜单按下即出现、Css 帧退出不下沉、剪贴板再次唤出的 stagger、识别历史
 *   插入；排除应用选择器的键盘全流程；
 * - 识别历史外框与内容逐帧同高（展开 / 收起 / 删除 / 插入）、换状态才有高度过渡；冷页面第一次 prepare
 *   报真实高度（Css / Native × 剪贴板列表 / 空）；居中放置长高时卡片顶边不动；OCR 识别菜单完整可见；翻译菜单
 *   的预留用 snappy、关闭淡出后才缩回、下拉帘期间不掉帧；剪贴板矮面板的右键菜单上移到卡片内；设置侧栏选中背景逐帧
 *   连续；引导换步骤的交叉区间；OCR 文本区结束后回到开头。
 */
import { expect, test, type Page } from '@playwright/test'
import { FRAME_MS, noteStarved, starved, stateTime } from './frames'

test.use({ viewport: { width: 1280, height: 800 } })

type Call = [string, unknown]

async function open(page: Page, url: string): Promise<Call[]> {
  const calls: Call[] = []
  page.on('console', (msg) => {
    if (!msg.text().startsWith('[mock ipc]')) return
    void Promise.all(msg.args().map((a) => a.jsonValue())).then(
      (args: unknown[]) => calls.push([String(args[1]), args[2]]),
      () => undefined,
    )
  })
  await page.goto(`${url}&os=macos&lang=zh-CN&hud=0&ipc_log=1`)
  await page.waitForSelector('html[data-settled]')
  return calls
}

test('翻译视图 ⋯：键盘第一次打开焦点就在菜单里；Esc 关闭后焦点回到 ⋯', async ({ page }) => {
  await open(page, '/panel.html?scenario=translate.done')
  // 非激活面板收不到键盘（与真机一致）：先点一下正文，面板成为 key
  await page.getByTestId('tv-stream-body').click()
  await page.getByTestId('tv-more').focus()
  await page.keyboard.press('ArrowDown')
  await expect(page.getByTestId('tv-more-menu')).toBeFocused()
  await page.keyboard.press('Escape')
  await expect(page.getByTestId('tv-more')).toBeFocused()
})

test('剪贴板筛选菜单：指针第一次打开焦点就在菜单里，↓ + Enter 选中', async ({ page }) => {
  await open(page, '/panel.html?scenario=clipboard.list')
  await page.getByTestId('clip-filter').click()
  await expect(page.getByTestId('clip-filter-menu')).toBeFocused()
  await page.keyboard.press('ArrowDown')
  await page.keyboard.press('Enter')
  await expect(page.getByTestId('clip-filter-menu')).toHaveCount(0)
})

test('OCR 头部 ⋯ → 识别历史…：打开设置 › OCR 的历史锚点并收起面板', async ({ page }) => {
  const calls = await open(page, '/panel.html?scenario=ocr.done')
  await page.getByTestId('ocr-meta-status').click()
  await page.getByTestId('ocr-more').click()
  await expect(page.getByTestId('ocr-more-menu')).toBeFocused()
  await page.getByTestId('ocr-more-menu-item-history').click()
  await expect
    .poll(() => calls.findLast(([c]) => c === 'window_open')?.[1])
    .toEqual({ target: { kind: 'settings', section: 'ocr', anchor: 'ocr_history' } })
})

test('设置深链锚点 ocr_history：滚到识别历史并聚焦分组标题，列表可见', async ({ page }) => {
  await open(page, '/settings.html?scenario=settings.ocr-history')
  const section = page.getByTestId('ocr-history-section')
  await expect(page.getByTestId('ocr-history-list')).toBeVisible()
  await expect(section.locator('.ui-section-title')).toBeFocused()
  await expect(section).toBeInViewport()
})

test('Tab 由外壳接管：两种引擎（含 macOS 默认「键盘导航」关闭的 WebKit）都按同一顺序经过卡片里的全部控件并回绕', async ({
  page,
}) => {
  await open(page, '/panel.html?scenario=translate.done')
  await page.getByTestId('tv-stream-body').click()
  await page.getByTestId('tv-langbar-source').focus()
  // 从第一个控件出发 Tab 一圈：记下顺序（中间的按钮也在内——WebKit 默认的 Tab 会跳过按钮，这里不交给浏览器）
  const order: Array<{ id: string | null; inCard: boolean }> = []
  for (let i = 0; i < 14; i++) {
    // oxlint-disable-next-line no-await-in-loop -- 按键依次发出
    await page.keyboard.press('Tab')
    order.push(
      // oxlint-disable-next-line no-await-in-loop -- 依次读取
      await page.evaluate(() => {
        const el = document.activeElement
        return {
          id: el?.getAttribute('data-testid') ?? null,
          inCard: !!el?.closest('[data-panel-content]'),
        }
      }),
    )
  }
  const ids = order.map((o) => o.id)
  expect(ids).toContain('tv-langbar-target')
  expect(ids).toContain('tv-more')
  expect(ids).toContain('tv-expand')
  // 回绕：最后一个之后回到第一个
  const back = ids.indexOf('tv-langbar-source')
  expect(back).toBeGreaterThan(0)
  expect(ids[back - 1]).toBe('tv-expand')
  // 焦点从不离开卡片（没有落到 body 或窗口外）
  expect(order.every((o) => o.inCard)).toBe(true)
  // 同一顺序：两圈完全一致
  const lap = ids.slice(0, back + 1)
  expect(ids.slice(back + 1, back + 1 + lap.length)).toEqual(lap.slice(0, ids.length - back - 1))
})

// ---------------------------------------------------------------------------
// 逐帧用例（rAF 采样）

/** 在页面里等第一个指针按下 / 按键，之后逐帧记录 fn 的返回值，持续 ms 毫秒 */
async function sampleAfterInput(page: Page, probe: string, ms: number) {
  return page.evaluate(
    ([source, duration]) =>
      new Promise<Array<{ t: number; v: number }>>((resolve) => {
        // oxlint-disable-next-line no-new-func -- 用例传入的采样表达式（只在测试页面里执行）
        const read = new Function(`return (${source})`) as () => number
        const out: Array<{ t: number; v: number }> = []
        const start = (e: Event) => {
          const t0 = e.timeStamp
          const tick = (now: number) => {
            out.push({ t: now - t0, v: read() })
            if (now - t0 < duration) requestAnimationFrame(tick)
            else resolve(out)
          }
          requestAnimationFrame(tick)
        }
        addEventListener('pointerdown', start, { once: true, capture: true })
        addEventListener('keydown', start, { once: true, capture: true })
      }),
    [probe, ms] as const,
  )
}

/**
 * 同 sampleAfterInput，但在**这一帧的 rAF 回调全部执行完之后**读值：rAF 里记下 `performance.now()`（`w`）并 postMessage，
 * 消息任务在这一帧的渲染更新之后、下一帧之前执行。Motion 的帧循环在同一帧的 rAF 里按 `performance.now()` 推进动画
 * （motion-dom 的 frameloop），所以 `v` 就是 Motion 在 `w` 这一刻算出的位置——按 `w` 换算的速度与帧时间戳是否均匀（macOS
 * WebKit 的 rAF 时间戳会出现 9 / 17 ms 交替）、回调有没有被拖后（CI 的 Chromium）、采样回调排在动画回调之前还是之后都
 * 无关。`t` 仍是 rAF 的帧时间戳
 */
async function sampleMotionAfterInput(
  page: Page,
  probe: string,
  ms: number,
  input: () => Promise<void>,
) {
  type Sample = { t: number; w: number; v: number }
  type Slot = { __wispSample?: Promise<Sample[]> }
  // 先装好监听（这次 evaluate 返回时监听已在），再发输入：同一次 evaluate 里等结果时，负载极高的页面可能先处理到指针按下、
  // 后装监听，采样永远不开始（用 CDP 把 CPU 降速 20 倍可复现）
  await page.evaluate(
    ([source, duration]) => {
      ;(window as unknown as Slot).__wispSample = new Promise<Sample[]>((resolve) => {
        // oxlint-disable-next-line no-new-func -- 用例传入的采样表达式（只在测试页面里执行）
        const read = new Function(`return (${source})`) as () => number
        const out: Sample[] = []
        const channel = new MessageChannel()
        let pending: { t: number; w: number } | null = null
        let done = false
        channel.port1.onmessage = () => {
          if (pending) out.push({ ...pending, v: read() })
          pending = null
          if (done) resolve(out)
        }
        const start = (e: Event) => {
          const t0 = e.timeStamp
          const tick = (now: number) => {
            pending = { t: now - t0, w: performance.now() - t0 }
            channel.port2.postMessage(0)
            if (now - t0 < duration) requestAnimationFrame(tick)
            else done = true
          }
          requestAnimationFrame(tick)
        }
        addEventListener('pointerdown', start, { once: true, capture: true })
      })
    },
    [probe, ms] as const,
  )
  await input()
  return page.evaluate(() => (window as unknown as Slot).__wispSample ?? [])
}

for (const frame of ['native', 'css'] as const) {
  test(`翻译视图 ⋯ 菜单（${frame} 帧）：按下的下一帧就出现（opacity > 0），160 ms 内完整可见`, async ({
    page,
  }) => {
    await open(page, `/panel.html?scenario=translate.done&frame=${frame}`)
    await page.getByTestId('tv-stream-body').click()
    const box = await page.getByTestId('tv-more').boundingBox()
    if (!box) throw new Error('no trigger')
    const frames = sampleAfterInput(
      page,
      "Number(getComputedStyle(document.querySelector('[data-testid=tv-more-menu]') ?? document.body).opacity) * (document.querySelector('[data-testid=tv-more-menu]') ? 1 : 0)",
      400,
    )
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2)
    await page.mouse.down()
    const log = await frames
    await page.mouse.up()
    // 输入之后的第一或第二帧（headless 下 rAF 可能先补一帧旧的）菜单就开始淡入
    const first = log.findIndex((f) => f.v > 0)
    expect(first).toBeGreaterThanOrEqual(0)
    expect(first).toBeLessThanOrEqual(2)
    // menuIn 140 ms：160 ms 之后完全不透明
    expect(log.filter((f) => f.t > 200).every((f) => f.v > 0.99)).toBe(true)
    // 抬起之后菜单仍开着（不会被随后的 click 关掉）
    await expect(page.getByTestId('tv-more-menu')).toBeVisible()
  })
}

/**
 * 逐帧用例的采样轮数：一轮里出现长帧（系统负载高、headless 引擎丢帧）或采到的帧不够时整轮重采，不直接判失败
 * （CI 容器里的 WebKit 走软件合成，rAF 偶尔在 140 ms 的动效里只来 2–3 帧）
 */
const SAMPLE_ROUNDS = 3

test('Css 帧的面板退出：原地淡出 + 缩到 0.98，y 恒为 0，140 ms 左右结束（不往下沉）', async ({
  page,
}) => {
  type Exit = {
    frames: Array<{ t: number; y: number; s: number }>
    /** 外框被卸载的时刻与那一刻的内联 transform（Motion 逐帧写入，即退出动画的最后一帧）；2 s 内没卸载为 null */
    removed: { t: number; y: number; s: number; transform: string } | null
  }
  const rounds: Exit[] = []
  // 一轮：打开页面、请求隐藏，逐帧读外框的 transform，直到外框被卸载。卸载的那一刻由 MutationObserver 捕获（事件，与帧数
  // 无关）。帧太少（≤ 3）或饿帧时整轮重采，最多 3 轮
  /* oxlint-disable no-await-in-loop -- 采样轮次必须串行：每轮重新打开页面 */
  for (let round = 1; round <= SAMPLE_ROUNDS; round++) {
    await page.goto('/panel.html?scenario=translate.done&frame=css&os=macos&lang=zh-CN&hud=0')
    await page.waitForSelector('html[data-settled]')
    const exit = await page.evaluate(
      () =>
        new Promise<Exit>((resolve) => {
          const SEL = '[data-testid=panel-card-frame]'
          const frames: Exit['frames'] = []
          let removed: Exit['removed'] = null
          const t0 = performance.now()
          const matrix = (transform: string) => new DOMMatrix(transform || 'none')
          // 被移除的子树保留着 Motion 写的内联样式：外框卸载那一刻的 transform 就是退出动画的最后一帧
          const mo = new MutationObserver((records) => {
            for (const r of records)
              for (const n of r.removedNodes) {
                if (removed || !(n instanceof Element)) continue
                const el = n.matches(SEL) ? n : n.querySelector(SEL)
                if (!(el instanceof HTMLElement)) continue
                const m = matrix(el.style.transform)
                removed = {
                  t: performance.now() - t0,
                  y: m.m42,
                  s: m.m11,
                  transform: el.style.transform,
                }
              }
          })
          mo.observe(document.body, { childList: true, subtree: true })
          const tick = () => {
            const el = document.querySelector<HTMLElement>(SEL)
            if (!el || removed || performance.now() - t0 > 2000) {
              mo.disconnect()
              resolve({ frames, removed })
              return
            }
            const m = matrix(getComputedStyle(el).transform)
            frames.push({ t: performance.now() - t0, y: m.m42, s: m.m11 })
            requestAnimationFrame(tick)
          }
          ;(
            window as unknown as { __WISP_MOCK__: { ctx: { requestHide(r: string): void } } }
          ).__WISP_MOCK__.ctx.requestHide('escape')
          requestAnimationFrame(tick)
        }),
    )
    rounds.push(exit)
    if (exit.frames.length > 3 && !starved(exit.frames.map((f) => f.t))) break
  }
  /* oxlint-enable no-await-in-loop */
  for (const [i, r] of rounds.entries()) {
    // 退出播完、外框卸载了（没有卡在退出里）
    expect(r.removed, `第 ${i + 1} 轮：2 s 内外框没有卸载`).not.toBeNull()
    // 卸载那一刻的最后一帧（事件，不依赖采到几帧）：缩到 0.98，y 为 0
    expect(r.removed?.s ?? 1, `第 ${i + 1} 轮最后一帧 ${r.removed?.transform}`).toBeLessThan(0.99)
    expect(r.removed?.s ?? 0).toBeGreaterThanOrEqual(0.979)
    expect(Math.abs(r.removed?.y ?? 99), `第 ${i + 1} 轮最后一帧下沉`).toBeLessThan(0.5)
    // 逐帧（每一轮采到的全部帧）：原地——y 恒为 0、缩放不低于 0.98
    for (const f of r.frames) {
      expect(Math.abs(f.y), `第 ${i + 1} 轮 ${Math.round(f.t)} ms 下沉`).toBeLessThan(0.5)
      expect(f.s, `第 ${i + 1} 轮 ${Math.round(f.t)} ms 缩放`).toBeGreaterThanOrEqual(0.979)
    }
  }
  // 「140 ms 左右结束」是时序断言：只在帧数够、没有饿帧的那一轮判（fast 140 ms + 兜底余量之内卸载）；Motion 的动画只在帧里
  // 推进，饿帧时卸载的时刻跟着被拉长
  const valid = rounds.find((r) => r.frames.length > 3 && !starved(r.frames.map((f) => f.t)))
  if (valid) expect(valid.removed?.t ?? 999).toBeLessThan(140 + 120)
  else
    test.info().annotations.push({
      type: 'starved-frames',
      description: `${rounds.length} 轮都只采到 ${rounds.map((r) => r.frames.length).join(' / ')} 帧或饿帧（无 GPU 的容器里 headless 引擎走软件合成）：没有判「140 ms 左右结束」（逐帧与最后一帧的布局断言照常）`,
    })
})

/** 剪贴板前几行的不透明度（逐帧） */
const ROWS_OPACITY =
  "[...document.querySelectorAll('.clip-list .ui-vlist-row')].slice(0, 6).map((r) => Number(getComputedStyle(r.firstElementChild?.firstElementChild ?? r).opacity)).reduce((a, b) => Math.min(a, b), 1)"

test('剪贴板打开的 stagger：Esc 隐藏后再次唤出时同样播放（首屏行从淡入开始，约 170 ms 内到位）', async ({
  page,
}) => {
  await open(page, '/panel.html?scenario=clipboard.list')
  // 按事件锚定：等退出播完、面板隐藏之后再唤出（不按固定的 400 ms），从卡片进入 entering / shown 且有行
  // 的那一帧起逐帧采 500 ms——从 prepare 起采的话，饿帧的 WebKit 里 prepare 到显示就占掉大半，只剩 2–3 帧
  const run = await page.evaluate(
    ([probe]) =>
      new Promise<{ frames: Array<{ t: number; v: number }>; hiddenMs: number }>((resolve) => {
        // oxlint-disable-next-line no-new-func -- 测试页面里的采样表达式
        const read = new Function(`return (${probe})`) as () => number
        const m = (
          window as unknown as {
            __WISP_MOCK__: {
              ctx: { requestHide(r: string): void; preparePanel(p: unknown): number }
              panel: { session: { prepare: { payload: unknown } } | null }
            }
          }
        ).__WISP_MOCK__
        const phase = () =>
          document.querySelector('[data-testid=panel-card]')?.getAttribute('data-phase') ?? null
        const payload = m.panel.session?.prepare.payload
        const t0 = performance.now()
        m.ctx.requestHide('escape')
        const reopen = () => {
          const hiddenMs = performance.now() - t0
          const frames: Array<{ t: number; v: number }> = []
          let shownAt: number | null = null
          m.ctx.preparePanel(payload)
          const t1 = performance.now()
          const tick = () => {
            const now = performance.now()
            const p = phase()
            const rows = document.querySelectorAll('.clip-list .ui-vlist-row').length
            if (shownAt === null && (p === 'entering' || p === 'shown') && rows > 0) shownAt = now
            if (shownAt !== null) frames.push({ t: now - shownAt, v: read() })
            if (shownAt === null ? now - t1 < 5000 : now - shownAt < 500)
              requestAnimationFrame(tick)
            else resolve({ frames, hiddenMs })
          }
          requestAnimationFrame(tick)
        }
        const waitHidden = () => {
          const p = phase()
          if (p === null || p === 'hidden' || performance.now() - t0 > 3000) reopen()
          else requestAnimationFrame(waitHidden)
        }
        waitHidden()
      }),
    [ROWS_OPACITY] as const,
  )
  const dump = run.frames.map((f) => `${Math.round(f.t)}:${f.v.toFixed(2)}`).join(' ')
  expect(
    run.frames.length,
    `再次唤出之后 5 s 内面板没有显示（隐藏用了 ${Math.round(run.hiddenMs)} ms）`,
  ).toBeGreaterThan(0)
  // 首屏行从淡入开始：stagger 在显示的那次提交里（绘制之前）开始，显示之后的第一帧画的就是起点——无论采到几帧都成立
  expect(
    run.frames.some((f) => f.v < 0.95),
    `首屏行没有从淡入开始（${dump}）`,
  ).toBe(true)
  // 最后全部到位（会自动重试的断言，不依赖采到的帧）
  await expect
    // oxlint-disable-next-line no-new-func -- 测试页面里的采样表达式
    .poll(() =>
      page.evaluate((probe) => new Function(`return (${probe})`)() as number, ROWS_OPACITY),
    )
    .toBe(1)
  // 「约 170 ms 内到位」是时序断言：显示后 500 ms 的最后一帧已经到位；饿帧时不判
  if (
    !noteStarved(
      run.frames.map((f) => f.t),
      '「显示后 500 ms 内到位」',
    )
  )
    expect(run.frames.at(-1)?.v, dump).toBe(1)
})

test('识别历史实时插入：已有的行逐帧下移（没有一帧整行下跳），新行在顶部展开进来', async ({
  page,
}) => {
  await page.goto('/settings.html?scenario=settings.ocr-history&os=macos&lang=zh-CN&hud=0')
  const list = page.getByTestId('ocr-history-list')
  await expect(list).toBeVisible()
  const firstId = await list.locator('li[data-id]').first().getAttribute('data-id')
  // 场景在进入后 1.2 s 插入一条：从现在起逐帧记录原来第一行的位置
  const log = await page.evaluate(
    ([id]) =>
      new Promise<Array<{ t: number; y: number; gap: number; swapping: boolean }>>((resolve) => {
        const out: Array<{ t: number; y: number; gap: number; swapping: boolean }> = []
        const t0 = performance.now()
        const tick = () => {
          const li = document.querySelector(`li[data-id="${id}"]`)
          const ul = li?.parentElement
          const box = document.querySelector('[data-testid=ocr-history-box]')
          const inner = box?.querySelector('.ui-autoheight-inner')
          if (li && ul && box && inner)
            out.push({
              t: performance.now() - t0,
              y: li.getBoundingClientRect().top - ul.getBoundingClientRect().top,
              gap: box.getBoundingClientRect().height - inner.getBoundingClientRect().height,
              // 外框在「换状态」的高度过渡里（读取中 → 列表；AutoHeight 的 data-animating）：那是有意的高度过渡
              swapping: box.hasAttribute('data-animating'),
            })
          if (performance.now() - t0 < 2200) requestAnimationFrame(tick)
          else resolve(out)
        }
        requestAnimationFrame(tick)
      }),
    [firstId] as const,
  )
  const ys = log.map((f) => f.y)
  const moved = Math.max(...ys) - Math.min(...ys)
  // 插入了一整行（≥ 40 px）
  expect(moved).toBeGreaterThan(40)
  // 逐帧位移按经过的帧数折算后不超过 24 px（220 ms 的平移约 13 帧；一帧整行下跳是 60 px）
  for (let i = 1; i < log.length; i++) {
    const a = log[i - 1]
    const b = log[i]
    if (!a || !b) continue
    const frames = Math.max(1, Math.round((b.t - a.t) / (1000 / 60)))
    expect(Math.abs(b.y - a.y)).toBeLessThanOrEqual(24 * frames)
  }
  // 中间有过渡帧（不是一步到位）
  const between = ys.filter((y) => y > Math.min(...ys) + 2 && y < Math.max(...ys) - 2)
  expect(between.length).toBeGreaterThan(2)
  // 插入开始之后，外框逐帧与内容同高：不裁掉最后一行、不留白（外框若用第二条弹簧追内容，最后一行会被裁掉
  // 约 0.3 s）。还在「读取中 → 列表」换状态过渡里的帧（页面加载慢的环境里会与插入重叠）是有意的高度过渡，不计
  const y0 = log[0]?.y ?? 0
  const from = log.findIndex((f) => Math.abs(f.y - y0) > 0.5)
  expect(from).toBeGreaterThan(0)
  for (const f of log.slice(from))
    if (!f.swapping)
      expect(Math.abs(f.gap), `${Math.round(f.t)} ms 外框与内容差`).toBeLessThanOrEqual(1)
})

/** 逐帧记录识别历史外框与内层的高度：先执行 action（在页面里），持续 ms 毫秒 */
async function sampleHistoryBox(page: Page, action: string, ms: number) {
  return page.evaluate(
    ([source, duration]) =>
      new Promise<Array<{ t: number; box: number; inner: number }>>((resolve) => {
        const box = document.querySelector('[data-testid=ocr-history-box]')
        const inner = box?.querySelector('.ui-autoheight-inner')
        if (!box || !inner) throw new Error('no history box')
        const out: Array<{ t: number; box: number; inner: number }> = []
        const t0 = performance.now()
        const tick = () => {
          out.push({
            t: performance.now() - t0,
            box: box.getBoundingClientRect().height,
            inner: inner.getBoundingClientRect().height,
          })
          if (performance.now() - t0 < duration) requestAnimationFrame(tick)
          else resolve(out)
        }
        // oxlint-disable-next-line no-new-func -- 用例传入的操作（只在测试页面里执行）
        new Function(source)()
        requestAnimationFrame(tick)
      }),
    [action, ms] as const,
  )
}

/**
 * 最后一次变化（外框或内层高度变化超过 0.5 px）的时刻。读到变化的那一帧的状态最早属于上一个采样的时刻（frames.ts 的
 * stateTime：长帧里采样回调可能排在动画回调之前），按它算
 */
function settledAt(log: Array<{ t: number; box: number; inner: number }>): number {
  const stamps = log.map((f) => f.t)
  let at = 0
  for (let i = 1; i < log.length; i++) {
    const a = log[i - 1]
    const b = log[i]
    if (a && b && (Math.abs(b.box - a.box) > 0.5 || Math.abs(b.inner - a.inner) > 0.5))
      at = stateTime(stamps, i)
  }
  return at
}

test('识别历史：展开 / 收起 / 删除一行时外框与内容逐帧同高（不裁切、不留白），0.5 s 内结束', async ({
  page,
}) => {
  await page.goto('/settings.html?scenario=settings.ocr-history&os=macos&lang=zh-CN&hud=0')
  await expect(page.getByTestId('ocr-history-list')).toBeVisible()
  // 场景在进入后 1.2 s 实时插入一条：等它展开完
  await page.waitForTimeout(2200)
  const first = "document.querySelector('.st-ocrh-list > li[data-id] .st-ocrh-main').click()"
  for (const [name, action] of [
    ['展开', first],
    ['收起', first],
    [
      '删除',
      "document.querySelector('.st-ocrh-list > li[data-id] [data-testid^=ocr-history-delete]').click()",
    ],
  ] as const) {
    // oxlint-disable-next-line no-await-in-loop -- 依次操作
    const log = await sampleHistoryBox(page, action, 900)
    const stamps = log.map((f) => f.t)
    const heights = log.map((f) => f.inner)
    // 确实动了（展开 / 收起几百 px，删除一整行）
    expect(Math.max(...heights) - Math.min(...heights), name).toBeGreaterThan(40)
    for (const f of log) {
      // 外框比内容矮 = 裁掉内容；比内容高 = 留白
      expect(f.inner - f.box, `${name} ${Math.round(f.t)} ms 裁切`).toBeLessThanOrEqual(1)
      expect(f.box - f.inner, `${name} ${Math.round(f.t)} ms 留白`).toBeLessThanOrEqual(1)
    }
    // 「0.5 s 内结束」是时序断言：动效进行期间（操作到结束）饿帧的环境（无 GPU 的容器）里不判，布局断言照常
    const end = settledAt(log)
    if (
      !noteStarved(
        stamps.filter((t) => t <= end + 2 * FRAME_MS),
        `「${name} 0.5 s 内结束」`,
      )
    )
      expect(end, name).toBeLessThanOrEqual(500)
    // oxlint-disable-next-line no-await-in-loop -- 等上一次的退场 / 提示结束
    await page.waitForTimeout(300)
  }
})

test('识别历史：换状态（列表 → 清空后的空状态）时外框高度过渡，结束后回到 height: auto', async ({
  page,
}) => {
  await page.goto('/settings.html?scenario=settings.ocr-history&os=macos&lang=zh-CN&hud=0')
  await expect(page.getByTestId('ocr-history-list')).toBeVisible()
  await page.waitForTimeout(2200)
  // 经 mock 的事件清空（等同在别处清空），列表换成空状态
  const log = await sampleHistoryBox(
    page,
    "window.__WISP_MOCK__.bus.emit('ocr_history:changed', { change: 'cleared' })",
    900,
  )
  await expect(page.getByTestId('ocr-history-empty')).toBeVisible()
  // 有过渡帧：外框高度不是一步跳到空状态的高度
  const boxes = log.map((f) => f.box)
  const lo = Math.min(...boxes)
  const hi = Math.max(...boxes)
  expect(hi - lo).toBeGreaterThan(100)
  expect(boxes.filter((b) => b > lo + 2 && b < hi - 2).length).toBeGreaterThan(2)
  // 结束后外框回到 height: auto（不再是显式高度），与内容同高
  const style = await page
    .getByTestId('ocr-history-box')
    .evaluate((el) => (el as HTMLElement).style.height)
  expect(style).toBe('')
  const last = log.at(-1)
  expect(Math.abs((last?.box ?? 0) - (last?.inner ?? 1))).toBeLessThanOrEqual(1)
})

// ---------------------------------------------------------------------------
// 布局与浮层

for (const frame of ['css', 'native'] as const) {
  for (const scenario of ['clipboard.list', 'clipboard.empty'] as const) {
    test(`冷页面第一次 prepare（${frame} 帧 · ${scenario}）：ready 报卡片的真实高度，窗口 = 卡片 + 2·阴影边距`, async ({
      page,
    }) => {
      // mock 场景加载即 prepare：与回收 / 空闲卸载 / 看门狗重建后的第一次唤出是同一条路径（外壳挂载之前到达）
      const calls = await open(page, `/panel.html?scenario=${scenario}&frame=${frame}`)
      await expect.poll(() => calls.some(([c]) => c === 'panel_ready')).toBe(true)
      // 等高度跟随与窗口（#root）过渡完
      await page.waitForTimeout(600)
      const m = await page.evaluate(() => {
        const content = document.querySelector<HTMLElement>('[data-panel-content]')
        const root = document.getElementById('root')
        const margin =
          Number.parseFloat(
            getComputedStyle(document.documentElement).getPropertyValue('--shadow-margin'),
          ) || 0
        return {
          content: content?.offsetHeight ?? 0,
          root: root?.offsetHeight ?? 0,
          margin,
        }
      })
      const ready = calls.find(([c]) => c === 'panel_ready')?.[1] as { content_height: number }
      expect(m.content).toBeGreaterThan(200)
      // ready 报的就是卡片里内容的真实高度（不是视图的估计值 240）
      expect(ready.content_height).toBe(m.content)
      // 模拟窗口 = 卡片 + 2·阴影边距（Native 帧的边距为 0）
      expect(m.root).toBe(m.content + 2 * (frame === 'css' ? m.margin : 0))
      // Native：之后的 resize 与 ready 同值（窗口一步到位，不先以矮卡片出现再长高）
      for (const [c, a] of calls)
        if (c === 'panel_resize')
          expect((a as { content_height: number }).content_height).toBe(m.content)
    })
  }
}

test('Css 帧、居中放置、卡片随实时插入长高时，卡片顶边逐帧不动（向下生长，不会上移一帧）', async ({
  page,
}) => {
  await open(page, '/panel.html?scenario=clipboard.few&frame=css')
  await page.waitForTimeout(400)
  const log = await page.evaluate(
    () =>
      new Promise<Array<{ t: number; top: number; h: number }>>((resolve) => {
        const m = (
          window as unknown as { __WISP_MOCK__: { bus: { emit(e: string, p: unknown): void } } }
        ).__WISP_MOCK__
        const card = () => document.querySelector('[data-testid=panel-card]')
        const out: Array<{ t: number; top: number; h: number }> = []
        const t0 = performance.now()
        const now = Date.now()
        // 连插 4 条，每条间隔 120 ms
        for (let i = 0; i < 4; i++) {
          setTimeout(() => {
            const text = `新复制的第 ${i + 1} 条`
            m.bus.emit('clipboard:changed', {
              change: 'inserted',
              item: {
                id: 900 + i,
                kind: 'text',
                created_at: now + i,
                last_used_at: now + i,
                use_count: 1,
                pinned: false,
                source: null,
                preview: text,
                char_count: text.length,
                line_count: 1,
                truncated: false,
                image: null,
                files: null,
                link: null,
                color: null,
              },
            })
          }, i * 120)
        }
        const tick = () => {
          const r = card()?.getBoundingClientRect()
          if (r) out.push({ t: performance.now() - t0, top: r.top, h: r.height })
          if (performance.now() - t0 < 1400) requestAnimationFrame(tick)
          else resolve(out)
        }
        requestAnimationFrame(tick)
      }),
  )
  const heights = log.map((f) => f.h)
  // 确实长高了（窗口按 +48 阶梯放大、卡片在窗口里弹簧跟随）
  expect(Math.max(...heights) - Math.min(...heights)).toBeGreaterThan(60)
  const tops = log.map((f) => f.top)
  expect(Math.max(...tops) - Math.min(...tops)).toBeLessThanOrEqual(0.5)
})

/** 逐帧记录一个元素（选择器）在页面上的矩形，持续 ms 毫秒；action 在第一帧之前执行 */
async function sampleRect(page: Page, selector: string, ms: number) {
  return page.evaluate(
    ([sel, duration]) =>
      new Promise<Array<{ t: number; top: number; bottom: number; clip: string }>>((resolve) => {
        const out: Array<{ t: number; top: number; bottom: number; clip: string }> = []
        const t0 = performance.now()
        const tick = () => {
          const el = document.querySelector<HTMLElement>(sel)
          if (el) {
            const r = el.getBoundingClientRect()
            out.push({
              t: performance.now() - t0,
              top: r.top,
              bottom: r.bottom,
              clip: el.style.clipPath,
            })
          }
          if (performance.now() - t0 < duration) requestAnimationFrame(tick)
          else resolve(out)
        }
        requestAnimationFrame(tick)
      }),
    [selector, ms] as const,
  )
}

for (const scenario of ['ocr.done&lang=zh-CN', 'ocr.done&lang=en-US&os=windows'] as const) {
  test(`OCR 识别菜单（${scenario}）：整份菜单完整可见、不用滚动（矮面板里收起低频项）`, async ({
    page,
  }) => {
    await page.goto(`/panel.html?scenario=${scenario}&hud=0`)
    await page.waitForSelector('html[data-settled]')
    await page.getByTestId('ocr-recognize-menu').click()
    const list = page.getByTestId('ocr-recognize-list')
    await expect(list).toBeVisible()
    await page.waitForTimeout(500)
    const m = await list.evaluate((el) => {
      const menu = el.closest<HTMLElement>('.ui-menu') ?? el
      const card = document.querySelector('[data-panel-content]')?.getBoundingClientRect()
      const r = menu.getBoundingClientRect()
      return {
        scroll: menu.scrollHeight - menu.clientHeight,
        top: r.top,
        bottom: r.bottom,
        cardTop: card?.top ?? 0,
        cardBottom: card?.bottom ?? 0,
      }
    })
    // 不需要滚动（没有被截掉一半的项）、整份在卡片里
    expect(m.scroll).toBeLessThanOrEqual(1)
    expect(m.top).toBeGreaterThanOrEqual(m.cardTop)
    expect(m.bottom).toBeLessThanOrEqual(m.cardBottom + 1)
  })
}

test('翻译 ⋯ 菜单（Css 帧）：预留的卡片用 spring.snappy 长高，菜单 0.3 s 内完整露出；关闭淡出之后卡片才缩回', async ({
  page,
}) => {
  await open(page, '/panel.html?scenario=translate.done&frame=css')
  await page.getByTestId('tv-stream-body').click()
  const box = await page.getByTestId('tv-more').boundingBox()
  if (!box) throw new Error('no trigger')
  const frames = sampleAfterInput(
    page,
    "(() => { const m = document.querySelector('[data-testid=tv-more-menu]'); return m ? (m.style.clipPath ? 1 : 0) : -1 })()",
    600,
  )
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2)
  await page.mouse.down()
  const log = await frames
  await page.mouse.up()
  // 下拉帘在追赶期间裁切（有 clip-path 的帧），300 ms 之后不再裁切（完整露出）。读到裁切的那一帧的状态最早属于上一个
  // 采样的时刻（stateTime）
  expect(log.some((f) => f.v === 1)).toBe(true)
  const stamps = log.map((f) => f.t)
  const lastClipped = Math.max(...log.flatMap((f, i) => (f.v === 1 ? [stateTime(stamps, i)] : [])))
  // 下拉帘逐帧只写 clip-path（不读布局）：卡片长高期间不掉帧（相邻采样间隔不超过 3 帧）。都是时序断言：饿帧的环境
  // （无 GPU 的容器）里不判，下面关闭时的布局断言照常
  const early = log.filter((f) => f.t <= 320)
  if (
    !noteStarved(
      early.map((f) => f.t),
      '「0.3 s 内完整露出 / 长高期间不掉帧」',
    )
  ) {
    expect(lastClipped).toBeLessThanOrEqual(300)
    for (let i = 1; i < early.length; i++)
      expect((early[i]?.t ?? 0) - (early[i - 1]?.t ?? 0)).toBeLessThanOrEqual(50)
  }
  // 关闭：菜单淡出（menuOut 100 ms）期间卡片高度不变，之后才缩回
  const card = '[data-testid=panel-card]'
  const before = await page.locator(card).evaluate((el) => el.getBoundingClientRect().height)
  const closing = sampleRect(page, card, 500)
  await page.keyboard.press('Escape')
  const shrink = await closing
  const fading = shrink.filter((f) => f.t < 90)
  for (const f of fading) expect(Math.abs(f.bottom - f.top - before)).toBeLessThanOrEqual(0.5)
  const end = shrink.at(-1)
  expect((end?.bottom ?? 0) - (end?.top ?? 0)).toBeLessThan(before - 20)
})

test('剪贴板矮面板右键靠下的行：整份菜单上移到卡片内，完整可见、不用滚动', async ({ page }) => {
  await open(page, '/panel.html?scenario=clipboard.compact')
  await page.waitForTimeout(300)
  const row = page.locator('.clip-list .ui-vlist-row').nth(2)
  const r = await row.boundingBox()
  if (!r) throw new Error('no row')
  await page.mouse.click(r.x + 60, r.y + r.height / 2, { button: 'right' })
  const menu = page.getByTestId('clip-context-menu')
  await expect(menu).toBeVisible()
  await page.waitForTimeout(300)
  const m = await menu.evaluate((el) => {
    const card =
      el.closest('.panel-card')?.getBoundingClientRect() ??
      document.querySelector('.clip')?.getBoundingClientRect()
    const rect = el.getBoundingClientRect()
    return {
      scroll: el.scrollHeight - el.clientHeight,
      top: rect.top,
      bottom: rect.bottom,
      cardTop: card?.top ?? 0,
      cardBottom: card?.bottom ?? 0,
    }
  })
  expect(m.scroll).toBeLessThanOrEqual(1)
  expect(m.top).toBeGreaterThanOrEqual(m.cardTop)
  expect(m.bottom).toBeLessThanOrEqual(m.cardBottom + 1)
})

/** 设置侧栏的选中背景：最多采样轮数（比其他逐帧用例多——CI 的 Chromium 偶尔一轮里有被拖长的帧；另有时间预算） */
const NAV_ROUNDS = 5
/** Motion 的帧循环一帧最多推进 40 ms（motion-dom 的 maxElapsed）：比这更长的帧，动画走的时间少于墙钟，速度换算不成立 */
const MOTION_MAX_ELAPSED = 40

test('设置侧栏的选中背景：按下就以这一刻为起点滑动，逐帧速度连续（相邻帧速度比 ≤ 3，不会一帧冲出去、下一帧停住）', async ({
  page,
}) => {
  // 重采的时间预算（从用例开始算）：负载极高时（用 CDP 把 CPU 降速 20 倍可复现）打开页面就要十几秒、一轮要几秒，
  // 不能拖到用例超时（30 s）
  const deadline = Date.now() + 18_000
  await page.goto('/settings.html?scenario=settings.general&os=macos&lang=zh-CN&hud=0')
  await page.waitForSelector('[data-testid=settings-nav-ocr]')
  await page.waitForTimeout(1200)
  // 一轮：点一个导航项、采 500 ms。位置在每一帧的 rAF 回调之后读、按那一帧回调执行时的 performance.now() 换算速度（px/ms），
  // 与 Motion 推进动画用的是同一个时钟（sampleMotionAfterInput）：丢一帧时位移翻倍但速度不变；帧时间戳不均匀、回调被拖后
  // 又紧接着补上时也不会被当成跳变（例如 Chromium 帧长 16.6 / 16.7 ms 时出现过 1727 → 139 px/s，
  // WebKit 的 rAF 时间戳 9 / 17 ms 交替时出现过 2952 → 589 px/s，都是换算用的时钟与 Motion 的不同）。移动期间有帧长
  // 不可用的帧（超过 Motion 的单帧上限 40 ms，或短于半帧）时再采一轮，最多 5 轮（用例开始后 18 s 内），轮流点「OCR」「通用」；
  // 每一轮里可比较的相邻帧都参与判定
  type Step = { d: number; dw: number }
  type Pair = { a: number; b: number; dwA: number; dwB: number; round: number }
  const pairs: Pair[] = []
  let moved = 0
  let irregular = 0
  let lastSteps: Step[] = []
  let rounds = 0
  // 可比较的一步：位移 ≥ 2 px，帧长在半帧到 Motion 单帧上限之间（更长：动画走的时间少于墙钟；更短：读数的相对误差大）
  const timely = (x: Step) => x.dw >= FRAME_MS / 2 && x.dw <= MOTION_MAX_ELAPSED
  /* oxlint-disable no-await-in-loop -- 采样轮次必须串行：每轮从上一轮停下的位置出发 */
  for (let round = 1; round <= NAV_ROUNDS && Date.now() < deadline; round++) {
    rounds = round
    const target = round % 2 === 1 ? 'settings-nav-ocr' : 'settings-nav-general'
    const box = await page.getByTestId(target).boundingBox()
    if (!box) throw new Error('no nav')
    const log = await sampleMotionAfterInput(
      page,
      "new DOMMatrix(getComputedStyle(document.querySelector('.st-nav-indicator')).transform).m42",
      500,
      async () => {
        await page.mouse.move(box.x + 20, box.y + box.height / 2)
        await page.mouse.down()
        await page.mouse.up()
      },
    )
    const span = Math.max(...log.map((f) => f.v)) - Math.min(...log.map((f) => f.v))
    moved = Math.max(moved, span)
    const steps: Step[] = log.slice(1).map((f, i) => ({
      d: Math.abs(f.v - (log[i]?.v ?? 0)),
      dw: f.w - (log[i]?.w ?? 0),
    }))
    lastSteps = steps
    // 第一段移动的帧只有半帧（动画从按下的那一刻开始），不计
    const first = steps.findIndex((x) => x.d >= 0.5)
    const rest = steps.slice(first + 1)
    irregular = rest.filter((x) => x.d >= 2 && !timely(x)).length
    let found = 0
    for (let i = 1; i < rest.length; i++) {
      const p0 = rest[i - 1]
      const p1 = rest[i]
      if (!p0 || !p1 || p0.d < 2 || p1.d < 2 || !timely(p0) || !timely(p1)) continue
      pairs.push({ a: p0.d / p0.dw, b: p1.d / p1.dw, dwA: p0.dw, dwB: p1.dw, round })
      found++
    }
    if (span > 60 && irregular === 0 && found >= 2) break
  }
  /* oxlint-enable no-await-in-loop */
  // 确实滑过去了
  expect(moved).toBeGreaterThan(60)
  // 每一轮都有帧长不可用的帧、可比较的相邻帧不足（无 GPU 的容器里 headless WebKit 走软件合成、负载极高）：环境量不了逐帧
  // 速度，跳过（本地与能跑满帧率的环境照常判）。判定只看帧长，与选中背景怎么动无关，不会把真实的跳变藏起来
  const frameLens = lastSteps.map((x) => x.dw.toFixed(1)).join(' ')
  test.skip(
    pairs.length < 2 && irregular > 0,
    `连续 ${rounds} 轮都有帧长不可用的帧（超过 ${MOTION_MAX_ELAPSED} ms 或短于半帧；最后一轮帧长 ms：${frameLens}）：跳过「逐帧速度连续」；本地与能跑满帧率的环境照常判`,
  )
  expect(
    pairs.length,
    `可比较的相邻帧不足（最后一轮帧长 ms：${frameLens}）`,
  ).toBeGreaterThanOrEqual(2)
  for (const x of pairs)
    expect(
      Math.max(x.a, x.b) / Math.min(x.a, x.b),
      `第 ${x.round} 轮相邻两帧速度 ${(x.a * 1000).toFixed(0)} → ${(x.b * 1000).toFixed(0)} px/s（帧长 ${x.dwA.toFixed(1)} / ${x.dwB.toFixed(1)} ms）`,
    ).toBeLessThanOrEqual(3)
})

test('引导换步骤：两层只在低不透明度区间重叠（不出现两层都 ≥ 0.4 的帧），步骤条与底栏随新步骤淡入', async ({
  page,
}) => {
  type Row = { t: number; old: number; neu: number; stepper: number }
  let log: Row[] = []
  let longFrames = 0
  // 一轮：打开欢迎页、点「开始」、逐帧采 450 ms。帧间隔超过 3 帧时（CPU 争用下 headless 引擎丢帧，过渡帧可能整段被
  // 跳过）整轮重采，最多 3 轮；两层的不透明度在同一帧里读取，不受丢帧影响，照常逐帧判
  /* oxlint-disable no-await-in-loop -- 采样轮次必须串行：每轮重新打开欢迎页 */
  for (let round = 1; round <= SAMPLE_ROUNDS; round++) {
    await page.goto(
      '/settings.html?label=onboarding&scenario=onboarding.welcome&os=macos&lang=zh-CN&hud=0',
    )
    await page.waitForSelector('[data-testid=onboarding-start]')
    await page.waitForTimeout(500)
    const sampled = page.evaluate(
      () =>
        new Promise<Row[]>((resolve) => {
          const out: Row[] = []
          const op = (el: Element | null | undefined) =>
            el ? Number(getComputedStyle(el).opacity) : -1
          const start = (e: Event) => {
            const t0 = e.timeStamp
            const tick = (now: number) => {
              const hosts = [...document.querySelectorAll<HTMLElement>('.ob-step-host')]
              out.push({
                t: now - t0,
                old: op(hosts.find((h) => h.dataset.stepHost === 'welcome')),
                neu: op(hosts.find((h) => h.dataset.stepHost === 'permissions')),
                stepper: op(document.querySelector('.ob-stepper-slot > div')),
              })
              if (now - t0 < 450) requestAnimationFrame(tick)
              else resolve(out)
            }
            requestAnimationFrame(tick)
          }
          addEventListener('pointerdown', start, { once: true, capture: true })
        }),
    )
    await page.getByTestId('onboarding-start').click()
    log = await sampled
    longFrames = log.slice(1).filter((r, i) => r.t - (log[i]?.t ?? 0) > 3 * FRAME_MS).length
    if (longFrames === 0) break
  }
  /* oxlint-enable no-await-in-loop */
  // 交叉期间：没有两层都清晰可读的帧
  for (const f of log)
    if (f.old >= 0 && f.neu >= 0)
      expect(
        Math.min(f.old, f.neu),
        `${f.t.toFixed(0)} ms：旧 ${f.old} / 新 ${f.neu}`,
      ).toBeLessThan(0.4)
  // 步骤条不是第一帧就不透明：有过渡帧
  const stepper = log.map((f) => f.stepper).filter((v) => v >= 0)
  expect(
    stepper.some((v) => v > 0.05 && v < 0.95),
    `步骤条没有过渡帧（${stepper.map((v) => v.toFixed(2)).join(' ')}；最后一轮 ${longFrames} 个长帧）`,
  ).toBe(true)
  expect(stepper.at(-1)).toBe(1)
})

test('OCR 窄面板：识别 / 翻译结束、用户没有手动滚动时文本区回到开头（第一行不被裁掉）', async ({
  page,
}) => {
  await page.goto('/panel.html?scenario=ocr.narrow&os=macos&lang=zh-CN&hud=0')
  await page.waitForSelector('html[data-settled]')
  await page.waitForTimeout(800)
  const boxes = await page.evaluate(() =>
    [...document.querySelectorAll<HTMLElement>('.ocr-box, .ocr-text')].map((el) => ({
      overflow: el.scrollHeight - el.clientHeight,
      top: el.scrollTop,
    })),
  )
  const overflowing = boxes.filter((b) => b.overflow > 1)
  for (const b of overflowing) expect(b.top).toBeLessThanOrEqual(1)
})

for (const scenario of ['translate.input-hint', 'translate.done'] as const) {
  test(`头部在 360 宽 + 伪本地化下截断而不重叠（${scenario}）：头部的控件矩形两两不相交、都在卡片内`, async ({
    page,
  }) => {
    // 伪本地化把每段文案拉长约 40%（更长的界面语言 / 大字号的替身）：标题先截断、语言名随后截断，图标按钮不收缩
    await page.goto(`/panel.html?scenario=${scenario}&panel_width=360&os=macos&lang=pseudo&hud=0`)
    await page.waitForSelector('html[data-settled]')
    await page.waitForTimeout(400)
    const rects = await page.evaluate(() => {
      const header = document.querySelector('.tv-header')
      if (!header) return null
      const card = header.getBoundingClientRect()
      const items = [
        ...header.querySelectorAll<HTMLElement>('.tv-title, button, [role=button]'),
      ].filter((el) => el.offsetParent !== null)
      return {
        card: { left: card.left, right: card.right },
        items: items.map((el) => {
          const r = el.getBoundingClientRect()
          return {
            id: el.dataset.testid ?? el.className,
            left: r.left,
            right: r.right,
            top: r.top,
            bottom: r.bottom,
          }
        }),
      }
    })
    if (!rects) throw new Error('no header')
    expect(rects.items.length).toBeGreaterThanOrEqual(3)
    for (const a of rects.items) {
      expect(a.left, `${a.id} 左缘`).toBeGreaterThanOrEqual(rects.card.left - 0.5)
      expect(a.right, `${a.id} 右缘`).toBeLessThanOrEqual(rects.card.right + 0.5)
    }
    // 嵌套的（语言条里的两个按钮在 .tv-header 下都会被选到，它们本身互不包含）两两比较水平区间
    for (let i = 0; i < rects.items.length; i++)
      for (let j = i + 1; j < rects.items.length; j++) {
        const a = rects.items[i]
        const b = rects.items[j]
        if (!a || !b) continue
        const overlap = Math.min(a.right, b.right) - Math.max(a.left, b.left)
        const vertical = Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top)
        expect(
          overlap > 0.5 && vertical > 0.5,
          `${a.id} 与 ${b.id} 重叠 ${overlap.toFixed(1)} px`,
        ).toBe(false)
      }
  })
}
