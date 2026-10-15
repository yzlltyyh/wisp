/**
 * 引导第 1 步：权限（只在 macOS 出现）。
 *
 * - 辅助功能（取词、粘贴）与屏幕录制（OCR）两张卡片：用途、当前状态、[前往系统设置]；授权后自动变绿
 *   （`permissions:changed`，引导页可见时 Rust 每 2 s 轮询一次，见 OnboardingApp 的 permissions_watch）；
 * - 屏幕录制授权后可能需要重新启动 Wisp 才能生效：点过「前往系统设置」后给出 [重新启动 Wisp]；
 * - 不包含输入监控（开启双击修饰键 / 浮标时在设置页按需申请）；
 * - 更新后授权失效：专门的说明——在系统设置里先移除 Wisp 再重新添加，或关闭再打开开关；
 * - 按钮层级：同一屏只有一个主按钮——第一张待授权卡片的按钮；其余卡片用次级按钮；两项都就绪前底栏只有「跳过这一步」
 *   （次级），就绪后交叉淡入成主按钮「下一步」（FooterSwap：同一位置、宽度按较宽的那个，不跳）；
 * - 状态徽标固定在标题右侧（未授权 / 已拒绝 / 已授权），授权前后位置不变；右侧只放按钮，授权后按钮原地淡出
 *   （占位保留、不能再点），卡片里的文字不会因此重排；
 * - 「重新启动 Wisp」调用 `app_relaunch(resume)`：Rust 把这一步写进 state.json，重启后经 `boot.route` 直接回到权限页
 *   （src-tauri/src/app/relaunch.rs），而不是从欢迎页重新点一遍。
 */
import {
  Accessibility,
  Check,
  RotateCw,
  ScreenShare,
  TriangleAlert,
  type LucideIcon,
} from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useState } from 'react'
import { useT } from '../../shared/i18n/useT'
import type { PermissionKind, PermissionState } from '../../shared/ipc/types'
import { usePreset } from '../../shared/motion/presets'
import { Badge, Banner, Button, Icon } from '../../shared/ui'
import { grantPermission } from '../../settings/lib/permissions'
import { useRuntime } from '../../settings/store'
import { relaunchSafely } from '../../settings/lib/writes'
import {
  FooterSwap,
  SkipStep,
  STEP_NUMBERS,
  StepFooter,
  StepLayout,
  type StepProps,
} from '../shared'

const CARDS: ReadonlyArray<{
  kind: Extract<PermissionKind, 'accessibility' | 'screen_recording'>
  icon: LucideIcon
}> = [
  { kind: 'accessibility', icon: Accessibility },
  { kind: 'screen_recording', icon: ScreenShare },
]

const granted = (s: PermissionState | undefined) => s === 'granted' || s === 'not_applicable'

export function Permissions({ onNext, onBack, onSkipAll, canBack }: StepProps) {
  const t = useT()
  const runtime = useRuntime()
  const perms = runtime?.permissions ?? null
  const [asked, setAsked] = useState<Set<PermissionKind>>(() => new Set())
  const allGranted = CARDS.every((c) => granted(perms?.[c.kind]))
  /** 第一张还没授权的卡片：只有它用主按钮 */
  const firstPending = CARDS.find((c) => !granted(perms?.[c.kind]))?.kind ?? null
  const rowInsert = usePreset('rowInsert')

  const ask = async (kind: PermissionKind) => {
    setAsked((s) => new Set(s).add(kind))
    await grantPermission(kind, perms)
  }

  return (
    <StepLayout
      title={t('onboarding.permissions.title')}
      description={t('onboarding.permissions.description')}
      data-testid="onboarding-permissions"
      footer={
        <StepFooter
          onSkipAll={onSkipAll}
          onBack={onBack}
          canBack={canBack}
          next={
            <FooterSwap
              active={allGranted ? 'b' : 'a'}
              a={<SkipStep onClick={onNext} />}
              b={
                <Button
                  variant="primary"
                  size="md"
                  onClick={onNext}
                  data-default={allGranted || undefined}
                  data-testid="onboarding-next-ready"
                >
                  {t('common.action.next')}
                </Button>
              }
            />
          }
        />
      }
    >
      <AnimatePresence initial={false}>
        {perms?.lost_after_update ? (
          <m.div key="lost" className="ob-banner" {...rowInsert}>
            <Banner
              tone="warning"
              icon={TriangleAlert}
              className="st-banner-top"
              data-testid="permissions-lost"
            >
              <span className="ob-banner-text">
                <b>{t('onboarding.permissions.lost_title')}</b>
                <span>{t('onboarding.permissions.lost_detail')}</span>
              </span>
            </Banner>
          </m.div>
        ) : null}
      </AnimatePresence>
      <div className="ob-perms">
        {CARDS.map((c) => (
          <PermissionCard
            key={c.kind}
            kind={c.kind}
            icon={c.icon}
            state={perms?.[c.kind]}
            primary={firstPending === c.kind}
            asked={asked.has(c.kind)}
            onGrant={() => void ask(c.kind)}
          />
        ))}
      </div>
      <AnimatePresence initial={false}>
        {allGranted ? (
          <m.p key="ready" className="ob-hint" data-tone="success" {...rowInsert}>
            <Icon icon={Check} size={14} />
            {t('onboarding.permissions.all_set')}
          </m.p>
        ) : (
          <m.p key="privacy" className="ob-hint" {...rowInsert}>
            {t('onboarding.permissions.privacy')}
          </m.p>
        )}
      </AnimatePresence>
    </StepLayout>
  )
}

