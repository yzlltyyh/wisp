/**
 * mock 场景：引导窗口。
 *
 * 每个场景 `{ id, label, view, setup(ctx) }`，id 以 `onboarding.` 开头；用 `?scenario=<id>` 打开（`&os=windows` 时没有
 * 权限步骤）。设置命令复用 `settings.ts` 的 `live()`；停在某一步用 `Boot.route`（0 欢迎、1 权限、2 接入模型、3 试一试）。
 * 权限场景里屏幕录制会在 1.6 s 后「授权」，演示卡片实时变绿；「试一试」场景会模拟一次 `onboarding:hotkey`
 * （已接入模型、权限就绪）；try-unconfigured 演示还没有模型 / 权限时任务上方的提示；load-error 演示读取设置失败。
 */
import type { MockHandle } from '../index'
import type { MockContext, Scenario } from '../types'
import {
  authErrorReport,
  clickWhenReady,
  live,
  model,
  okReport,
  seed,
  type LiveOptions,
} from './settings'

function step(ctx: MockContext, n: number): void {
  ctx.boot.route = { kind: 'onboarding', step: n }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms))
}

function scenario(
  id: string,
  label: string,
  n: number,
  setup: (ctx: MockContext, mock: MockHandle) => void = () => {},
  options: LiveOptions = {},
  providers = false,
  manualSettle = false,
): Scenario {
  return {
    id: `onboarding.${id}`,
    label,
    view: 'onboarding',
    manualSettle,
    setup(ctx) {
      const mock = live(ctx, options)
      seed(mock, { providers })
      mock.state.permissions = {
        accessibility: ctx.boot.os === 'macos' ? 'not_determined' : 'not_applicable',
        screen_recording: ctx.boot.os === 'macos' ? 'not_determined' : 'not_applicable',
        input_monitoring: ctx.boot.os === 'macos' ? 'not_determined' : 'not_applicable',
        lost_after_update: false,
      }
      step(ctx, n)
      setup(ctx, mock)
    },
  }
}

/** 在受控输入框里「输入」（React 只认原生 setter + input 事件） */
function typeInto(selector: string, value: string): void {
  const el = document.querySelector<HTMLInputElement>(selector)
  if (!el) return
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')?.set?.call(el, value)
  el.dispatchEvent(new Event('input', { bubbles: true }))
}

/** 元素出现后（每 50 ms 查一次，最多 5 s）再执行 */
function whenPresent(ctx: MockContext, selector: string, then: () => void): void {
  let tries = 0
  const tick = () => {
    if (document.querySelector(selector)) {
      then()
      return
    }
    tries += 1
    if (tries < 100) ctx.after(50, tick)
  }
  ctx.after(50, tick)
}

/** 两项权限都已授权（macOS；其他平台本来就是 not_applicable） */
function grantAll(ctx: MockContext, mock: MockHandle): void {
  if (ctx.boot.os !== 'macos') return
  mock.state.permissions = {
    ...mock.state.permissions,
    accessibility: 'granted',
    screen_recording: 'granted',
  }
}

/** 新服务的测试：成功，并能拉到模型（与 Rust 一样随测试报告带回合并好的列表；「拉取模型」按钮仍走 provider_models） */
function healthyService(ctx: MockContext): void {
  const models = () => [
    model('gpt-5.4-mini', 'yes'),
    model('gpt-5.4', 'yes', { reasoning: true }),
    model('gpt-5.4-nano', 'yes'),
  ]
  ctx.handle('provider_test', async ({ provider }) => {
    await sleep(600)
    return okReport(
      provider.protocol === 'anthropic_messages'
        ? 'https://api.anthropic.com/v1/messages'
        : 'https://api.openai.com/v1/responses',
      'gpt-5.4-mini',
      models(),
    )
  })
  ctx.handle('provider_models', async () => {
    await sleep(300)
    return models()
  })
}

