/**
 * mock 场景：面板外壳与组件展示页。
 *
 * - `shell.gallery`：组件展示页（全部组件 × 全部状态、每个动效 preset、panelEnter 三种 placement 的中间帧）；
 * - `shell.view-switch`：视图切换（输入 → 翻译、翻译 → 剪贴板、剪贴板 → 翻译），覆盖「宽度变化先淡出再淡入」与
 *   「宽度不变交叉淡入」两条路径；流畅度 trace 场景之一。默认用视觉稿内容（只看外壳的切换效果），
 *   加 `&views=real` 改用视图注册表里的真实视图；
 * - `shell.enter-below` / `shell.enter-above` / `shell.enter-center`：面板进入动效（动效类，QA 截中间帧）。
 * 配合 URL 参数：`hidden_raf=0` 验证 prepare 路径不依赖 rAF / ResizeObserver（40 ms 内 panel_ready），`dev=1` 显示延迟埋点；
 * `shell.view-switch` 加 `&cold=1` 跳过字体与 Intl 的预热，观察进程里第一次切换（启动后的空闲预热还没做完）的开销。
 */
import type { PanelPayload, Placement } from '../../types'
import type { MockContext, Scenario } from '../types'

async function applyDesignViews(): Promise<void> {
  if (new URLSearchParams(location.search).get('views') === 'real') return
  const [{ renderDesignByPayload }, { setViewOverride }] = await Promise.all([
    import('../../../../panel/shell/design/DesignViews'),
    import('../../../../panel/shell/devHost'),
  ])
  setViewOverride(renderDesignByPayload)
}

/**
 * 预先解码剪贴板样例里的占位图。mock 的应用图标 / 缩略图是 data: SVG（图标里还有文字）：Chromium 第一次排版带文字的
 * SVG 图片文档要 10 ms 以上，而且发生在插入 `<img>` 的那个任务里；真实应用里它们是 Rust 经 wisp:// 送来的 PNG，
 * 在主线程之外解码。`shell.view-switch` 是流畅度场景，量的是外壳的切换，不应该把 mock 占位图的一次性开销
 * 算进去，所以在采样开始（应用挂载）之前解码好；视觉稿的其他场景不受影响
 */
async function predecodeClipImages(ctx: MockContext): Promise<void> {
  const urls = new Set<string>()
  for (const item of ctx.fixtures.clipItems()) {
    if (item.source?.icon_url) urls.add(item.source.icon_url)
    if (item.image?.thumb_url) urls.add(item.image.thumb_url)
  }
  await Promise.all(
    [...urls].map((src) => {
      const img = new Image()
      img.src = src
      return img.decode().catch(() => {})
    }),
  )
}

/**
 * 常驻面板的稳态：真实应用里面板窗口在启动时隐藏创建，字体与 Intl 的一次性开销在启动后的空闲时间里付掉
 * （PanelShell → shared/perf/warmup），用户第一次切换视图时早已完成。mock 页面一加载就开始场景，空闲预热会和
 * 切换挤在同一段时间里，所以流畅度场景在挂载之前同步做完同样的预热（报告里另列了冷启动的数据）
 */
async function warmUpLikeResidentPanel(): Promise<void> {
  const { warmUp } = await import('../../../perf/warmup')
  warmUp()
}

function translate(ctx: MockContext): PanelPayload {
  return ctx.fixtures.payload.translate(null)
}

const enter = (placement: Placement): Scenario => ({
  id: `shell.enter-${placement}`,
  label: `面板进入 · ${placement}`,
  view: placement === 'center' ? 'clipboard' : 'translate',
  motion: true,
  async setup(ctx) {
    await applyDesignViews()
    const payload = placement === 'center' ? ctx.fixtures.payload.clipboard() : translate(ctx)
    ctx.preparePanel(payload, { placement, width: placement === 'center' ? undefined : 400 })
    // 进入动画进行到大约一半时标记中间帧（spring.snappy 约 250 ms 内基本到位）
    ctx.after(60, () => ctx.motionMid())
  },
})

export const scenarios: readonly Scenario[] = [
  {
    id: 'shell.gallery',
    label: '组件展示页',
    view: 'page',
    async setup() {
      const [{ Gallery }, { setPageOverride }, { createElement }] = await Promise.all([
        import('../../../../panel/shell/gallery/Gallery'),
        import('../../../../panel/shell/devHost'),
        import('react'),
      ])
      document.documentElement.dataset.window = 'gallery'
      setPageOverride(createElement(Gallery))
    },
  },
  {
    id: 'shell.view-switch',
    label: '视图切换（输入 → 翻译 → 剪贴板 → 翻译）',
    view: 'input',
    motion: true,
    manualSettle: true,
    async setup(ctx) {
      const cold = new URLSearchParams(location.search).get('cold') === '1'
      await Promise.all([
        applyDesignViews(),
        predecodeClipImages(ctx),
        cold ? null : warmUpLikeResidentPanel(),
      ])
      const { usePanelStore } = await import('../../../../panel/shell/panelStore')
      ctx.preparePanel(ctx.fixtures.payload.input(), { width: 420, focused: true })
      // 输入 → 翻译：视图内 switchView，宽度变化（先淡出、透明帧改宽度、再淡入）
      ctx.after(1200, () => usePanelStore.getState().switchPayload(translate(ctx), 380))
      // 翻译 → 剪贴板：Rust 发起的原位交叉淡入（transition = crossfade），宽度变化
      ctx.after(2600, () => {
        ctx.preparePanel(ctx.fixtures.payload.clipboard(), {
          transition: 'crossfade',
          placement: 'below',
          width: 420,
          focused: true,
        })
        ctx.after(60, () => ctx.motionMid())
      })
      // 剪贴板 → 翻译：宽度不变，交叉淡入
      ctx.after(4200, () => usePanelStore.getState().switchPayload(translate(ctx), 420))
      ctx.after(5400, () => ctx.settled())
    },
  },
  enter('below'),
  enter('above'),
  enter('center'),
]