function PermissionCard({
  kind,
  icon,
  state,
  primary,
  asked,
  onGrant,
}: {
  kind: 'accessibility' | 'screen_recording'
  icon: LucideIcon
  state: PermissionState | undefined
  /** 这一屏唯一的主按钮 */
  primary: boolean
  asked: boolean
  onGrant: () => void
}) {
  const t = useT()
  const pulse = usePreset('statusPulse')
  const fade = usePreset('footerIn')
  const ok = granted(state)
  // 屏幕录制：系统往往要重启进程才认新授权
  const needsRelaunch = kind === 'screen_recording' && asked && !ok
  return (
    <div className="ob-perm" data-granted={ok || undefined} data-testid={`permission-${kind}`}>
      <m.span key={ok ? 'ok' : 'no'} className="ob-perm-icon" {...pulse}>
        <Icon icon={icon} size={18} />
      </m.span>
      <span className="ob-perm-text">
        <span className="ob-perm-title">
          <b>{t(`onboarding.permissions.${kind}.title`)}</b>
          {/* 状态徽标固定在标题右侧：授权前后位置不变 */}
          {ok ? (
            <m.span key="granted" className="ob-perm-badge" {...pulse}>
              <Badge tone="success" data-testid={`permission-${kind}-granted`}>
                <Icon icon={Check} size={12} />
                {t('onboarding.permissions.granted')}
              </Badge>
            </m.span>
          ) : state === 'denied' ? (
            <Badge tone="warning" dot data-testid={`permission-${kind}-denied`}>
              {t('onboarding.permissions.denied')}
            </Badge>
          ) : (
            <Badge tone="neutral" dot>
              {t('onboarding.permissions.not_granted')}
            </Badge>
          )}
        </span>
        <span>{t(`onboarding.permissions.${kind}.why`)}</span>
        {needsRelaunch ? (
          <m.span className="ob-perm-note" {...fade}>
            {t('onboarding.permissions.relaunch_hint')}
          </m.span>
        ) : null}
      </span>
      {/* 右侧只放按钮；授权后原地淡出（保留占位，文字不重排），不能再点、读屏跳过 */}
      <span className="ob-perm-actions" data-hidden={ok || undefined} inert={ok}>
        {needsRelaunch ? (
          <>
            <Button size="sm" variant="ghost" onClick={onGrant}>
              {t('onboarding.permissions.open_again')}
            </Button>
            <Button
              size="sm"
              variant={primary ? 'primary' : 'secondary'}
              icon={RotateCw}
              onClick={() => {
                // 重启后回到这一步（屏幕录制要重启才生效，不必从欢迎页重新点一遍）：Rust 把目标写进 state.json，下次启动随
                // Boot.route 带回
                void relaunchSafely({ kind: 'onboarding', step: STEP_NUMBERS.permissions })
              }}
              data-testid="permission-relaunch"
            >
              {t('settings.permission.relaunch')}
            </Button>
          </>
        ) : (
          <Button
            size="sm"
            variant={primary && !ok ? 'primary' : 'secondary'}
            onClick={onGrant}
            data-testid={`permission-${kind}-grant`}
          >
            {t('common.action.open_system_settings')}
          </Button>
        )}
      </span>
    </div>
  )
}
