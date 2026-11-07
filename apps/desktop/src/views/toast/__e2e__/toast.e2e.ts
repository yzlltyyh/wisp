/**
 * toast 视图的浏览器级用例：折行与高度只有真实引擎排得出来（jsdom 不排版）。
 * - 连续粘贴进度放在屏幕角落，Rust 给的卡片高度上限很小（placement::NOTICE_MAX_HEIGHT = 88）；第一条之后的进度只有一行，
 *   但这一次有说明（留到下一次 / 粘贴键不对）时说明必须完整显示：标题 + 至多两行说明放得进上限，不被截掉。
 * 运行方式见 apps/desktop/playwright.config.ts。
 */
import { expect, test, type Page } from '@playwright/test'

test.use({ viewport: { width: 1280, height: 800 } })

/** 角落提示的卡片高度上限（与 placement.rs 的 NOTICE_MAX_HEIGHT、mock 场景的 maxHeight 一致） */
const NOTICE_MAX_HEIGHT = 88

async function open(page: Page, scenario: string, os: string, lang: string): Promise<void> {
  await page.goto(`/panel.html?scenario=${scenario}&os=${os}&lang=${lang}&hud=0`)
  await page.waitForSelector('html[data-settled]')
  await page.evaluate(() => document.fonts.ready.then(() => undefined))
}

/** 说明的行数、内容区的布局高度（外壳上报给 Rust 的就是它，超过上限的部分会被卡片截掉） */
async function measure(page: Page) {
  return page.evaluate(() => {
    const detail = document.querySelector<HTMLElement>('[data-testid="toast-detail"]')
    const content = document.querySelector<HTMLElement>('[data-panel-content]')
    if (!detail || !content) return null
    const line = parseFloat(getComputedStyle(detail).lineHeight)
    return {
      text: detail.textContent ?? '',
      lines: Math.round(detail.offsetHeight / line),
      content: content.offsetHeight,
      single: document.querySelector('[data-testid="view-toast"]')?.hasAttribute('data-single'),
    }
  })
}

const CASES = [
  // 判断没粘贴进去：第 2 条之后（单行规则本来会去掉说明）
  {
    scenario: 'clipboard.queue-kept',
    os: 'macos',
    note: { zh: '留到下一次', en: 'stays for next paste' },
  },
  {
    scenario: 'clipboard.queue-kept',
    os: 'windows',
    note: { zh: '留到下一次', en: 'stays for next paste' },
  },
  // 粘贴键不对：说明里带前台应用自己的粘贴键，是最长的一种说明
  {
    scenario: 'clipboard.queue-key-mismatch',
    os: 'windows',
    note: { zh: 'Shift+Insert', en: 'Shift+Insert' },
  },
  {
    scenario: 'clipboard.queue-key-mismatch',
    os: 'linux',
    note: { zh: 'Shift+Insert', en: 'Shift+Insert' },
  },
] as const

for (const c of CASES) {
  for (const lang of ['zh-CN', 'en-US'] as const) {
    test(`连续粘贴的说明不被单行规则与角落的高度上限截掉：${c.scenario} · ${c.os} · ${lang}`, async ({
      page,
    }) => {
      await open(page, c.scenario, c.os, lang)
      await expect(page.getByTestId('toast-detail')).toBeVisible()
      const m = await measure(page)
      expect(m).not.toBeNull()
      if (!m) return
      expect(m.text).toContain(lang === 'zh-CN' ? c.note.zh : c.note.en)
      expect(m.single).toBe(false)
      expect(m.lines).toBeGreaterThanOrEqual(1)
      expect(m.lines).toBeLessThanOrEqual(2)
      expect(m.content).toBeLessThanOrEqual(NOTICE_MAX_HEIGHT)
    })
  }
}
