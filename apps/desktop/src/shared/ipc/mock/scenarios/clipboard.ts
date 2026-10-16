/**
 * mock 场景：剪贴板视图与 toast 视图。
 *
 * 每个场景 `{ id, label, view, setup(ctx) }`，id 以 `clipboard.` 开头；用 `?scenario=<id>` 打开，配合 `theme` / `os` /
 * `lang` / `reduced` 参数覆盖浅色 / 深色、mac / win、中英文与减少动效。
 *
 * - 数据：`ClipDb` 是一个小的「Rust 侧」替身——按 Rust 的顺序（收藏在前、最近使用在前）分页、检索（大小写不敏感的
 *   子串，收藏的名称也能搜到；命中在预览之外时给出以 … 开头的上下文片段，与 Rust 的检索一致）、详情、粘贴（整条 / 变换
 *   后 / 片段 / 多条合并；只复制时按 Rust 的做法显示 toast 视图）、检视（mock/smart.ts 的正则近似）、收藏与命名、删除、
 *   清空、暂停，并推送 `clipboard:changed`；
 * - 交互：场景用真实的 DOM 事件驱动视图（键盘事件走统一的键盘路由，指针事件走列表的事件委托），截图前等动效结束；
 * - `clipboard.scroll-10k` 是流畅度场景（`pnpm qa:shots --perf`）：1 万条混合行高（44 / 64）+ 分组标题，连续滚动之后按住 ↓，期间
 *   断言 DOM 行节点数 ≤ 可视行数 × 3 + 分组标题数、没有原图请求，结果写在 `<html data-qa-*>` 上；
 * - `clipboard.paste-toast-*`：每个 `CopiedReason` 一个 toast 场景（中英文由 `lang` 参数切换）；
 * - 收藏、芯片、多选、「粘贴为…」、命名：`clipboard.favorites-*`、`clipboard.chips-otp`、`clipboard.multi-select`、
 *   `clipboard.paste-as`、`clipboard.rename`；
 * - 悬浮剪贴板与连续粘贴：`clipboard.float`、`clipboard.float-queue`（clipfloat 入口）、`clipboard.queue-start`、
 *   `clipboard.queue-toast`。
 */
import { matchesFilter, startOfDay } from '../../../../views/clipboard/model'
import { mockInspect } from '../smart'
import type {
  ClipboardPayload,
  ClipFilter,
  ClipHit,
  ClipItemDetail,
  ClipItemMeta,
  ClipKind,
  ClipPage,
  CopiedReason,
  FileRef,
  PanelPayload,
  PasteMode,
  PasteOutcome,
  SourceApp,
} from '../../types'
import { appIcon, thumbnail } from '../fixtures'
import type { MockContext, PrepareOptions, Scenario } from '../types'
import { mockError } from '../types'

// ---------------------------------------------------------------------------
// 样例数据

const MINUTE = 60_000
const HOUR = 60 * MINUTE
const DAY = 24 * HOUR

interface Seed {
  kind: ClipKind
  preview: string
  /** 完整正文（预览之外还有内容的长文本 / 多行文本） */
  text?: string
  at: number
  pinned?: boolean
  app?: SourceApp | null
  image?: { width: number; height: number; hue: number; thumb?: boolean }
  files?: FileRef[]
  link?: string
  color?: string
  use?: number
  truncated?: boolean
  /** 收藏的名称 */
  name?: string
  /** 验证码：多少分钟后在本机自动删除 */
  expiresIn?: number
}

function apps(os: string) {
  const app = (mac: string, win: string, name: string, hue: number, winName = name): SourceApp => ({
    id: os === 'macos' ? mac : win,
    name: os === 'macos' ? name : winName,
    icon_url: appIcon(name, hue),
  })
  return {
    wechat: app('com.tencent.xinWeChat', 'wechat.exe', '微信', 140),
    safari: app('com.apple.Safari', 'msedge.exe', 'Safari', 205, 'Edge'),
    code: app('com.microsoft.VSCode', 'code.exe', 'VS Code', 210),
    figma: app('com.figma.Desktop', 'figma.exe', 'Figma', 12),
    notes: app('com.apple.Notes', 'notepad.exe', '备忘录', 45, 'Notepad'),
    finder: app('com.apple.finder', 'explorer.exe', 'Finder', 215, 'Explorer'),
    slack: app('com.tinyspeck.slackmacgap', 'slack.exe', 'Slack', 300),
    mail: app('com.apple.mail', 'outlook.exe', 'Mail', 195, 'Outlook'),
    shot: app('com.apple.screenshot.launcher', 'snippingtool.exe', '截屏', 280, 'Snipping Tool'),
    terminal: app('com.apple.Terminal', 'WindowsTerminal.exe', 'Terminal', 160),
  }
}

const LONG_TEXT = [
  'Design principle: every trigger gets immediate feedback.',
  '',
  'Streaming output renders as it arrives, and motion serves perception rather than decoration.',
  'The panel never steals focus from the app you are working in; it appears next to the cursor,',
  'waits quietly, and disappears the moment you are done.',
  '',
  '设计原则：每一次触发都要立即有反馈。流式输出边到边显示，动效服务于感知而不是装饰。',
  '面板从不抢走你正在使用的应用的焦点；它出现在光标旁，安静地等着，用完即走。',
  '',
  ...Array.from(
    { length: 40 },
    (_, i) =>
      `${i + 1}. Latency budget check — prepare ≤ 40 ms, first token p95 ≤ 1.2 s, idle CPU ≈ 0% (round ${i + 1}).`,
  ),
].join('\n')

const CODE_TEXT = `export function useCopy(): (text: string) => Promise<boolean> {
  return copyText
}

async function copyText(text: string): Promise<boolean> {
  try {
    await clipboardCopyText(text)
    return true
  } catch (error) {
    return canFallBack(error) && (await writeViaWebView(text))
  }
}`

const compact = (text: string) => text.replace(/\s+/g, ' ').trim()

