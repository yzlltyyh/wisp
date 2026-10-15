/**
 * 设置 → 通用：开机自启（没能写进系统时在开关下方说明原因，RuntimeInfo.autostart_issue）、静默启动、界面语言（跟随系统 / 简体中文 / English）、更新策略；
 * 高级：开发者模式、日志级别、隐藏时降低 WebView 内存、空闲卸载面板。改动即时生效。
 */
import { usePlatform } from '../../shared/hooks/usePlatform'
import { useT, useTKey } from '../../shared/i18n/useT'
import type { LogLevel, UiLanguage, UpdatePolicy } from '../../shared/ipc/types'
import { Field, NumberField, Section, Switch } from '../../shared/ui'
import { Disclosure } from '../components/Disclosure'
import { Page, SelectRow, SwitchRow } from '../components/Page'
import { at } from '../lib/patch'
import { patchSettings, useRuntime, useSettings } from '../store'

/**
 * advanced.unload_panel_after_idle_min 的范围（wisp_types::defaults::ranges::UNLOAD_PANEL_AFTER_IDLE_MIN）与开关打开时的初值
 * （`defaults::UNLOAD_PANEL_SUGGESTED_MIN`）
 */
const UNLOAD_RANGE = { min: 5, max: 1440, initial: 10 } as const

export default function GeneralPage() {
  const t = useT()
  const tKey = useTKey()
  const os = usePlatform()
  const settings = useSettings()
  const runtime = useRuntime()
  if (!settings) return null
  const g = settings.general
  const adv = settings.advanced

  return (
    <Page title={t('settings.nav.general')} description={t('settings.general.description')}>
      <Section title={t('settings.general.startup')}>
        <SwitchRow
          label={t('settings.general.launch_at_login')}
          description={t(`settings.general.launch_at_login_desc.${os}`)}
          checked={g.launch_at_login}
          // 开关开着、却没能写进系统（从 App Translocation 运行 / 写入失败）：开关下方说明原因与处理办法，
          // 不让用户以为已经开机自启
          message={
            g.launch_at_login && runtime?.autostart_issue
              ? tKey(
                  runtime.autostart_issue,
                  undefined,
                  t('settings.general.autostart_issue.failed'),
                )
              : null
          }
          messageTone="warning"
          onChange={(v) => void patchSettings(at('general.launch_at_login', v))}
          ariaLabel={t('settings.general.launch_at_login')}
          data-testid="general-launch"
        />
        <SwitchRow
          label={t('settings.general.start_hidden')}
          description={t('settings.general.start_hidden_desc')}
          checked={g.start_hidden}
          onChange={(v) => void patchSettings(at('general.start_hidden', v))}
          ariaLabel={t('settings.general.start_hidden')}
          data-testid="general-start-hidden"
        />
      </Section>

      <Section title={t('settings.general.language_section')}>
        <SelectRow<UiLanguage>
          label={t('settings.general.ui_language')}
          description={t('settings.general.ui_language_desc')}
          value={g.ui_language}
          options={[
            { value: 'system', label: t('settings.general.lang_system') },
            // i18n-ignore：语言名以各自的语言显示（选择器惯例）
            { value: 'zh-CN', label: '简体中文' },
            // i18n-ignore：语言名以各自的语言显示（选择器惯例）
            { value: 'en-US', label: 'English' },
          ]}
          onChange={(v) => void patchSettings(at('general.ui_language', v))}
          ariaLabel={t('settings.general.ui_language')}
          data-testid="general-language"
        />
      </Section>

      <Section title={t('settings.general.updates')}>
        <SelectRow<UpdatePolicy>
          label={t('settings.general.update_policy')}
          description={t(`settings.general.update_policy_desc.${g.update_policy}`)}
          value={g.update_policy}
          options={[
            { value: 'notify', label: t('settings.general.update_notify') },
            { value: 'auto_install', label: t('settings.general.update_auto') },
            { value: 'off', label: t('settings.general.update_off') },
          ]}
          onChange={(v) => void patchSettings(at('general.update_policy', v))}
          ariaLabel={t('settings.general.update_policy')}
          data-testid="general-update"
        />
      </Section>

      <Section>
        <Disclosure title={t('settings.general.advanced')} data-testid="general-advanced">
          <SwitchRow
            label={t('settings.general.developer_mode')}
            description={t('settings.general.developer_mode_desc')}
            checked={g.developer_mode}
            onChange={(v) => void patchSettings(at('general.developer_mode', v))}
            ariaLabel={t('settings.general.developer_mode')}
            data-testid="general-dev"
          />
          <SelectRow<LogLevel>
            label={t('settings.general.log_level')}
            description={t('settings.general.log_level_desc')}
            value={adv.log_level}
            options={(['error', 'warn', 'info', 'debug'] as const).map((v) => ({
              value: v,
              label: t(`settings.general.log.${v}`),
            }))}
            onChange={(v) => void patchSettings(at('advanced.log_level', v))}
            ariaLabel={t('settings.general.log_level')}
          />
          <SwitchRow
            label={t('settings.general.low_memory')}
            description={t('settings.general.low_memory_desc')}
            checked={adv.webview_low_memory_when_hidden}
            onChange={(v) => void patchSettings(at('advanced.webview_low_memory_when_hidden', v))}
            ariaLabel={t('settings.general.low_memory')}
          />
          <Field
            label={t('settings.general.unload_panel')}
            description={t('settings.general.unload_panel_desc')}
          >
            {adv.unload_panel_after_idle_min !== null ? (
              <NumberField
                value={adv.unload_panel_after_idle_min}
                min={UNLOAD_RANGE.min}
                max={UNLOAD_RANGE.max}
                step={5}
                unit={t('settings.unit.minutes')}
                aria-label={t('settings.general.unload_panel')}
                className="st-number"
                onChange={(v) => void patchSettings(at('advanced.unload_panel_after_idle_min', v))}
              />
            ) : null}
            <Switch
              checked={adv.unload_panel_after_idle_min !== null}
              aria-label={t('settings.general.unload_panel')}
              onChange={(v) =>
                void patchSettings(
                  at('advanced.unload_panel_after_idle_min', v ? UNLOAD_RANGE.initial : null),
                )
              }
            />
          </Field>
        </Disclosure>
      </Section>
    </Page>
  )
}