export const scenarios: readonly Scenario[] = [
  scenario('welcome', '引导 · 欢迎', 0),
  scenario('permissions', '引导 · 权限（授权后实时变绿）', 1, (ctx, mock) => {
    mock.state.permissions = { ...mock.state.permissions, accessibility: 'granted' }
    ctx.handle('permissions_watch', ({ active }) => {
      if (!active || ctx.boot.os !== 'macos') return
      ctx.after(1600, () => {
        mock.state.permissions = { ...mock.state.permissions, screen_recording: 'granted' }
        mock.bus.emit('permissions:changed', mock.state.permissions)
      })
    })
  }),
  scenario('permissions-pending', '引导 · 权限（两项都未授权）', 1),
  scenario('permissions-lost', '引导 · 权限（更新后授权失效）', 1, (_ctx, mock) => {
    mock.state.permissions = {
      ...mock.state.permissions,
      accessibility: 'denied',
      screen_recording: 'granted',
      lost_after_update: true,
    }
  }),
  scenario('model', '引导 · 接入模型（四张卡片）', 2),
  scenario('model-others', '引导 · 接入模型（其他服务）', 2, (ctx) =>
    clickWhenReady(ctx, '[data-testid="card-others"]'),
  ),
  scenario(
    'model-form',
    '引导 · 接入模型（填写 Key、测试、选模型）',
    2,
    (ctx) => {
      healthyService(ctx)
      // 选 OpenAI → 填 Key（不按回车，草稿的 Key 随测试 / 保存写入）→ 测试 → 拉到模型后默认模型卡片展开
      clickWhenReady(ctx, '[data-testid="card-openai-responses"]', () =>
        whenPresent(ctx, '[data-testid="key-input"]', () => {
          typeInto('[data-testid="key-input"]', 'sk-proj-7f3a91c2')
          clickWhenReady(ctx, '[data-testid="onboarding-test"]', () =>
            // 默认模型卡片出现后等展开（spring.gentle）与滚入视野结束再标记稳定
            whenPresent(ctx, '[data-testid="onboarding-defaults"]', () =>
              ctx.after(900, () => ctx.settled()),
            ),
          )
        }),
      )
    },
    {},
    false,
    true,
  ),
  scenario('model-auth-error', '引导 · 接入模型（Key 无效）', 2, (ctx) => {
    ctx.handle('provider_test', async () => {
      await sleep(500)
      return authErrorReport('https://api.anthropic.com/v1/messages', 'claude-haiku-4-5')
    })
    // 填一个（会被拒绝的）Key 再测试：Key 字段就地提示「服务拒绝了这个 Key」并滚入视野
    clickWhenReady(ctx, '[data-testid="card-anthropic"]', () =>
      whenPresent(ctx, '[data-testid="key-input"]', () => {
        typeInto('[data-testid="key-input"]', 'sk-ant-expired-0000')
        ctx.after(300, () => clickWhenReady(ctx, '[data-testid="onboarding-test"]'))
      }),
    )
  }),
  scenario('model-configured', '引导 · 接入模型（已接入）', 2, undefined, {}, true),
  scenario(
    'try',
    '引导 · 试一试',
    3,
    (ctx, mock) => {
      grantAll(ctx, mock)
      ctx.after(900, () => ctx.emit('onboarding:hotkey', { action: 'clipboard' }))
    },
    {},
    true,
  ),
  scenario('try-unconfigured', '引导 · 试一试（还没有模型与权限）', 3),
  scenario(
    'try-broken',
    '引导 · 试一试（快捷键注册失败）',
    3,
    (ctx, mock) => {
      grantAll(ctx, mock)
      mock.state.hotkeys = mock.state.hotkeys.map((h) =>
        h.target.kind === 'action' && h.target.action === 'translate_selection'
          ? {
              ...h,
              effective: null,
              state: 'failed',
              reason_key: 'hotkeys.reason.both_unavailable',
            }
          : h,
      )
    },
    {},
    true,
  ),
  {
    id: 'onboarding.load-error',
    label: '引导 · 读取设置失败',
    view: 'onboarding',
    setup(ctx) {
      live(ctx, { loadError: true })
    },
  },
  {
    id: 'onboarding.loading',
    label: '引导 · 加载中',
    view: 'onboarding',
    manualSettle: true,
    setup(ctx) {
      live(ctx, { loadDelay: Infinity })
      ctx.after(400, () => ctx.settled())
    },
  },
]