function seeds(os: string, now: number): Seed[] {
  const a = apps(os)
  const today = startOfDay(now)
  const yesterday = today - DAY
  const docs =
    os === 'windows'
      ? 'C:\\Users\\me\\Documents\\'
      : os === 'macos'
        ? '/Users/me/Documents/'
        : '/home/me/Documents/'
  // 「今天」的条目离现在不超过当天已过去的时间（凌晨运行时也都落在今天）
  const ago = (m: number) => Math.max(today + 1000, now - m * MINUTE)
  return [
    {
      kind: 'text',
      preview: '收货地址：上海市徐汇区漕溪北路 88 号 12 层 1203 室',
      at: now - 3 * DAY,
      pinned: true,
      app: a.wechat,
      use: 14,
    },
    {
      kind: 'link',
      preview: 'https://github.com/yzlltyyh/wisp',
      link: 'https://github.com/yzlltyyh/wisp',
      at: now - 9 * DAY,
      pinned: true,
      app: a.safari,
      use: 6,
    },
    {
      kind: 'code',
      preview: compact(CODE_TEXT).slice(0, 200),
      text: CODE_TEXT,
      at: ago(2),
      app: a.code,
    },
    {
      kind: 'text',
      preview: 'Latency budget: prepare ≤ 40 ms · first token p95 ≤ 1.2 s · idle CPU ≈ 0%',
      at: ago(14),
      app: a.notes,
    },
    { kind: 'color', preview: '#3B82F6', color: '#3B82F6', at: ago(21), app: a.figma },
    {
      kind: 'image',
      preview: '',
      image: { width: 2560, height: 1440, hue: 220 },
      at: ago(33),
      app: a.shot,
    },
    {
      kind: 'files',
      preview: '季度报告.pdf, design-v3.fig',
      files: [
        { path: `${docs}季度报告.pdf`, name: '季度报告.pdf', exists: true, size: 2_418_112 },
        { path: `${docs}design-v3.fig`, name: 'design-v3.fig', exists: true, size: 18_874_368 },
      ],
      at: ago(47),
      app: a.finder,
    },
    {
      kind: 'rich_text',
      preview: 'Wisp 让翻译跟手出现：选中、按键、译文就在光标旁。',
      at: ago(68),
      app: a.notes,
    },
    {
      kind: 'text',
      preview: compact(LONG_TEXT).slice(0, 200),
      text: LONG_TEXT,
      at: yesterday + 17 * HOUR + 42 * MINUTE,
      app: a.safari,
    },
    {
      kind: 'link',
      preview: 'https://developer.apple.com/documentation/screencapturekit',
      link: 'https://developer.apple.com/documentation/screencapturekit',
      at: yesterday + 15 * HOUR + 5 * MINUTE,
      app: a.safari,
    },
    {
      kind: 'text',
      preview: "Thanks for the quick turnaround — let's ship it on Friday.",
      at: yesterday + 10 * HOUR + 30 * MINUTE,
      app: a.slack,
    },
    {
      kind: 'image',
      preview: '',
      image: { width: 4032, height: 3024, hue: 32 },
      at: today - 3 * DAY + 9 * HOUR,
      app: a.wechat,
    },
    {
      kind: 'code',
      preview: '$ pnpm qa:shots --filter clipboard --perf',
      at: today - 4 * DAY + 20 * HOUR,
      app: a.terminal,
    },
    {
      kind: 'files',
      preview: '旧版安装包.dmg',
      files: [{ path: `${docs}旧版安装包.dmg`, name: '旧版安装包.dmg', exists: false, size: null }],
      at: today - 5 * DAY,
      app: a.finder,
    },
    {
      kind: 'text',
      preview: 'ありがとうございます。明日またご連絡いたします。',
      at: today - 12 * DAY,
      app: a.mail,
    },
    {
      kind: 'color',
      preview: 'oklch(0.72 0.17 152)',
      color: 'oklch(0.72 0.17 152)',
      at: today - 20 * DAY,
      app: a.figma,
    },
    {
      kind: 'text',
      preview: '会议纪要：剪贴板视图走查，确认搜索高亮、置顶与快捷粘贴的交互细节。',
      at: today - 40 * DAY,
      app: null,
    },
  ]
}

function toMeta(seed: Seed, id: number): ClipItemMeta {
  const text = seed.text ?? seed.preview
  const binary = seed.kind === 'image' || seed.kind === 'files'
  return {
    id,
    kind: seed.kind,
    created_at: seed.at,
    last_used_at: seed.at,
    use_count: seed.use ?? 1,
    pinned: seed.pinned ?? false,
    source: seed.app ?? null,
    preview: seed.preview,
    char_count: binary ? null : text.length,
    line_count: binary ? null : text.split('\n').length,
    truncated: seed.truncated ?? false,
    image: seed.image
      ? {
          width: seed.image.width,
          height: seed.image.height,
          bytes: Math.round(seed.image.width * seed.image.height * 0.45),
          thumb_url:
            seed.image.thumb === false
              ? null
              : thumbnail(seed.image.width, seed.image.height, seed.image.hue),
        }
      : null,
    files: seed.files?.slice(0, 5) ?? null,
    link: seed.link ?? null,
    color: seed.color ?? null,
    ...(seed.name ? { name: seed.name } : {}),
    ...(seed.expiresIn !== undefined ? { expires_at: seed.at + seed.expiresIn * MINUTE } : {}),
  }
}

// ---------------------------------------------------------------------------
// 「Rust 侧」替身

class ClipDb {
  items: ClipItemMeta[] = []
  readonly texts = new Map<number, string>()
  readonly files = new Map<number, FileRef[]>()
  /** 图片的色相：原图与缩略图用同一个（悬停 / 预览里「缩略图打底 → 原图淡入」不会变色） */
  readonly hues = new Map<number, number>()
  paused = false
  nextId = 1
  /** 详情请求次数（快速滚动期间应为 0） */
  detailCalls = 0
  /** 检索的人为延迟（加载态场景） */
  searchDelay = 0
  /** 让某个命令失败一次（错误态场景） */
  failNext: string | null = null

  constructor(private readonly ctx: MockContext) {}

  add(seed: Seed): ClipItemMeta {
    const meta = toMeta(seed, this.nextId++)
    if (seed.text) this.texts.set(meta.id, seed.text)
    if (seed.files) this.files.set(meta.id, seed.files)
    if (seed.image) this.hues.set(meta.id, seed.image.hue)
    this.items.push(meta)
    return meta
  }

  sorted(): ClipItemMeta[] {
    return this.items.sort(
      (x, y) =>
        Number(y.pinned) - Number(x.pinned) || y.last_used_at - x.last_used_at || y.id - x.id,
    )
  }

  page(offset: number, limit: number, filter: ClipFilter): ClipPage {
    const all = this.sorted().filter((it) => matchesFilter(it, filter))
    return {
      items: all.slice(offset, offset + limit).map((item) => ({ item, highlights: [] })),
      pinned_count: all.filter((it) => it.pinned).length,
      next_offset: offset + limit < all.length ? offset + limit : null,
      query: null,
      filter,
    }
  }

  search(query: string, filter: ClipFilter, limit: number): ClipPage {
    const q = query.trim().toLowerCase()
    const hits: ClipHit[] = []
    for (const item of this.sorted()) {
      if (!matchesFilter(item, filter)) continue
      const hit = this.match(item, q)
      if (hit) hits.push(hit)
      if (hits.length >= limit) break
    }
    return {
      items: hits,
      pinned_count: hits.filter((h) => h.item.pinned).length,
      next_offset: null,
      query,
      filter,
    }
  }

  /**
   * 命中在预览里时高亮预览；在预览之外时给出以 … 开头的上下文片段（≤ 200 字），高亮针对片段；收藏的名称命中时不带
   * 高亮（名称的高亮由前端按查询词计算）
   */
  private match(item: ClipItemMeta, q: string): ClipHit | null {
    const at = item.preview.toLowerCase().indexOf(q)
    if (at >= 0) return { item, highlights: [{ start: at, end: at + q.length }] }
    if (item.pinned && item.name?.toLowerCase().includes(q)) return { item, highlights: [] }
    const full = compact(this.texts.get(item.id) ?? '')
    const pos = full.toLowerCase().indexOf(q)
    if (pos < 0) return null
    const from = Math.max(0, pos - 24)
    const snippet = `…${full.slice(from, from + 199)}`
    const start = pos - from + 1
    return { item: { ...item, preview: snippet }, highlights: [{ start, end: start + q.length }] }
  }

  detail(id: number): ClipItemDetail | null {
    this.detailCalls++
    const meta = this.items.find((it) => it.id === id)
    if (!meta) return null
    const textual = meta.kind !== 'image' && meta.kind !== 'files'
    return {
      meta,
      text: textual ? (this.texts.get(id) ?? meta.preview) : null,
      has_html: meta.kind === 'rich_text',
      has_rtf: false,
      image_url: meta.image
        ? thumbnail(meta.image.width, meta.image.height, this.hues.get(id) ?? (id * 47) % 360)
        : null,
      files: this.files.get(id) ?? meta.files,
    }
  }

