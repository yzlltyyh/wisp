/**
 * 首次启动引导：欢迎 → 权限（仅 macOS）→ 接入模型 → 试一试。
 *
 * - 每一步和整个引导都可以跳过；「稍后设置」（跳过整个引导）与完成一样调用 `onboarding_complete`（下次启动不再自动打开），
 *   然后关闭窗口；之后可以从托盘「使用引导…」重新进入；
 * - 步骤编号（`WindowTarget.onboarding.step`、`boot.route`、`window:navigate`）：0 欢迎、1 权限、2 接入模型、3 试一试；
 *   Windows / Linux 没有权限步骤，1 视为接入模型；更新后授权失效（`lost_after_update`）时直接从权限开始；
 *   「重新启动 Wisp」经 `app_relaunch(resume)` 让 Rust 把当前步骤写进 state.json，重启后由 `boot.route` 回到这一步
 *   （src-tauri/src/app/relaunch.rs）；
 * - 权限轮询（`permissions_watch`）只在权限步骤显示、且窗口可见时开启，离开即停；
 * - 步骤条的对勾只看真实状态：权限两项都已授权、已经接入模型并选好翻译模型才算完成；走过但没完成的步骤显示「已跳过」
 *   （空心的中性样式），不画对勾；
 * - 版式：步骤条与底栏是固定的外框。步骤条叠在内容区顶部、不占欢迎页的布局（离开欢迎页时与新步骤一起淡入
 *   presets.stepChrome，减少动效时 ≤ 100 ms 纯淡入）；底栏由各步经 portal 填入（shared.tsx），换步骤时旧按钮组原地淡出、
 *   新按钮组与正文同时淡入（不硬切）；
 * - 步骤切换新旧交叉进出（presets.stepCross）：旧步骤原地 100 ms 淡出（ease.exit），新步骤延后 60 ms 再 x ±8→0 + 淡入——
 *   两种排版完全不同的步骤只在低不透明度区间重叠；减少动效时只淡入淡出；
 * - 键盘：换步骤后焦点交给新步骤的标题（读屏播报，下一次 Tab 从这一步开始）；焦点不在任何控件上时 Return 触发底栏的
 *   主按钮（`data-default`，macOS 引导的默认按钮）；⌘W 关窗；
 * - 「接入模型」的草稿放在这里（steps/modelDraft.ts），离开这一步再回来不会丢；
 * - ⌘W 与「完成 / 稍后设置」关窗前先放弃没保存的草稿 Key、提交延迟写入并等它们返回（settings/lib/writes.ts）；
 * - 读取设置失败：说明 + 「重试」+「稍后设置」，不会停在骨架上；
 * - macOS 标题栏是 Overlay：顶部留出交通灯的空间，顶部条带可拖动窗口；页面底色只取 `--window-bg`。
 */
