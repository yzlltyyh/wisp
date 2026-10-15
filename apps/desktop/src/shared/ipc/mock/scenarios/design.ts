/**
 * mock 场景：高保真视觉稿 design.*（各视图实现的视觉基准）。
 *
 * 面板类视觉稿走真实的面板会话（PanelCard、进入动效、高度跟随、假桌面），只把视图换成静态组合
 * （panel/shell/design/DesignViews.tsx）；设置 / 引导视觉稿整页替换为模拟窗口（DesignPages.tsx）。
 * 每个都用 URL 参数切换浅色 / 深色 × mac / win：`?scenario=design.translate-done&theme=dark&os=windows`。
 *
 * 只用动态导入引用 React 与视觉稿：mock 的静态模块图保持很小（vitest 里 runtime 测试加载 mock 时不会在环境拆除后
 * 还在加载 React）。
 */
import type { DesignViewId as ViewId } from '../../../../panel/shell/design/DesignViews'
import type { PanelPayload } from '../../types'
import type { MockContext, PrepareOptions, Scenario } from '../types'

async function showView(
  ctx: MockContext,
  id: ViewId,
  payload: PanelPayload,
  options: PrepareOptions,
) {
  const [{ renderDesignView }, { setViewOverride }] = await Promise.all([
    import('../../../../panel/shell/design/DesignViews'),
    import('../../../../panel/shell/devHost'),
  ])
  setViewOverride(renderDesignView(id))
  ctx.preparePanel(payload, options)
}

function translatePayload(ctx: MockContext, dict = false): PanelPayload {
  const p = ctx.fixtures.payload.translate(null)
  return p.kind === 'translate' ? { ...p, dict_mode: dict } : p
}

const panel = (
  id: ViewId,
  label: string,
  view: PanelPayload['kind'],
  payload: (ctx: MockContext) => PanelPayload,
  options: PrepareOptions = {},
): Scenario => ({
  id: `design.${id}`,
  label,
  view,
  setup: (ctx) => showView(ctx, id, payload(ctx), options),
})

export const scenarios: readonly Scenario[] = [
  panel('translate-streaming', '翻译 · 流式中', 'translate', (c) => translatePayload(c), {
    width: 420,
  }),
  panel('translate-done', '翻译 · 完成', 'translate', (c) => translatePayload(c), { width: 420 }),
  panel('translate-dict', '翻译 · 词典式', 'translate', (c) => translatePayload(c, true), {
    width: 360,
  }),
  panel('translate-error', '翻译 · 错误', 'translate', (c) => translatePayload(c), { width: 420 }),
  panel('ocr-recognizing', 'OCR · 识别中', 'ocr', (c) => c.fixtures.payload.ocr(null), {
    width: 440,
  }),
  panel('ocr-done', 'OCR · 完成 + 译文区', 'ocr', (c) => c.fixtures.payload.ocr(null), {
    width: 440,
  }),
  panel('clipboard-list', '剪贴板 · 混合类型列表', 'clipboard', (c) =>
    c.fixtures.payload.clipboard(),
  ),
  panel('clipboard-search', '剪贴板 · 搜索高亮', 'clipboard', (c) =>
    c.fixtures.payload.clipboard(),
  ),
  panel('clipboard-empty', '剪贴板 · 空状态', 'clipboard', (c) => c.fixtures.payload.clipboard([])),
  panel('input', '输入翻译', 'input', (c) => c.fixtures.payload.input(), { width: 420 }),
  {
    id: 'design.settings-models',
    label: '设置 · 模型与服务',
    view: 'page',
    async setup() {
      const [{ SettingsModelsPage }, { setPageOverride }, { createElement }] = await Promise.all([
        import('../../../../panel/shell/design/DesignPages'),
        import('../../../../panel/shell/devHost'),
        import('react'),
      ])
      setPageOverride(createElement(SettingsModelsPage))
    },
  },
  {
    id: 'design.onboarding-permissions',
    label: '引导 · 权限',
    view: 'page',
    async setup() {
      const [{ OnboardingPermissionsPage }, { setPageOverride }, { createElement }] =
        await Promise.all([
          import('../../../../panel/shell/design/DesignPages'),
          import('../../../../panel/shell/devHost'),
          import('react'),
        ])
      setPageOverride(createElement(OnboardingPermissionsPage))
    },
  },
]