  find(id: number): ClipItemMeta {
    const it = this.items.find((x) => x.id === id)
    if (!it) throw mockError('not_found', `mock: clip ${id} not found`)
    return it
  }

  private fail(cmd: string): void {
    if (this.failNext === cmd) {
      this.failNext = null
      throw mockError('internal', `mock: ${cmd} failed`)
    }
  }

  /** Rust：粘贴时立即隐藏面板；只复制时在原位置以 toast 视图提示 */
  private pasted(ids: readonly number[], mode: PasteMode): PasteOutcome {
    for (const id of ids) {
      const item = this.find(id)
      item.last_used_at = Date.now()
      item.use_count += 1
      this.ctx.emit('clipboard:changed', { change: 'touched', item: { ...item } })
    }
    this.ctx.requestHide('paste')
    if (mode === 'copy_only') {
      this.ctx.after(260, () => showToast(this.ctx, 'copy_only'))
      return { type: 'copied', reason: 'copy_only' }
    }
    return { type: 'pasted' }
  }

  /** 实时插入一条（相当于用户在别处复制了内容） */
  insert(seed: Seed): void {
    const meta = this.add(seed)
    this.ctx.emit('clipboard:changed', { change: 'inserted', item: meta })
  }

  install(): void {
    const { ctx } = this
    ctx.handle('clipboard_recent', ({ offset, limit, filter }) => this.page(offset, limit, filter))
    ctx.handle('clipboard_search', async ({ query, filter, limit }) => {
      if (this.searchDelay > 0) await new Promise((r) => setTimeout(r, this.searchDelay))
      return this.search(query, filter, limit)
    })
    ctx.handle('clipboard_get', ({ id }) => this.detail(id))
    ctx.handle('clipboard_paste', ({ id, mode }) => this.pasted([id], mode))
    // 片段与合并粘贴：结果不进历史，只更新来源条目的最近使用时间
    ctx.handle('clipboard_paste_text', ({ from, mode }) =>
      this.pasted(from === null ? [] : [from], mode),
    )
    ctx.handle('clipboard_paste_many', ({ ids, mode }) => this.pasted(ids, mode))
    ctx.handle('clipboard_inspect', ({ id }) => {
      const item = this.find(id)
      const binary = item.kind === 'image' || item.kind === 'files'
      return mockInspect(item.kind, binary ? null : (this.texts.get(id) ?? item.preview))
    })
    ctx.handle('clipboard_pin', ({ id, pinned }) => {
      this.fail('clipboard_pin')
      const item = this.find(id)
      item.pinned = pinned
      // 与 Rust 一致：收藏去掉验证码的到期时间，取消收藏去掉名称
      if (pinned) delete item.expires_at
      else delete item.name
      return { ...item }
    })
    ctx.handle('clipboard_set_name', ({ id, name }) => {
      this.fail('clipboard_set_name')
      const item = this.find(id)
      const clean = name?.trim() ?? ''
      if (clean) {
        item.name = clean
        item.pinned = true
        delete item.expires_at
      } else delete item.name
      return { ...item }
    })
    ctx.handle('clipboard_set_favorites_collapsed', () => undefined)
    ctx.handle('clipboard_delete', ({ id, force }) => {
      this.fail('clipboard_delete')
      const item = this.find(id)
      if (item.pinned && !force) throw mockError('conflict', 'pinned item requires force')
      this.items = this.items.filter((x) => x.id !== id)
      ctx.emit('clipboard:changed', { change: 'removed', id })
    })
    ctx.handle('clipboard_clear', ({ keep_pinned }) => {
      const before = this.items.length
      this.items = keep_pinned ? this.items.filter((x) => x.pinned) : []
      ctx.emit('clipboard:changed', { change: 'cleared', keep_pinned })
      return before - this.items.length
    })
    ctx.handle('clipboard_set_paused', ({ paused }) => {
      this.paused = paused
      ctx.emit('clipboard:changed', { change: 'paused', paused })
    })
    ctx.handle('clipboard_save_image', ({ id }) => {
      this.find(id)
      return ctx.fixtures.os === 'windows'
        ? 'C:\\Users\\me\\Pictures\\wisp.png'
        : '~/Pictures/wisp.png'
    })
    ctx.handle('clipboard_reveal', ({ id }) => void this.find(id))
    ctx.handle('clipboard_open_link', ({ id }) => void this.find(id))
    ctx.handle('clipboard_send_to', ({ id }) => void this.find(id))
  }

  payload(extra: Partial<ClipboardPayload> = {}): PanelPayload {
    return {
      kind: 'clipboard',
      page: this.page(0, 200, 'all'),
      paused: this.paused,
      hint_bar: true,
      paste_on_double_click: false,
      ...extra,
    }
  }
}

// ---------------------------------------------------------------------------
// 交互辅助：真实的 DOM 事件

const $ = <T extends Element = HTMLElement>(sel: string) => document.querySelector<T>(sel)

function isMac(): boolean {
  return document.documentElement.dataset.os === 'macos'
}

/** 发一个按键（keydown + keyup），走统一的键盘路由（window 捕获阶段） */
function press(
  key: string,
  code: string,
  mods: { mod?: boolean; shift?: boolean; alt?: boolean; repeat?: boolean } = {},
): void {
  const mac = isMac()
  const init: KeyboardEventInit = {
    key,
    code,
    bubbles: true,
    cancelable: true,
    shiftKey: !!mods.shift,
    altKey: !!mods.alt,
    metaKey: !!mods.mod && mac,
    ctrlKey: !!mods.mod && !mac,
    repeat: !!mods.repeat,
  }
  const target = document.activeElement ?? document.body
  target.dispatchEvent(new KeyboardEvent('keydown', init))
  if (mods.repeat) return
  target.dispatchEvent(new KeyboardEvent('keyup', init))
  // 松开 ⌘ / Ctrl（否则视图以为修饰键还按着，行尾一直显示 ⌘n 角标）
  if (mods.mod) {
    target.dispatchEvent(
      new KeyboardEvent('keyup', {
        key: mac ? 'Meta' : 'Control',
        code: mac ? 'MetaLeft' : 'ControlLeft',
        bubbles: true,
        shiftKey: !!mods.shift,
      }),
    )
  }
}

/** 按住 ⌘（Windows / Linux 为 Ctrl） */
function holdMod(): void {
  const mac = isMac()
  window.dispatchEvent(
    new KeyboardEvent('keydown', {
      key: mac ? 'Meta' : 'Control',
      code: mac ? 'MetaLeft' : 'ControlLeft',
      bubbles: true,
      metaKey: mac,
      ctrlKey: !mac,
    }),
  )
}

/** 往搜索框里输入（React 受控输入：用原生 setter 再派发 input 事件） */
function typeQuery(text: string): void {
  const input = $<HTMLInputElement>('[data-testid="clip-search"]')
  if (!input) return
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')?.set
  setter?.call(input, text)
  input.dispatchEvent(new Event('input', { bubbles: true }))
}

function rowEl(n: number): HTMLElement | null {
  return document.querySelectorAll<HTMLElement>('[data-testid="clip-row"]')[n - 1] ?? null
}

/** 指针移到第一条某类型的行（悬停选中 + 300 ms 后的预览卡片；按 data-kind 定位，不依赖排序后的序号） */
function hoverKind(kind: ClipKind): void {
  hoverEl($(`[data-testid="clip-row"][data-kind="${kind}"]`))
}

