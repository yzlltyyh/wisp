/**
 * 引导第 3 步：试一试。
 *
 * - 一段示例英文；引导选中后按划词翻译的快捷键（显示实际生效的组合：macOS ⌥D，Windows Ctrl+Alt+D），面板出现后打勾；
 *   再引导按剪贴板的快捷键，打勾；
 * - Wisp 自己的窗口在前台时取词不走系统通道：Rust 把热键转成 `onboarding:hotkey`，窗口级的转发（actions.ts）用
 *   `window.getSelection()` 取得文本后调用 `trigger_action(…, origin: onboarding)`，这里订阅执行结果打勾；
 * - 快捷键注册失败 / 未设置时先在这里设置快捷键：任务换成不依赖组合键的完整句子，原因只说一行
 *   （「⌥D：已被其他程序占用，换一个组合即可」），下面是就地换键的录制器（标红显示没注册上的组合）；
 *   快捷键在当前环境不可用（Wayland）时提供「直接试试」；
 * - 邻居应用冲突与键盘布局建议；
 * - 还没接入模型（翻译会失败）、或 macOS 上还没有辅助功能权限时，任务上方先说明，并给一个回到那一步的按钮——
 *   步骤条可以直接跳到这一步，不能假设前面的步骤都完成了。
 */
import { Accessibility, Check, MousePointerClick, Server } from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useEffect, useRef, useState, type ReactNode } from 'react'
import { Trans } from '../../shared/i18n/Trans'
import { useT, useTKey } from '../../shared/i18n/useT'
import { api } from '../../shared/ipc'
import type { HotkeyAction, HotkeyStatus } from '../../shared/ipc/types'
import { usePreset } from '../../shared/motion/presets'
import { usePlatform } from '../../shared/hooks/usePlatform'
import { Banner, Button, Icon, Kbd } from '../../shared/ui'
import { AdviceBanners } from '../../settings/components/AdviceBanners'
import { HotkeyIssue, HotkeyRow } from '../../settings/components/HotkeyRow'
import { translateReady } from '../../settings/providers/engine'
import { setHotkeys, useRuntime, useSettings } from '../../settings/store'
import { onOnboardingAction, runOnboardingAction, setOnboardingSample } from '../actions'
import { StepFooter, StepLayout, type StepProps } from '../shared'

type Task = 'translate_selection' | 'clipboard'
const TASKS: readonly Task[] = ['translate_selection', 'clipboard']

function statusOf(hotkeys: readonly HotkeyStatus[] | undefined, action: HotkeyAction) {
  return hotkeys?.find((h) => h.target.kind === 'action' && h.target.action === action)
}

