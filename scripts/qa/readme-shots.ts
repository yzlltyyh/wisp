/**
 * scripts/qa/readme-shots.ts：README 截图（docs/images/）的清单与核对。
 *
 * 每张图记录「场景 id → 主题 → 视口 → 裁切框 → 输出宽度」，由 `pnpm qa:shots --readme` 一键重新生成（screenshots.ts 的
 * README 档位：WebKit ≈ WKWebView、macOS 外观、简体中文、不减少动效；等 data-settled 与画面静止后按裁切框截取）。
 * 裁切框是 CSS 像素；按 2× 渲染（与 macOS 的 Retina 屏一致，macOS 没有小数缩放），再在浏览器里用高质量插值缩小到输出
 * 宽度，输出高度 = round(裁切框高度 × 输出宽度 / 裁切框宽度)。不直接用小数 deviceScaleFactor 渲染：WebKit 在 1.5、
 * 7/6 这类缩放下 Overlay 的暗化层与选区错位（Chromium 正常），也会让 mock 按 devicePixelRatio 推算的截图尺寸变成
 * 853 × 320 pt 这样的怪数。
 *
 * 构图沿用 2026-10-12 手工制作时的样子：面板类截图是假桌面上的「Notes」窗口（mock 的 .app 在 1280 × 800 视口里位于
 * (90, 72)、760 × 520）加少量壁纸，面板锚在选区下方；框选截图取冻结帧的大半个屏幕（不随主题变化，只有一张）；设置窗口按默认窗口尺寸
 * （tauri.conf.json 的 760 × 560）整页截取。场景用真实视图（translate.* / ocr.* / clipboard.* / overlay.* / settings.*），
 * 不用 design.* 的静态视觉稿。同一天里两次生成的结果逐字节相同；剪贴板列表的相对时间（「3 天」「10月4日」）按生成当天
 * 计算，隔天重新生成会有这类细小差别。
 *
 * 只用可擦除的 TS 语法（Node 类型剥离直接运行），不依赖 Playwright 类型：截图逻辑在 screenshots.ts。
 */

export interface Rect {
  x: number
  y: number
  width: number
  height: number
}

export interface ReadmeShot {
  /** docs/images 下的文件名 */
  file: string
  /** mock 场景 id（`pnpm qa:shots --list`） */
  scenario: string
  theme: 'light' | 'dark'
  /** 页面视口（CSS 像素） */
  viewport: { width: number; height: number }
  /** 裁切框（CSS 像素，相对视口左上角） */
  clip: Rect
  /** 输出宽度（像素） */
  width: number
  /**
   * 画面静止之后再等这么久（毫秒）、再等一次静止：跳过短暂的提示（例如 OCR 自动复制后底栏 1.5 s 的「已复制」，
   * motion tokens 的 toastHold），否则截到的内容取决于机器快慢
   */
  hold?: number
}

/** 单张上限：README 在 GitHub 上逐张加载，超过时报错（构图或场景内容需要收敛） */
export const MAX_BYTES = 500 * 1024
/** 渲染倍率（macOS Retina）；输出宽度不等于裁切框宽度 × 它时再缩小 */
export const RENDER_SCALE = 2

/** 原样复制的图标（不经截图） */
export const README_ICON = {
  file: 'icon.png',
  from: 'apps/desktop/src-tauri/icons/128x128@2x.png',
} as const

/** 面板类场景的视口：mock 假桌面按它布局（.app 窗口在 (90, 72)、760 × 520） */
const DESKTOP = { width: 1280, height: 800 }
/** 设置窗口的默认尺寸（tauri.conf.json 的 settings 窗口） */
const SETTINGS_WINDOW = { width: 760, height: 560 }

/**
 * 不随主题变化的画面只出一张 `<name>.png`（README 里不用 `<picture>`）：框选是冻结帧 + Overlay，Overlay 的选区、尺寸标签与
 * 放大镜按设计在浅色 / 深色主题下一致（overlay/select.css 的说明），两张会一模一样，只是多占仓库体积
 */
function single(
  name: string,
  scenario: string,
  viewport: ReadmeShot['viewport'],
  clip: Rect,
  width: number,
): ReadmeShot[] {
  return [{ file: `${name}.png`, scenario, theme: 'light', viewport, clip, width }]
}

/** 同一构图的浅色 / 深色两张：`<name>-light.png`、`<name>-dark.png`（README 用 `<picture>` 按读者的主题显示） */
function pair(
  name: string,
  scenario: string,
  viewport: ReadmeShot['viewport'],
  clip: Rect,
  width: number,
  hold?: number,
): ReadmeShot[] {
  return (['light', 'dark'] as const).map((theme) => ({
    file: `${name}-${theme}.png`,
    scenario,
    theme,
    viewport,
    clip,
    width,
    ...(hold === undefined ? {} : { hold }),
  }))
}