function hoverEl(row: HTMLElement | null): void {
  if (!row) return
  const r = row.getBoundingClientRect()
  const init: PointerEventInit = {
    bubbles: true,
    clientX: r.left + r.width * 0.4,
    clientY: r.top + r.height / 2,
    pointerType: 'mouse',
  }
  row.dispatchEvent(new PointerEvent('pointerover', init))
  // 列表把指针第一次出现的位置只记作基线（面板显示时 WebKit 的合成 pointermove 不选中，ClipList.tsx）：先报一个
  // 偏 1 px 的位置，再报真正的悬停位置
  const clientX = init.clientX ?? 0
  const clientY = init.clientY ?? 0
  row.dispatchEvent(
    new PointerEvent('pointermove', { ...init, clientX: clientX - 1, clientY: clientY - 1 }),
  )
  row.dispatchEvent(new PointerEvent('pointermove', init))
}

/** 单击第 n 行（可带修饰键：⌘ / Ctrl 切换标记、⇧ 扩展选择） */
function clickRow(n: number, mods: { mod?: boolean; shift?: boolean } = {}): void {
  const row = rowEl(n)
  if (!row) return
  const r = row.getBoundingClientRect()
  const mac = isMac()
  row.dispatchEvent(
    new MouseEvent('click', {
      bubbles: true,
      cancelable: true,
      button: 0,
      clientX: r.left + r.width * 0.5,
      clientY: r.top + r.height / 2,
      metaKey: !!mods.mod && mac,
      ctrlKey: !!mods.mod && !mac,
      shiftKey: !!mods.shift,
    }),
  )
}

function contextMenuOn(n: number): void {
  const row = rowEl(n)
  if (!row) return
  const r = row.getBoundingClientRect()
  row.dispatchEvent(
    new MouseEvent('contextmenu', {
      bubbles: true,
      cancelable: true,
      button: 2,
      clientX: r.left + r.width * 0.55,
      clientY: r.top + r.height / 2,
    }),
  )
}

/** 面板进入动画结束后执行（卡片 data-phase = shown） */
function whenShown(ctx: MockContext, fn: () => void, waited = 0): void {
  ctx.after(50, () => {
    const shown = $('[data-testid="panel-card"]')?.dataset.phase === 'shown'
    if (shown || waited > 3000) fn()
    else whenShown(ctx, fn, waited + 50)
  })
}

/** 依次执行（每项的数字是距上一步的间隔，ms） */
function steps(ctx: MockContext, list: readonly (readonly [number, () => void])[]): void {
  let at = 0
  for (const [delay, fn] of list) {
    at += delay
    ctx.after(at, fn)
  }
}

// ---------------------------------------------------------------------------
// 收藏、验证码、「粘贴为…」用到的额外条目

/** 7 条带名称的收藏（超过 5 条：「全部」里只列前 5 条，末尾是「查看全部 7 个收藏」） */
function favoriteSeeds(os: string, now: number): Seed[] {
  const a = apps(os)
  const fav = (name: string, preview: string, days: number, app = a.notes): Seed => ({
    kind: 'text',
    preview,
    at: now - days * DAY,
    pinned: true,
    name,
    app,
  })
  return [
    fav('家里地址', '上海市徐汇区漕溪北路 88 号 12 层 1203 室', 1, a.wechat),
    fav('发票抬头', '上海某某信息科技有限公司 · 税号 91310104MA1FR0XX0Q', 2),
    fav('Wi-Fi', 'Wisp-Guest / 密码 correct-horse-battery', 3),
    fav('会议链接', 'https://meet.example.com/wisp-weekly', 4, a.safari),
    fav('签名', 'Best regards, Wisp Team', 5, a.mail),
    fav('常用回复', '收到，我看一下，稍后回复你。', 6, a.slack),
    fav('快递地址', '北京市海淀区中关村大街 1 号', 8, a.wechat),
  ]
}

/**
 * 最近的两条：一条短信验证码（10 分钟后删除，刚复制）与一条 JSON（「粘贴为…」可以格式化），排在其他样例前面；不带
 * 收藏（光标打开时就在验证码上）
 */
function smartSeeds(os: string, now: number): Seed[] {
  const a = apps(os)
  return [
    {
      kind: 'text',
      preview: '【Wisp】您的验证码 482913，10 分钟内有效。客服电话 138 0013 8000',
      at: now - 40_000,
      expiresIn: 10,
      app: a.wechat,
    },
    {
      kind: 'code',
      preview: '{"name":"wisp","version":"0.2.0","features":["sync","chips"]}',
      at: now - 90_000,
      app: a.code,
    },
    ...seeds(os, now).filter((s) => !s.pinned),
  ]
}

// ---------------------------------------------------------------------------
// 场景构造

/** 操作之后至少再等这么久才算稳定（悬停 300 ms + popover / previewIn 动效 + 详情请求） */
const SETTLE_HOLD_MS = 700

interface ClipSetup {
  /** 操作之后再等多久算稳定（轻提示 1.5 s 后就消失，这类场景要短一些） */
  hold?: number
  seeds?: (os: string, now: number) => Seed[]
  payload?: Partial<ClipboardPayload>
  options?: PrepareOptions
  before?: (db: ClipDb) => void
  /** 面板显示之后的操作 */
  act?: (ctx: MockContext, db: ClipDb) => void
}

function openClipboard(ctx: MockContext, setup: ClipSetup = {}): ClipDb {
  const db = new ClipDb(ctx)
  for (const seed of (setup.seeds ?? seeds)(ctx.fixtures.os, Date.now())) db.add(seed)
  setup.before?.(db)
  db.install()
  ctx.preparePanel(db.payload(setup.payload), setup.options ?? {})
  const act = setup.act
  if (act) {
    whenShown(ctx, () => {
      act(ctx, db)
      // 视图里由操作引出的计时器（悬停 300 ms、检索去抖、动效）不计入 mock 的「未稳定」：再多等一会儿才截图
      ctx.after(setup.hold ?? SETTLE_HOLD_MS, () => undefined)
    })
  }
  return db
}

/**
 * 悬浮剪贴板（clipfloat 入口）：同一份样例数据经 `clipboard_float_ready` 交给窗口，光标接着面板的第三条；`act` 在
 * 视图挂载之后执行
 */
const float = (id: string, label: string, act?: (ctx: MockContext) => void): Scenario => ({
  id: `clipboard.${id}`,
  label,
  view: 'clipboard',
  entry: 'clipfloat',
  setup(ctx) {
    const db = new ClipDb(ctx)
    for (const seed of seeds(ctx.fixtures.os, Date.now())) db.add(seed)
    db.install()
    const { kind: _kind, ...payload } = db.payload() as Extract<PanelPayload, { kind: 'clipboard' }>
    ctx.handle('clipboard_float_ready', () => ({
      query: '',
      filter: 'all',
      selected_id: db.items[2]?.id ?? null,
      on_top: true,
      payload,
    }))
    act?.(ctx)
  },
})

const clip = (
  id: string,
  label: string,
  setup: ClipSetup = {},
  extra: Partial<Scenario> = {},
): Scenario => ({
  id: `clipboard.${id}`,
  label,
  view: 'clipboard',
  setup: (ctx) => void openClipboard(ctx, setup),
  ...extra,
})

/**
 * toast 视图：Rust 按粘贴键估算宽度（与 placement::toast_width 同一公式）后不激活地显示；`focused` 模拟 KDE Wayland
 * 下剪贴板视图在原位切过来的前端退路（面板持有焦点）
 */