export function Try({ onBack, onFinish, onGoTo, canBack }: StepProps) {
  const t = useT()
  const os = usePlatform()
  const runtime = useRuntime()
  const settings = useSettings()
  const rowInsert = usePreset('rowInsert')
  // 离线翻译模型同样算已接入（还在下载时翻译面板会显示进度）
  const noModel = !translateReady(settings)
  const perm = runtime?.permissions.accessibility
  const noAccessibility = os === 'macos' && perm !== 'granted' && perm !== 'not_applicable'
  const [done, setDone] = useState<ReadonlySet<Task>>(() => new Set())
  const sample = useRef<HTMLParagraphElement>(null)

  // 窗口级转发（actions.ts）执行成功后打勾；「直接试试」没有选中文字时用示例段落
  useEffect(
    () =>
      onOnboardingAction((action) => {
        if (action === 'translate_selection' || action === 'clipboard') {
          setDone((prev) => new Set(prev).add(action))
        }
      }),
    [],
  )
  const tryNow = (task: Task) =>
    void runOnboardingAction(task, sample.current?.textContent?.trim() ?? null)
  // 转发来的划词翻译没有选中文字时，译这段示例（与「直接试试」一致）
  const sampleText = t('onboarding.try.sample')
  useEffect(() => {
    setOnboardingSample(sampleText)
    return () => setOnboardingSample(null)
  }, [sampleText])

  // 进入这一步时重新取一次快捷键状态（Rust 对备选 / 失败项整体重试一次）
  useEffect(() => {
    let alive = true
    api
      .hotkeyStatus()
      .then((list) => {
        if (alive && Array.isArray(list) && list.length > 0) setHotkeys(list)
      })
      .catch(() => undefined)
    return () => {
      alive = false
    }
  }, [])

  const hotkeys = runtime?.hotkeys
  const ocrCombo = statusOf(hotkeys, 'ocr')?.effective ?? null
  const allDone = TASKS.every((x) => done.has(x))

  return (
    <StepLayout
      title={t('onboarding.try.title')}
      description={t('onboarding.try.description')}
      data-testid="onboarding-try"
      footer={
        <StepFooter
          onBack={onBack}
          canBack={canBack}
          next={
            <Button
              variant="primary"
              size="md"
              onClick={onFinish}
              data-default
              data-testid="onboarding-finish"
            >
              {allDone ? t('onboarding.try.finish_done') : t('common.action.done')}
            </Button>
          }
        />
      }
    >
      <AnimatePresence initial={false}>
        {noModel ? (
          <m.div key="no-model" className="ob-banner" {...rowInsert}>
            <Banner
              tone="warning"
              icon={Server}
              className="st-banner-top"
              data-testid="try-no-model"
              action={
                <Button size="sm" variant="ghost" onClick={() => onGoTo('model')}>
                  {t('onboarding.try.go_model')}
                </Button>
              }
            >
              {t('onboarding.try.no_model')}
            </Banner>
          </m.div>
        ) : null}
        {noAccessibility ? (
          <m.div key="no-permission" className="ob-banner" {...rowInsert}>
            <Banner
              tone="warning"
              icon={Accessibility}
              className="st-banner-top"
              data-testid="try-no-permission"
              action={
                <Button size="sm" variant="ghost" onClick={() => onGoTo('permissions')}>
                  {t('onboarding.try.go_permissions')}
                </Button>
              }
            >
              {t('onboarding.try.no_permission')}
            </Banner>
          </m.div>
        ) : null}
      </AnimatePresence>
      <div className="ob-sample-card">
        <p ref={sample} className="ob-sample" data-testid="onboarding-sample">
          {t('onboarding.try.sample')}
        </p>
      </div>
      <ol className="ob-tasks">
        {TASKS.map((task, i) => (
          <TaskRow
            key={task}
            index={i + 1}
            done={done.has(task)}
            task={task}
            status={statusOf(hotkeys, task)}
            onTry={() => tryNow(task)}
            testId={`task-${task}`}
          />
        ))}
      </ol>
      <p className="ob-hint">
        {ocrCombo ? (
          <Trans k="onboarding.try.more" nodes={{ combo: <Kbd combo={ocrCombo} /> }} />
        ) : (
          t('onboarding.try.more_nokey')
        )}
      </p>
      <AdviceBanners />
    </StepLayout>
  )
}

function TaskRow({
  index,
  done,
  task,
  status,
  onTry,
  testId,
}: {
  index: number
  done: boolean
  task: Task
  status: HotkeyStatus | undefined
  onTry: () => void
  testId: string
}) {
  const t = useT()
  const tKey = useTKey()
  const pulse = usePreset('statusPulse')
  const expand = usePreset('sectionExpand')
  const combo = status?.effective ?? null
  const unavailable = status?.state === 'unavailable'
  const broken = !combo || status?.state === 'failed' || unavailable
  // 有可用的组合键：「选中上面的文字，按 ⌥D」；没有时换成不依赖组合键的完整句子
  const label = combo ? (
    <>
      {t(`onboarding.try.task.${task}`)}
      <Kbd combo={combo} />
    </>
  ) : (
    t(`onboarding.try.task_nokey.${task}`)
  )
  let hint: ReactNode = t(`onboarding.try.hint.${task}`)
  if (done) hint = t('onboarding.try.done')
  else if (unavailable)
    hint = status?.reason_key ? tKey(status.reason_key) : t('onboarding.try.unavailable')
  else if (status?.state === 'failed')
    hint = <Trans k="onboarding.try.broken" nodes={{ issue: <HotkeyIssue status={status} /> }} />
  else if (broken) hint = t('onboarding.try.unbound')
  return (
    <li className="ob-task" data-done={done || undefined} data-testid={testId}>
      <m.span key={done ? 'done' : 'todo'} className="ob-task-check" {...pulse} aria-hidden="true">
        {done ? <Icon icon={Check} size={14} /> : index}
      </m.span>
      <span className="ob-task-text">
        <span className="ob-task-label">{label}</span>
        <span className="ob-task-hint" data-tone={broken && !done ? 'warning' : undefined}>
          {hint}
        </span>
      </span>
      {!done ? (
        <Button
          size="sm"
          variant="ghost"
          icon={MousePointerClick}
          onClick={onTry}
          data-testid={`${testId}-try`}
        >
          {t('onboarding.try.try_now')}
        </Button>
      ) : null}
      <AnimatePresence initial={false}>
        {broken && status && !unavailable && !done ? (
          <m.div key="fix" className="st-clip ob-task-fix" {...expand}>
            <HotkeyRow
              target={status.target}
              label={t(`hotkeys.action.${task}`)}
              status={status}
              description={t('onboarding.try.fix')}
              variant="embedded"
            />
          </m.div>
        ) : null}
      </AnimatePresence>
    </li>
  )
}