export const README_SHOTS: readonly ReadmeShot[] = [
  // 划词翻译：选区下方的译文面板（420 宽，(163, 268) 起），Notes 窗口四周各留约 10 px 壁纸
  ...pair('translate', 'translate.done', DESKTOP, { x: 80, y: 55, width: 800, height: 564 }, 1200),
  // 词典式结果：serendipity（360 宽）
  ...pair('dictionary', 'translate.dict', DESKTOP, { x: 80, y: 55, width: 800, height: 550 }, 1200),
  // 框选：冻结帧上的选区、尺寸标签、四角把手与放大镜（取屏幕的大半，输出 1400 宽）；不随主题变化，只出一张
  ...single('overlay', 'overlay.select', DESKTOP, { x: 40, y: 54, width: 1200, height: 648 }, 1400),
  // OCR：识别结果 + 展开的译文区（440 宽，下沿到 y ≈ 724，超出 Notes 窗口）；等「已复制」收起，底栏显示模型与耗时
  ...pair('ocr', 'ocr.translate', DESKTOP, { x: 80, y: 55, width: 800, height: 700 }, 1200, 2000),
  // 剪贴板：置顶 / 最近混合列表（420 宽，(430, 163) 起，右沿与 Notes 窗口对齐）
  ...pair('clipboard', 'clipboard.list', DESKTOP, { x: 80, y: 55, width: 800, height: 600 }, 1200),
  // 设置窗口「模型与服务」：默认窗口尺寸整页
  ...pair(
    'settings-providers',
    'settings.providers',
    SETTINGS_WINDOW,
    { x: 0, y: 0, ...SETTINGS_WINDOW },
    1200,
  ),
]

/** 输出图片的预期尺寸（像素） */
export function expectedSize(shot: ReadmeShot): { width: number; height: number } {
  return {
    width: shot.width,
    height: Math.round(shot.clip.height * (shot.width / shot.clip.width)),
  }
}

/** 清单自洽：文件名唯一、裁切框在视口内、尺寸为正、只缩小不放大（错误列表，空表示通过） */
export function validateShots(shots: readonly ReadmeShot[]): string[] {
  const problems: string[] = []
  const seen = new Set<string>()
  for (const s of shots) {
    if (seen.has(s.file)) problems.push(`${s.file}：文件名重复`)
    seen.add(s.file)
    if (s.file === README_ICON.file) problems.push(`${s.file}：与图标同名`)
    const { x, y, width, height } = s.clip
    if (!(width > 0 && height > 0 && s.width > 0))
      problems.push(`${s.file}：裁切框或输出宽度不是正数`)
    if (x < 0 || y < 0 || x + width > s.viewport.width || y + height > s.viewport.height) {
      problems.push(
        `${s.file}：裁切框 ${x},${y} ${width}×${height} 超出视口 ${s.viewport.width}×${s.viewport.height}`,
      )
    }
    if (s.width > width * RENDER_SCALE)
      problems.push(
        `${s.file}：输出宽度 ${s.width} 超过 ${RENDER_SCALE}× 渲染的 ${width * RENDER_SCALE}`,
      )
  }
  return problems
}

/** PNG 的像素尺寸（IHDR）；不是 PNG 时返回 null */
export function pngSize(buf: Uint8Array): { width: number; height: number } | null {
  const sig = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]
  if (buf.length < 24 || sig.some((b, i) => buf[i] !== b)) return null
  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength)
  return { width: view.getUint32(16), height: view.getUint32(20) }
}

/** README 里引用的 docs/images 文件名（按出现顺序去重） */
export function readmeImageRefs(readme: string): string[] {
  const refs = [...readme.matchAll(/docs\/images\/([\w.-]+\.(?:png|jpe?g|gif|svg|webp))/g)].map(
    (m) => m[1] as string,
  )
  return [...new Set(refs)]
}

/**
 * 核对 README 与清单：`missing` 是 README 引用了、目录里却没有的文件；`unlisted` 是 README 引用了、但不由清单生成的
 * 文件（图标除外）；`unreferenced` 是清单生成了、README 没有引用的文件
 */
export function checkReadmeRefs(
  readme: string,
  shots: readonly ReadmeShot[],
  existing: ReadonlySet<string>,
): { missing: string[]; unlisted: string[]; unreferenced: string[] } {
  const refs = readmeImageRefs(readme)
  const listed = new Set([...shots.map((s) => s.file), README_ICON.file])
  return {
    missing: refs.filter((f) => !existing.has(f)),
    unlisted: refs.filter((f) => !listed.has(f)),
    unreferenced: [...listed].filter((f) => !refs.includes(f)),
  }
}