function showToast(ctx: MockContext, reason: CopiedReason, focused = false): void {
  const pasteKey = ctx.fixtures.os === 'macos' ? '⌘V' : 'Ctrl+V'
  let units = 26
  for (const ch of pasteKey) units += /[\u2e80-\uffff]/.test(ch) ? 2 : 1
  const width = Math.min(360, Math.max(220, 64 + units * 7))
  ctx.preparePanel(
    { kind: 'toast', reason, paste_key: pasteKey },
    { width, placement: 'below', focused },
  )
}

const REASONS: readonly CopiedReason[] = [
  'copy_only',
  'wayland',
  'elevated',
  'target_gone',
  'focus_timeout',
  'no_permission',
]

const toast = (reason: CopiedReason): Scenario => ({
  id: `clipboard.paste-toast-${reason}`,
  label: `toast · ${reason}`,
  view: 'toast',
  // 截图在停留期间（1.6 s 后会自动淡出）
  manualSettle: true,
  setup(ctx) {
    showToast(ctx, reason)
    ctx.after(700, () => ctx.settled())
  },
})

// ---------------------------------------------------------------------------
// 1 万条（流畅度场景）

/** 预先栅格化的缩略图 / 应用图标（真实应用里是 Rust 送来的 JPEG / PNG，在主线程之外解码） */
async function rasterImages(ctx: MockContext): Promise<{ thumbs: string[]; icons: string[] }> {
  const raster = async (
    path: string,
    w: number,
    h: number,
    draw: (g: CanvasRenderingContext2D) => void,
  ) => {
    const canvas = document.createElement('canvas')
    canvas.width = w
    canvas.height = h
    const g = canvas.getContext('2d')
    if (!g) return ctx.asset(path, '')
    draw(g)
    const blob = await new Promise<Blob | null>((r) => canvas.toBlob(r, 'image/png'))
    return ctx.asset(path, blob ?? '')
  }
  const thumbs = await Promise.all(
    Array.from({ length: 8 }, (_, i) =>
      raster(`thumb/perf-${i}.png`, 112, 64 + (i % 3) * 16, (g) => {
        const grad = g.createLinearGradient(0, 0, 112, 96)
        grad.addColorStop(0, `hsl(${i * 45} 70% 86%)`)
        grad.addColorStop(1, `hsl(${(i * 45 + 50) % 360} 65% 66%)`)
        g.fillStyle = grad
        g.fillRect(0, 0, 112, 112)
        g.fillStyle = 'rgba(255,255,255,0.8)'
        g.fillRect(10, 12, 56, 8)
      }),
    ),
  )
  const icons = await Promise.all(
    Array.from({ length: 6 }, (_, i) =>
      raster(`icon/perf-${i}.png`, 64, 64, (g) => {
        g.fillStyle = `hsl(${i * 60} 72% 52%)`
        g.beginPath()
        g.roundRect(4, 4, 56, 56, 14)
        g.fill()
        g.fillStyle = '#fff'
        g.beginPath()
        g.arc(32, 32, 12, 0, Math.PI * 2)
        g.fill()
      }),
    ),
  )
  await Promise.all(
    [...thumbs, ...icons].map((src) => {
      const img = new Image()
      img.src = src
      return img.decode().catch(() => {})
    }),
  )
  return { thumbs, icons }
}

const PERF_WORDS = [
  'Invoice INV-2026',
  '会议纪要',
  'https://example.com/docs/',
  'const value = await fetch(url)',
  'Latency budget',
  '收货地址：漕溪北路',
  'SELECT * FROM clips',
  'Thanks for the update',
]

/** 1 万条：每 7 条一张图（64 行高），其余文本 / 链接 / 代码；前 3 条置顶（「置顶 / 最近」两组），时间从今天往前铺 60 天 */
function perfItems(thumbs: string[], icons: string[], now: number): ClipItemMeta[] {
  const today = startOfDay(now)
  const out: ClipItemMeta[] = []
  for (let i = 0; i < 10_000; i++) {
    const id = i + 1
    const at =
      i < 60
        ? Math.max(today + 1000, now - i * 3 * MINUTE)
        : today - Math.floor((i - 60) / 165) * DAY - (i % 165) * 5 * MINUTE
    const icon = icons[i % icons.length] ?? null
    const source: SourceApp = { id: `app${i % 6}`, name: `App ${i % 6}`, icon_url: icon }
    const base = {
      id,
      created_at: at,
      last_used_at: at,
      use_count: 1,
      pinned: i < 3,
      source,
      truncated: false,
      files: null,
      link: null,
      color: null,
    }
    if (i % 7 === 3) {
      out.push({
        ...base,
        kind: 'image',
        preview: '',
        char_count: null,
        line_count: null,
        image: {
          width: 1600 + (i % 5) * 200,
          height: 900 + (i % 3) * 120,
          bytes: 1_200_000 + i,
          thumb_url: thumbs[i % thumbs.length] ?? null,
        },
      })
      continue
    }
    const kind: ClipKind = i % 11 === 5 ? 'link' : i % 13 === 7 ? 'code' : 'text'
    const preview = `${PERF_WORDS[i % PERF_WORDS.length] ?? ''} #${i} — lorem ipsum dolor sit amet, consectetur adipiscing elit`
    out.push({ ...base, kind, preview, char_count: preview.length, line_count: 1, image: null })
  }
  return out.sort(
    (x, y) => Number(y.pinned) - Number(x.pinned) || y.last_used_at - x.last_used_at || y.id - x.id,
  )
}

const scroll10k: Scenario = {
  id: 'clipboard.scroll-10k',
  label: '剪贴板 · 1 万条连续滚动 + 按住 ↓',
  view: 'clipboard',
  manualSettle: true,
  async setup(ctx) {
    const [{ thumbs, icons }, { warmUp }] = await Promise.all([
      rasterImages(ctx),
      import('../../../perf/warmup'),
    ])
    // 常驻面板的稳态：字体与 Intl 的一次性开销在启动后的空闲时间里已经付掉（与 shell.view-switch 相同）
    warmUp()
    const sorted = perfItems(thumbs, icons, Date.now())
    let details = 0
    // 预先排好序：分页直接切片（mock 的处理函数不把排序开销算进被测页面的主线程）
    ctx.handle('clipboard_recent', ({ offset, limit, filter }) => {
      const all = filter === 'all' ? sorted : sorted.filter((it) => matchesFilter(it, filter))
      return {
        items: all.slice(offset, offset + limit).map((item) => ({ item, highlights: [] })),
        pinned_count: 3,
        next_offset: offset + limit < all.length ? offset + limit : null,
        query: null,
        filter,
      }
    })
    ctx.handle('clipboard_get', () => {
      details++
      return null
    })
    ctx.preparePanel({
      kind: 'clipboard',
      page: {
        items: sorted.slice(0, 200).map((item) => ({ item, highlights: [] })),
        pinned_count: 3,
        next_offset: 200,
        query: null,
        filter: 'all',
      },
      paused: false,
      hint_bar: true,
      paste_on_double_click: false,
    })
    const html = document.documentElement
    let maxRows = 0
    let visible = 0
    const sample = () => {
      const list = $('[data-testid="clip-list"]')
      if (!list) return
      visible ||= Math.ceil(list.clientHeight / 44)
      const rows = list.querySelectorAll('.ui-vlist-row').length
      const groups = list.querySelectorAll('.clip-group').length
      maxRows = Math.max(maxRows, rows)
      if (rows > visible * 3 + groups) {
        html.dataset.qaRowsViolation = `${rows} > ${visible} × 3 + ${groups}`
      }
    }
    whenShown(ctx, () => {
      const list = $('[data-testid="clip-list"]')
      if (!list) return
      sample()
      const finish = () => {
        sample()
        // 原图（img/）只在悬停大图与 Space 预览时请求；快速滚动全程应为 0
        const originals = performance
          .getEntriesByType('resource')
          .filter((e) => e.name.includes('/img/')).length
        html.dataset.qaRowsMax = String(maxRows)
        html.dataset.qaRowsLimit = String(visible * 3)
        html.dataset.qaOriginals = String(originals + details)
        const ok = !html.dataset.qaRowsViolation && originals + details === 0
        html.dataset.qaAssert = ok ? 'ok' : 'fail'
        if (!ok) {
          // 断言失败时不到达 data-settled：qa:shots 等待超时、记为失败并以非零码退出（虚拟列表的行数上限是门禁，不只是记录）
          console.error('[clipboard.scroll-10k] 虚拟列表断言失败', {
            rows: html.dataset.qaRowsViolation,
            originals: originals + details,
          })
          return
        }
        setTimeout(() => ctx.settled(), 200)
      }
      // 2) 回到顶部，按住 ↓ 1.5 s（约 30 次 / s 的按键重复）
      const holdDown = () => {
        press('ArrowUp', 'ArrowUp', { mod: true })
        let n = 0
        const repeat = () => {
          press('ArrowDown', 'ArrowDown', { repeat: n > 0 })
          sample()
          if (++n < 45) setTimeout(repeat, 33)
          else finish()
        }
        setTimeout(repeat, 120)
      }
      // 1) 连续滚动 2.4 s（约 3000 px/s，越过几次分页）
      const started = performance.now()
      const tick = (t: number) => {
        list.scrollTop += 52
        sample()
        if (t - started < 2400) requestAnimationFrame(tick)
        else holdDown()
      }
      requestAnimationFrame(tick)
    })
  },
}