import '../settings/settings.css'
import './onboarding.css'
import { Check, FileWarning } from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { startTransition, useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { usePlatform } from '../shared/hooks/usePlatform'
import { useT } from '../shared/i18n/useT'
import { api, boot, getTransport, on } from '../shared/ipc'
import type { OsKind, PermissionState } from '../shared/ipc/types'
import { useKeyScope } from '../shared/keys/useKeyScope'
import { MotionRoot } from '../shared/motion/MotionRoot'
import { adaptPreset, presets } from '../shared/motion/presets'
import { useReducedMotionPref } from '../shared/motion/useReducedMotionPref'
import { Button, EmptyState, Icon, Skeleton } from '../shared/ui'
import { prefetchAdvice } from '../settings/components/AdviceBanners'
import { NoticeToast } from '../settings/components/NoticeToast'
import { ipcErrorText } from '../settings/lib/errors'
import { closeWindowSafely, prepareToLeave } from '../settings/lib/writes'
import { translateReady } from '../settings/providers/engine'
import {
  loadSettings,
  notify,
  setPermissions,
  setSaveErrorHandler,
  startSettingsSync,
  useRuntime,
  useSettings,
  useSettingsStore,
} from '../settings/store'
import { startOnboardingActions } from './actions'
import { loadOnboardingMessages } from './i18n'
import { FooterSlot, focusStepTitle, STEP_NUMBERS, type StepId, type StepProps } from './shared'
import { Model } from './steps/Model'
import { useModelDraft } from './steps/modelDraft'
import { Permissions } from './steps/Permissions'
import { Try } from './steps/Try'
import { Welcome } from './steps/Welcome'

loadOnboardingMessages()

/** 引导步骤（macOS 3 步，其他平台 2 步；欢迎页不计入步骤条） */
export function stepsFor(os: OsKind): readonly StepId[] {
  return os === 'macos' ? ['welcome', 'permissions', 'model', 'try'] : ['welcome', 'model', 'try']
}

/** 路由里的步骤编号 → 步骤；不存在的步骤（其他平台的权限）落到下一步 */
export function stepFromNumber(n: number | null | undefined, os: OsKind): StepId | null {
  if (n === null || n === undefined || !Number.isInteger(n) || n < 0) return null
  const steps = stepsFor(os)
  return steps.find((s) => STEP_NUMBERS[s] >= n) ?? steps[steps.length - 1] ?? null
}

/** 权限是否就绪（已授权，或这个平台不需要） */
export const permissionReady = (s: PermissionState | undefined) =>
  s === 'granted' || s === 'not_applicable'

/**
 * 初始步骤：路由指定的（含重启续接：重启前 `app_relaunch(resume)` 记下的步骤由 Rust 写进 state.json、下次启动随 Boot.route
 * 带回）；没有时为 null（欢迎页）
 */
function initialStep(): StepId | null {
  const r = boot.route
  return r?.kind === 'onboarding' ? stepFromNumber(r.step, boot.os) : null
}

/** 焦点在这些元素上时 Return 交给它们自己（按钮、链接、单选……），不触发底栏的默认按钮 */
const INTERACTIVE =
  'button, a[href], input, textarea, select, [role="radio"], [role="tab"], [tabindex="0"]'

export function OnboardingApp() {
  return (
    <MotionRoot>
      <OnboardingWindow />
    </MotionRoot>
  )
}

function OnboardingWindow() {
  const t = useT()
  const os = usePlatform()
  const steps = stepsFor(os)
  const runtime = useRuntime()
  const settings = useSettings()
  const loaded = useSettingsStore((s) => s.snap !== null)
  const loadError = useSettingsStore((s) => s.loadError)
  const reduced = useReducedMotionPref()
  const [step, setStep] = useState<StepId | null>(initialStep)
  const [dir, setDir] = useState<1 | -1>(1)
  const [visible, setVisible] = useState(() => document.visibilityState !== 'hidden')
  const [footer, setFooter] = useState<HTMLElement | null>(null)
  const stage = useRef<HTMLElement>(null)
  const current: StepId = step ?? 'welcome'
  /** 换过步骤：之后的步骤才播进入动画（窗口打开时的第一步不动） */
  const [switched, setSwitched] = useState(false)
  const modelDraft = useModelDraft()

  useEffect(() => {
    setSaveErrorHandler((error) =>
      notify(t('settings.notice.save_failed', { reason: ipcErrorText(error) }), 'danger'),
    )
  }, [t])

  useEffect(() => {
    const stop = startSettingsSync()
    // 引导窗口在前台时被 Rust 拦下的全局动作：任何一步都转发
    const stopActions = startOnboardingActions()
    void loadSettings()
    // 「试一试」的邻居冲突 / 键盘布局提示：提前取好，到那一步时首帧就能渲染
    const idle = setTimeout(() => void prefetchAdvice(), 0)
    const onVisibility = () => setVisible(document.visibilityState !== 'hidden')
    document.addEventListener('visibilitychange', onVisibility)
    return () => {
      clearTimeout(idle)
      stop()
      stopActions()
      document.removeEventListener('visibilitychange', onVisibility)
    }
  }, [])

  // 更新后授权失效：还在欢迎页、且没有指定路由时直接从权限步骤开始
  const lost = runtime?.permissions.lost_after_update ?? false
  if (lost && step === null && os === 'macos') setStep('permissions')

  const goTo = useCallback(
    (next: StepId) => {
      const order = stepsFor(os)
      // 过渡更新：新步骤的渲染按时间片进行，点击之后的那一帧不被同步挂载整个步骤（权限步骤约 40 ms）卡住，按钮的
      // 按下反馈立即出现；提交时新旧步骤交叉进出（见下方 AnimatePresence）
      startTransition(() => {
        setDir(order.indexOf(next) >= order.indexOf(current) ? 1 : -1)
        setSwitched(true)
        setStep(next)
      })
    },
    [os, current],
  )

  // 已打开时的深链（托盘「权限缺失」→ 第 1 步）
  useEffect(() => {
    let disposed = false
    let unlisten: (() => void) | null = null
    void on('window:navigate', (target) => {
      if (target.kind !== 'onboarding') return
      const s = stepFromNumber(target.step, os)
      if (s) goTo(s)
    }).then((un) => {
      if (disposed) un()
      else unlisten = un
    })
    return () => {
      disposed = true
      unlisten?.()
    }
  }, [os, goTo])

  // 换步骤之后：焦点交给新步骤的标题（读屏播报；原来的底栏按钮已经随 portal 卸载，焦点不能掉到 body 上）
  const focusedStep = useRef(current)
  useEffect(() => {
    if (focusedStep.current === current) return
    focusedStep.current = current
    // 只在新步骤里找标题：旧步骤还在原位淡出（AnimatePresence），它的标题排在前面
    focusStepTitle(
      stage.current?.querySelector<HTMLElement>(`.ob-step-host[data-step-host="${current}"]`) ??
        null,
    )
  }, [current])

  // 权限轮询只在权限步骤可见时存在（离开引导页后不再轮询）
  const watching = current === 'permissions' && visible
  useEffect(() => {
    if (!watching) return
    void api.permissionsWatch(true).catch(() => undefined)
    void api
      .permissionsStatus()
      .then((p) => {
        if (p) setPermissions(p)
      })
      .catch(() => undefined)
    return () => {
      void api.permissionsWatch(false).catch(() => undefined)
    }
  }, [watching])

  const finish = useCallback(async () => {
    try {
      // 没保存的草稿 Key 删掉、延迟写入提交并等它们返回，再记为完成并关窗
      await prepareToLeave()
      await api.onboardingComplete()
    } catch (error) {
      notify(ipcErrorText(error, 'onboarding_complete'), 'danger')
      return
    }
    await getTransport().window.close()
  }, [])

  useKeyScope(
    'onboarding-window',
    [
      // ⌘W / Ctrl+W：先放弃草稿 Key、提交延迟写入并等它们返回，再关窗
      { combo: 'Mod+KeyW', allowInInput: true, run: () => void closeWindowSafely() },
      // Return：焦点不在任何控件上时触发底栏的主按钮（macOS 引导的默认按钮）
      {
        combo: 'Enter',
        when: () =>
          !(
            document.activeElement instanceof Element && document.activeElement.matches(INTERACTIVE)
          ),
        run: () => {
          const target = footer?.querySelector<HTMLButtonElement>('[data-default]:not(:disabled)')
          if (!target) return false
          target.click()
          return true
        },
      },
    ],
    { layer: 'global' },
  )

  const index = steps.indexOf(current)
  const props: StepProps = {
    onNext: () => {
      const next = steps[index + 1]
      if (next) goTo(next)
      else void finish()
    },
    onBack: () => {
      const prev = steps[index - 1]
      if (prev) goTo(prev)
    },
    onSkipAll: () => void finish(),
    onFinish: () => void finish(),
    onGoTo: goTo,
    canBack: index > 1,
  }

  // 步骤条的完成状态：只看真实状态
  const perms = runtime?.permissions
  const done: Record<StepId, boolean> = {
    welcome: true,
    permissions: permissionReady(perms?.accessibility) && permissionReady(perms?.screen_recording),
    // 接入了服务并选了默认翻译模型，或者在本机选用了离线翻译模型（settings/providers/engine.ts）
    model: translateReady(settings),
    try: false,
  }

  const motion = adaptPreset(presets.stepCross(dir), 'fade', reduced)
  // 步骤条与新步骤一起淡入（减少动效时 ≤ 100 ms 纯淡入）
  const stepperIn = adaptPreset(presets.stepChrome, 'fade', reduced)
  const failed = !loaded && loadError !== null
  const footerSlot = useMemo(() => ({ el: footer, animate: switched }), [footer, switched])

  return (
    <FooterSlot.Provider value={footerSlot}>
      <div className="ob-window" data-os={os} data-step={current} data-testid="onboarding-app">
        <div className="ob-drag" data-tauri-drag-region />
        <div className="ob-stepper-slot">
          {current !== 'welcome' && loaded ? (
            // 离开欢迎页时淡入；回到欢迎页时直接消失（欢迎页不显示步骤条）
            <m.div initial={switched ? stepperIn.initial : false} animate={stepperIn.animate}>
              <Stepper
                steps={steps.filter((s) => s !== 'welcome')}
                current={current}
                done={done}
                onPick={goTo}
              />
            </m.div>
          ) : null}
        </div>
        <main ref={stage} className="ob-stage" aria-label={t('onboarding.window_label')}>
          {loaded ? (
            // 新旧步骤交叉进出（presets.stepCross：旧步骤原地 100 ms 淡出，新步骤延后 60 ms 再 x ±8 + 淡入）：旧步骤
            // 脱离排版叠在原位淡出（popLayout），两层只在低不透明度区间重叠
            <AnimatePresence mode="popLayout" initial={false}>
              <m.div
                key={current}
                className="ob-step-host"
                data-step-host={current}
                data-stepper={current !== 'welcome' || undefined}
                initial={switched ? motion.initial : false}
                animate={motion.animate}
                exit={motion.exit}
              >
                {current === 'welcome' ? (
                  <Welcome {...props} />
                ) : current === 'permissions' ? (
                  <Permissions {...props} />
                ) : current === 'model' ? (
                  <Model {...props} store={modelDraft} />
                ) : (
                  <Try {...props} />
                )}
              </m.div>
            </AnimatePresence>
          ) : failed ? (
            <LoadError error={loadError} onSkip={() => void finish()} />
          ) : (
            <div className="ob-loading" aria-busy="true" data-testid="onboarding-loading">
              <Skeleton lines={3} />
            </div>
          )}
        </main>
        <footer ref={setFooter} className="ob-foot" data-testid="onboarding-foot" />
        <NoticeToast />
      </div>
    </FooterSlot.Provider>
  )
}

/** 读取设置失败：说明 + 重试 + 稍后设置（不会一直停在骨架上，只能 ⌘W） */
function LoadError({
  error,
  onSkip,
}: {
  error: Parameters<typeof ipcErrorText>[0]
  onSkip: () => void
}) {
  const t = useT()
  return (
    <div className="ob-load-error" data-testid="onboarding-load-error">
      <EmptyState
        icon={FileWarning}
        title={t('settings.load_failed')}
        detail={
          <span className="st-load-error">
            <span>{ipcErrorText(error)}</span>
            <span>{t('settings.load_failed_detail')}</span>
          </span>
        }
        action={
          <span className="st-empty-actions">
            <Button onClick={onSkip} data-testid="onboarding-load-error-skip">
              {t('onboarding.skip_all')}
            </Button>
            <Button
              variant="primary"
              onClick={() => void loadSettings()}
              data-testid="onboarding-retry"
            >
              {t('common.action.retry')}
            </Button>
          </span>
        }
      />
    </div>
  )
}

function Stepper({
  steps,
  current,
  done,
  onPick,
}: {
  steps: readonly StepId[]
  current: StepId
  done: Record<StepId, boolean>
  onPick: (s: StepId) => void
}) {
  const t = useT()
  const reduced = useReducedMotionPref()
  const pulse = adaptPreset(presets.statusPulse, 'none', reduced)
  const at = steps.indexOf(current)
  return (
    <nav
      className="ob-stepper"
      aria-label={t('onboarding.steps_label')}
      data-testid="onboarding-stepper"
    >
      {steps.map((s, i) => {
        const isCurrent = s === current
        // 对勾只看真实状态；走过但没完成的步骤是「已跳过」（中性的空心样式）
        const state = isCurrent ? 'current' : done[s] ? 'done' : i < at ? 'skipped' : 'todo'
        const prev = steps[i - 1]
        return (
          <span key={s} className="ob-stepper-item">
            {i > 0 && prev ? (
              <span className="ob-step-line" data-done={done[prev] || undefined} />
            ) : null}
            <button
              type="button"
              className="ob-step-pill"
              data-state={state}
              aria-current={isCurrent ? 'step' : undefined}
              onClick={() => onPick(s)}
              data-testid={`onboarding-step-${s}`}
            >
              <m.b key={state === 'done' ? 'done' : 'n'} {...pulse} aria-hidden="true">
                {state === 'done' ? <Icon icon={Check} size={11} /> : i + 1}
              </m.b>
              {t(`onboarding.step.${s}` as 'onboarding.step.model')}
              {state === 'done' || state === 'skipped' ? (
                <span className="st-sr-only">
                  {t(state === 'done' ? 'onboarding.step_done' : 'onboarding.step_skipped')}
                </span>
              ) : null}
            </button>
          </span>
        )
      })}
    </nav>
  )
}
