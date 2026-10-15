/**
 * 欢迎页：应用图标、一句话介绍、三大场景与它们实际生效的快捷键（按平台显示：macOS ⌥D，Windows Ctrl+Alt+D）。
 * 「开始设置」进入第一步（默认按钮，Return 触发）；「稍后设置」直接完成。
 */
import { ClipboardList, Languages, ScanText, type LucideIcon } from 'lucide-react'
import { m } from 'motion/react'
import { keepQuoted } from '../../shared/i18n/keepQuoted'
import { useT } from '../../shared/i18n/useT'
import type { HotkeyAction } from '../../shared/ipc/types'
import { usePreset } from '../../shared/motion/presets'
import { Button, Icon, Kbd } from '../../shared/ui'
import { useRuntime } from '../../settings/store'
import appIcon from '../../../src-tauri/icons/src/app-icon.svg'
import { LaterButton, StepFooterPortal, type StepProps } from '../shared'

const FEATURES: ReadonlyArray<{ action: HotkeyAction; icon: LucideIcon }> = [
  { action: 'translate_selection', icon: Languages },
  { action: 'ocr', icon: ScanText },
  { action: 'clipboard', icon: ClipboardList },
]

export function Welcome({ onNext, onSkipAll }: StepProps) {
  const t = useT()
  const runtime = useRuntime()
  const cardIn = usePreset('cardIn')
  const combo = (action: HotkeyAction) =>
    runtime?.hotkeys.find((h) => h.target.kind === 'action' && h.target.action === action)
      ?.effective ?? null
  return (
    <section className="ob-step ob-welcome" data-testid="onboarding-welcome">
      <div className="ob-step-body ob-welcome-body">
        <m.img
          {...cardIn}
          className="ob-welcome-icon"
          src={appIcon}
          alt=""
          width={72}
          height={72}
        />
        <h1 className="ob-welcome-title" tabIndex={-1} data-step-title>
          {t('onboarding.welcome.title')}
        </h1>
        <p className="ob-welcome-desc">{keepQuoted(t('onboarding.welcome.description'))}</p>
        <ul className="ob-features">
          {FEATURES.map((f) => {
            const c = combo(f.action)
            return (
              <li key={f.action} className="ob-feature">
                <span className="ob-feature-icon">
                  <Icon icon={f.icon} size={16} />
                </span>
                <span className="ob-feature-text">
                  {/* 功能名与快捷键页、托盘一致（hotkeys.action.*） */}
                  <b>{t(`hotkeys.action.${f.action}`)}</b>
                  <span>
                    {t(
                      `onboarding.welcome.feature_desc.${f.action}` as 'onboarding.welcome.feature_desc.ocr',
                    )}
                  </span>
                </span>
                {c ? <Kbd combo={c} /> : null}
              </li>
            )
          })}
        </ul>
      </div>
      <StepFooterPortal>
        <LaterButton onClick={onSkipAll} />
        <span className="ob-spacer" />
        {/* 默认按钮：焦点不在控件上时按 Return 触发（OnboardingApp） */}
        <Button
          variant="primary"
          size="md"
          onClick={onNext}
          data-default
          data-testid="onboarding-start"
        >
          {t('onboarding.welcome.start')}
        </Button>
      </StepFooterPortal>
    </section>
  )
}