// ---------------------------------------------------------------------------
// 焦点与菜单的键盘回归（真实浏览器里跑：jsdom 不检查可见性，测不出「菜单第一次打开时焦点没进去」）

/**
 * 唤出后焦点在搜索框里，每个菜单（⌘F 过滤、⌘K 操作、⋯、右键）在会话里**第一次**打开时焦点就在菜单里、
 * ↓ 立即移动高亮、Esc 关闭后焦点回到搜索框。任何一步不满足都不到达 data-settled：qa:shots 等待超时、记为失败
 */
const focusKeyboard: Scenario = {
  id: 'clipboard.focus-keyboard',
  label: '剪贴板 · 唤出即可打字 + 每个菜单第一次打开就能用键盘（回归）',
  view: 'clipboard',
  manualSettle: true,
  setup(ctx) {
    const html = document.documentElement
    const failures: string[] = []
    const focusIn = (testId: string) =>
      document.activeElement?.closest(`[data-testid="${testId}"]`) !== null &&
      document.activeElement !== null
    const expect = (ok: boolean, what: string) => {
      if (!ok) failures.push(what)
    }
    const activeItem = (menu: string) =>
      $(`[data-testid="${menu}"] [data-active]`)?.getAttribute('data-testid') ?? null
    /**
     * 每 20 ms 查一次 `cond`，成立或到 `limit` ms 时执行 `then`（由它断言）。不按固定延时断言：CI 的 macOS
     * 虚拟机上 4 个页面并发、没有 GPU，菜单的退出动画靠 rAF 推进而帧被饿住，Esc 之后 220 ms 菜单可能还在 DOM 里。
     * 上限远大于正常耗时，「焦点没进菜单 / ↓ 不动 / Esc 关不掉」这类回归照样拦得住
     */
    const until = (cond: () => boolean, limit: number, then: () => void) => {
      const deadline = performance.now() + limit
      const check = () => {
        if (cond() || performance.now() >= deadline) then()
        else ctx.after(20, check)
      }
      ctx.after(20, check)
    }
    const menuOpen = (menu: string) => $(`[data-testid="${menu}"][role="menu"]`) !== null
    /** 打开菜单 → 焦点在菜单里 → ↓ 移动高亮 → Esc 关闭、焦点回到搜索框 */
    const menuCase = (menu: string, open: () => void, next: () => void) => {
      open()
      until(
        () => focusIn(menu),
        2000,
        () => {
          expect(focusIn(menu), `${menu}：第一次打开时焦点不在菜单里`)
          const before = activeItem(menu)
          press('ArrowDown', 'ArrowDown')
          until(
            () => activeItem(menu) !== before,
            1000,
            () => {
              expect(activeItem(menu) !== before, `${menu}：↓ 没有移动高亮`)
              press('Escape', 'Escape')
              until(
                () => !menuOpen(menu) && focusIn('clip-search'),
                2000,
                () => {
                  expect(!menuOpen(menu), `${menu}：Esc 没有关闭菜单`)
                  expect(focusIn('clip-search'), `${menu}：关闭后焦点没有回到搜索框`)
                  next()
                },
              )
            },
          )
        },
      )
    }
    const finish = () => {
      html.dataset.qaAssert = failures.length === 0 ? 'ok' : 'fail'
      if (failures.length > 0) {
        console.error('[clipboard.focus-keyboard] 断言失败', failures)
        return
      }
      ctx.after(200, () => ctx.settled())
    }
    openClipboard(ctx, {
      act: () => {
        expect(focusIn('clip-search'), '唤出后焦点不在搜索框里')
        menuCase(
          'clip-filter-menu',
          () => press('f', 'KeyF', { mod: true }),
          () =>
            menuCase(
              'clip-context-menu',
              () => press('k', 'KeyK', { mod: true }),
              () =>
                menuCase(
                  'clip-more-menu',
                  () => $<HTMLButtonElement>('[data-testid="clip-more"]')?.click(),
                  finish,
                ),
            ),
        )
      },
    })
  },
}

// ---------------------------------------------------------------------------

const mac = () => isMac()
const deleteKey = (): [string, string] =>
  mac() ? ['Backspace', 'Backspace'] : ['Delete', 'Delete']

export const scenarios: readonly Scenario[] = [
  clip('list', '剪贴板 · 混合类型（收藏 / 最近）'),
  clip('search', '剪贴板 · 搜索高亮', { act: () => typeQuery('wisp') }),
  clip('search-snippet', '剪贴板 · 命中在预览之外（上下文片段）', {
    act: () => typeQuery('round 37'),
  }),
  clip('search-cjk', '剪贴板 · 中文两个字的搜索', { act: () => typeQuery('地址') }),
  clip('search-loading', '剪贴板 · 检索进行中（转圈）', {
    before: (db) => {
      db.searchDelay = 60_000
    },
    act: (ctx) => {
      typeQuery('budget')
      ctx.after(900, () => undefined)
    },
  }),
  clip('no-result', '剪贴板 · 无结果', { act: () => typeQuery('不存在的内容 xyz') }),
  clip('no-result-filtered', '剪贴板 · 过滤后无结果', {
    // 全部 → 收藏 → 文本 → 图片：在图片里搜文字
    act: (ctx) =>
      steps(ctx, [
        [0, () => press('Tab', 'Tab')],
        [150, () => press('Tab', 'Tab')],
        [150, () => press('Tab', 'Tab')],
        [150, () => typeQuery('budget')],
      ]),
  }),
  clip('empty', '剪贴板 · 空状态', { seeds: () => [] }),
  clip('empty-filtered', '剪贴板 · 某类型为空', {
    seeds: (os, now) => seeds(os, now).filter((s) => s.kind !== 'color'),
    act: () => press('Tab', 'Tab', { shift: true }),
  }),
  clip('filter-images', '剪贴板 · 只看图片', {
    act: (ctx) =>
      steps(ctx, [
        [0, () => press('Tab', 'Tab')],
        [120, () => press('Tab', 'Tab')],
        [120, () => press('Tab', 'Tab')],
      ]),
  }),
  clip('favorites-collapsed', '剪贴板 · 收藏分组折叠（标题后是条数）', {
    seeds: (os, now) => [...favoriteSeeds(os, now), ...seeds(os, now)],
    payload: { favorites_collapsed: true },
  }),
  clip('favorites-more', '剪贴板 · 收藏超过 5 条：只列前 5 条与「查看全部 7 个收藏」', {
    seeds: (os, now) => [...favoriteSeeds(os, now), ...seeds(os, now)],
  }),
  clip('favorites-view', '剪贴板 · 收藏视图（点「查看全部」进入，名称加粗）', {
    seeds: (os, now) => [...favoriteSeeds(os, now), ...seeds(os, now)],
    act: () => $<HTMLButtonElement>('[data-testid="clip-show-favorites"]')?.click(),
  }),
  clip('chips-otp', '剪贴板 · 验证码条目的芯片条与「N 分钟后删除」（⌘E 进入芯片条）', {
    seeds: smartSeeds,
    // 光标打开时就在验证码上：停下来之后检视，芯片条出现；⌘E 把键盘焦点交给芯片条
    act: (ctx) => steps(ctx, [[400, () => press('e', 'KeyE', { mod: true })]]),
  }),
  clip('multi-select', '剪贴板 · 多选（⌘ 点击按顺序编号，提示条换成合并粘贴）', {
    act: (ctx) =>
      steps(ctx, [
        [0, () => clickRow(4, { mod: true })],
        [80, () => clickRow(3, { mod: true })],
        [80, () => clickRow(6, { mod: true })],
      ]),
  }),
  clip('paste-as', '剪贴板 · 「粘贴为…」子页（⌥⇧↩）', {
    seeds: smartSeeds,
    act: (ctx) =>
      steps(ctx, [
        [0, () => press('ArrowDown', 'ArrowDown')],
        [400, () => press('Enter', 'Enter', { alt: true, shift: true })],
      ]),
  }),
  clip('rename', '剪贴板 · 收藏的命名条', {
    seeds: (os, now) => [...favoriteSeeds(os, now).slice(0, 2), ...seeds(os, now)],
    act: (ctx) =>
      steps(ctx, [
        [0, () => contextMenuOn(1)],
        [250, () => $<HTMLElement>('[data-testid="clip-context-menu-item-rename"]')?.click()],
      ]),
  }),
  float('float', '悬浮剪贴板（接着面板的光标，置顶；头部是置顶开关与关闭）'),
  float('float-queue', '悬浮剪贴板 · 连续粘贴进行中（头部的「结束」与进度提示）', (ctx) =>
    ctx.after(300, () => ctx.emit('clipboard:queue', { active: true, index: 2, total: 4 })),
  ),
  clip('queue-start', '剪贴板 · 选了三条，提示条里的 ⌘↩ 是连续粘贴', {
    act: (ctx) =>
      steps(ctx, [
        [0, () => clickRow(3, { mod: true })],
        [80, () => clickRow(4, { mod: true })],
        [80, () => clickRow(6, { mod: true })],
      ]),
  }),
  {
    id: 'clipboard.queue-toast',
    label: 'toast · 连续粘贴进度（2 / 5）',
    view: 'toast',
    manualSettle: true,
    setup(ctx) {
      const pasteKey = ctx.fixtures.os === 'macos' ? '⌘V' : 'Ctrl+V'
      ctx.preparePanel(
        {
          kind: 'toast',
          reason: 'copy_only',
          paste_key: pasteKey,
          queue: { active: true, index: 2, total: 5 },
        },
        { width: 300, placement: 'below', focused: false },
      )
      ctx.after(700, () => ctx.settled())
    },
  },
  clip('filter-menu', '剪贴板 · 类型过滤菜单（⌘F）', {
    act: () => press('f', 'KeyF', { mod: true }),
  }),
  clip('more-menu', '剪贴板 · ⋯ 菜单', {
    act: () => $<HTMLButtonElement>('[data-testid="clip-more"]')?.click(),
  }),
  clip('context-menu', '剪贴板 · 右键菜单', { act: () => contextMenuOn(3) }),
  clip('context-menu-image', '剪贴板 · 图片条目的右键菜单', { act: () => contextMenuOn(6) }),
  clip('paused', '剪贴板 · 监听已暂停', {
    before: (db) => {
      db.paused = true
    },
  }),
  // 历史库来自更新版本（打不开），列表为空——列表区说明原因，而不是「复制点什么」
  clip('unavailable', '剪贴板 · 历史库不可用（来自更新版本）', {
    seeds: () => [],
    payload: { unavailable: 'clipboard.history_too_new' },
  }),
  clip('disabled', '剪贴板 · 设置里关闭了监听', {
    payload: { recording: { state: 'disabled' } },
  }),
  clip('hover-text', '剪贴板 · 悬停预览（多行代码）', { act: () => hoverKind('code') }),
  clip('hover-image', '剪贴板 · 悬停预览（图片大图）', { act: () => hoverKind('image') }),
  clip('hover-files', '剪贴板 · 悬停预览（文件）', { act: () => hoverKind('files') }),
  clip('hover-color', '剪贴板 · 悬停预览（颜色）', { act: () => hoverKind('color') }),
  clip('preview-text', '剪贴板 · 全文预览（长文本 + 搜索高亮）', {
    act: (ctx) =>
      steps(ctx, [
        [0, () => typeQuery('budget')],
        [250, () => press('ArrowDown', 'ArrowDown')],
        [60, () => press(' ', 'Space', { shift: true })],
      ]),
  }),
  clip('preview-code', '剪贴板 · 全文预览（代码，等宽）', {
    act: (ctx) =>
      steps(ctx, [
        [0, () => press('ArrowDown', 'ArrowDown')],
        [0, () => press('ArrowDown', 'ArrowDown')],
        [120, () => press(' ', 'Space')],
      ]),
  }),
  clip('preview-image', '剪贴板 · 全文预览（图片）', {
    act: (ctx) =>
      steps(ctx, [
        [0, () => hoverKind('image')],
        [40, () => press(' ', 'Space')],
      ]),
  }),
  clip('preview-files', '剪贴板 · 全文预览（文件）', {
    act: (ctx) =>
      steps(ctx, [
        [0, () => hoverKind('files')],
        [40, () => press(' ', 'Space')],
      ]),
  }),
  clip('preview-color', '剪贴板 · 全文预览（颜色）', {
    act: (ctx) =>
      steps(ctx, [
        [0, () => hoverKind('color')],
        [40, () => press(' ', 'Space')],
      ]),
  }),
  clip('preview-link', '剪贴板 · 全文预览（链接）', {
    act: (ctx) =>
      steps(ctx, [
        [0, () => press('ArrowDown', 'ArrowDown')],
        [40, () => press(' ', 'Space')],
      ]),
  }),
  clip('confirm-delete', '剪贴板 · 删除置顶项的行内确认', {
    act: () => press(...deleteKey()),
  }),
  clip('confirm-clear', '剪贴板 · 清空确认条', {
    act: (ctx) =>
      steps(ctx, [
        [0, () => $<HTMLButtonElement>('[data-testid="clip-more"]')?.click()],
        [200, () => $<HTMLElement>('[data-testid="clip-more-menu-item-clear"]')?.click()],
      ]),
  }),
  clip('action-menu', '剪贴板 · ⌘K 操作菜单（纯键盘到达全部操作）', {
    act: (ctx) =>
      steps(ctx, [
        [0, () => press('ArrowDown', 'ArrowDown')],
        [0, () => press('ArrowDown', 'ArrowDown')],
        [120, () => press('k', 'KeyK', { mod: true })],
      ]),
  }),
  clip('preview-tab-focus', '剪贴板 · 预览里 Tab 移到按钮（焦点环），不切换过滤', {
    act: (ctx) =>
      steps(ctx, [
        [0, () => hoverKind('color')],
        [40, () => press(' ', 'Space')],
        [300, () => press('Tab', 'Tab')],
      ]),
  }),
  clip('hover-many-files', '剪贴板 · 悬停 12 个文件的条目（元数据只带前 5 个：「5+ 个文件」）', {
    seeds: (os, now) => [
      ...seeds(os, now).slice(0, 2),
      {
        kind: 'files',
        preview: Array.from({ length: 12 }, (_, i) => `scan-${i + 1}.png`).join(', '),
        files: Array.from({ length: 12 }, (_, i) => ({
          path: `/Users/me/Desktop/scan-${i + 1}.png`,
          name: `scan-${i + 1}.png`,
          exists: true,
          size: 120_000 + i,
        })),
        at: now - 8 * MINUTE,
      },
      ...seeds(os, now).slice(2, 6),
    ],
    act: (ctx) => steps(ctx, [[200, () => hoverKind('files')]]),
  }),
  clip('edge-rows', '剪贴板 · 只有空白的条目、文件都已不存在、5 个以上的文件', {
    seeds: (os, now) => [
      ...seeds(os, now).slice(0, 3),
      { kind: 'text', preview: '', text: '   ', at: now - 5 * MINUTE },
      { kind: 'text', preview: '', text: '\n\n', at: now - 6 * MINUTE },
      {
        kind: 'files',
        preview: Array.from({ length: 12 }, (_, i) => `scan-${i + 1}.png`).join(', '),
        files: Array.from({ length: 12 }, (_, i) => ({
          path: `/Users/me/Desktop/scan-${i + 1}.png`,
          name: `scan-${i + 1}.png`,
          exists: true,
          size: 120_000 + i,
        })),
        at: now - 8 * MINUTE,
      },
      ...seeds(os, now).slice(13, 14),
    ],
  }),
  clip('mod-badges', '剪贴板 · 按住 ⌘ 显示 ⌘1–9 角标', {
    act: (ctx) => {
      holdMod()
      ctx.after(700, () => undefined)
    },
  }),
  clip('error', '剪贴板 · 操作失败的提示', {
    hold: 200,
    before: (db) => {
      db.failNext = 'clipboard_pin'
    },
    act: (ctx) =>
      steps(ctx, [
        [0, () => press('ArrowDown', 'ArrowDown')],
        [0, () => press('ArrowDown', 'ArrowDown')],
        [80, () => press('p', 'KeyP', { mod: true })],
      ]),
  }),
  clip('secret-skipped', '剪贴板 · 跳过疑似密钥', {
    hold: 200,
    act: (ctx) => ctx.emit('clipboard:changed', { change: 'secret_skipped' }),
  }),
  clip(
    'live-insert',
    '剪贴板 · 实时插入（高亮不跳）',
    {
      act: (ctx, db) =>
        steps(ctx, [
          [0, () => press('ArrowDown', 'ArrowDown')],
          [0, () => press('ArrowDown', 'ArrowDown')],
          [
            300,
            () =>
              db.insert({
                kind: 'text',
                preview: '新复制的一段文字：剪贴板会实时插入新条目，选中项不跳',
                at: Date.now(),
                app: apps(ctx.fixtures.os).notes,
              }),
          ],
          [60, () => ctx.motionMid()],
          [
            700,
            () =>
              db.insert({
                kind: 'link',
                preview: 'https://tauri.app/develop/',
                link: 'https://tauri.app/develop/',
                at: Date.now(),
                app: apps(ctx.fixtures.os).safari,
              }),
          ],
        ]),
    },
    { motion: true },
  ),
  clip(
    'pin-move',
    '剪贴板 · 置顶后移到置顶区（位移动效）',
    {
      act: (ctx) =>
        steps(ctx, [
          [0, () => press('ArrowDown', 'ArrowDown')],
          [0, () => press('ArrowDown', 'ArrowDown')],
          [0, () => press('ArrowDown', 'ArrowDown')],
          [200, () => press('p', 'KeyP', { mod: true })],
          [60, () => ctx.motionMid()],
        ]),
    },
    { motion: true },
  ),
  clip('thumb-pending', '剪贴板 · 缩略图稍后生成（updated 原地替换）', {
    seeds: (os, now) =>
      seeds(os, now).map((s) =>
        s.image ? { ...s, image: { ...s.image, thumb: s.image.width !== 2560 } } : s,
      ),
    act: (ctx, db) =>
      ctx.after(400, () => {
        const img = db.items.find((it) => it.image?.thumb_url === null)
        if (!img?.image) return
        img.image = { ...img.image, thumb_url: thumbnail(img.image.width, img.image.height, 220) }
        ctx.emit('clipboard:changed', { change: 'updated', item: { ...img } })
      }),
  }),
  clip('compact', '剪贴板 · 窄而矮（360 × 上限 300）', {
    options: { width: 360, maxHeight: 300 },
  }),
  clip('wide', '剪贴板 · 宽（560）', { options: { width: 560 } }),
  clip('few', '剪贴板 · 只有两条（高度下限）', {
    seeds: (os, now) => seeds(os, now).slice(2, 4),
  }),
  clip('no-hint-bar', '剪贴板 · 隐藏提示条、双击粘贴', {
    payload: { hint_bar: false, paste_on_double_click: true },
  }),
  clip(
    'paste-flow',
    '剪贴板 · ⌘↩ 仅复制 → toast',
    {
      act: (ctx) =>
        steps(ctx, [
          [0, () => press('ArrowDown', 'ArrowDown')],
          [250, () => press('Enter', 'Enter', { mod: true })],
        ]),
    },
    { motion: true },
  ),
  clip(
    'delete-motion',
    '剪贴板 · 删除一条（残影淡出、下面的行合拢、高亮随新选中行移动）',
    {
      act: (ctx) =>
        steps(ctx, [
          [0, () => press('ArrowDown', 'ArrowDown')],
          [0, () => press('ArrowDown', 'ArrowDown')],
          [0, () => press('ArrowDown', 'ArrowDown')],
          [300, () => press(...deleteKey())],
          [60, () => ctx.motionMid()],
        ]),
    },
    { motion: true },
  ),
  clip(
    'clear-keep-pinned',
    '剪贴板 · 清空历史（保留置顶项）：被清掉的行一起淡出',
    {
      act: (ctx) =>
        steps(ctx, [
          [0, () => $<HTMLButtonElement>('[data-testid="clip-more"]')?.click()],
          [200, () => $<HTMLElement>('[data-testid="clip-more-menu-item-clear"]')?.click()],
          [400, () => press('Enter', 'Enter')],
          [50, () => ctx.motionMid()],
        ]),
    },
    { motion: true },
  ),
  focusKeyboard,
  scroll10k,
  ...REASONS.map(toast),
  {
    id: 'clipboard.paste-toast-wayland-inplace',
    label: 'toast · KDE Wayland 前端退路（面板持有焦点：关闭提示后再按粘贴键）',
    view: 'toast',
    manualSettle: true,
    setup(ctx) {
      showToast(ctx, 'wayland', true)
      ctx.after(700, () => ctx.settled())
    },
  },
]
