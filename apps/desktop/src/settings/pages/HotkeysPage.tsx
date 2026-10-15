/**
 * 设置 → 快捷键。
 *
 * - 5 个动作：录制器（组件库的 HotkeyRecorder：录制期间 Rust 注销全部全局键、三级冲突提示、「仍然使用」）、注册状态
 *   （已改用备选键 / 注册失败 + 原因与原来的组合）、「测试」（同一时刻只有一个目标在测试；10 s 内收到事件显示「已收到」，
 *   超时提示可能被占用并建议换键，测试期间不执行动作）、单项恢复默认；分组右上角「全部恢复默认」（就地二次确认）；
 * - 风格快捷键汇总（在「翻译 → 风格」里编辑）；
 * - 扩展触发：浮标、双击修饰键（键、间隔 200/300/400）、鼠标键（中键 / 后退 / 前进各自启用 + 动作，长按）；每项显示
 *   运行状态（待授权时在**那一项下面**给出授权入口），Wayland / 缺少 XInput 时整块置灰并写明原因，X11 上中键置灰；
 * - 邻居应用冲突、键盘布局建议与已知限制。
 */
import { CircleDashed, RotateCcw, TriangleAlert } from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useDeferredValue, useEffect } from 'react'
import { usePlatform } from '../../shared/hooks/usePlatform'
import { useT, useTKey } from '../../shared/i18n/useT'
import { api } from '../../shared/ipc'
import type {
  ExtTriggerState,
  HotkeyAction,
  HotkeyStatus,
  ModifierKey,
  MouseButtonKind,
  OsKind,
  PermissionKind,
} from '../../shared/ipc/types'
import { usePreset } from '../../shared/motion/presets'
import {
  Badge,
  Button,
  Checkbox,
  Field,
  Icon,
  Kbd,
  NumberField,
  Section,
  SegmentedControl,
  Select,
  Switch,
} from '../../shared/ui'
import { useSettingsNav } from '../context'
import { AdviceBanners } from '../components/AdviceBanners'
import { HeaderConfirm } from '../components/Confirm'
import { Disclosure } from '../components/Disclosure'
import { HotkeyRow, sameTarget, stateBadge } from '../components/HotkeyRow'
import { Page } from '../components/Page'
import { ipcErrorText } from '../lib/errors'
import { at } from '../lib/patch'
import { grantPermission } from '../lib/permissions'
import { styleName } from '../lib/styles'
import { notify, patchSettings, setHotkeys, useRuntime, useSettings, useStyles } from '../store'
import { relaunchSafely } from '../lib/writes'

const ACTIONS: readonly HotkeyAction[] = [
  'translate_selection',
  'translate_input',
  'ocr',
  'ocr_translate',
  'clipboard',
]

export default function HotkeysPage() {
  const t = useT()
  const tKey = useTKey()
  const runtime = useRuntime()
  const settings = useSettings()
  const rest = useDeferredValue(true, false)
  // 打开快捷键页时重新取一次状态：Rust 对「已改用备选键 / 注册失败」的项整体重试一次（占用首选键的程序可能已经退出）
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
  if (!runtime || !settings) return null
  const statuses = runtime.hotkeys
  const hooksCap = runtime.platform.capabilities.global_hotkeys

  const resetAll = async () => {
    try {
      setHotkeys(await api.hotkeyResetDefaults())
      notify(t('settings.hotkeys.reset_done'), 'success')
    } catch (error) {
      notify(ipcErrorText(error, 'hotkey_reset_defaults'), 'danger')
    }
  }

  return (
    <Page title={t('settings.nav.hotkeys')} description={t('settings.hotkeys.description')}>
      <AdviceBanners />
      <Section
        title={t('settings.hotkeys.global')}
        action={
          <HeaderConfirm
            action={t('settings.hotkeys.reset_all')}
            confirm={t('settings.hotkeys.reset_all_confirm')}
            confirmLabel={t('settings.hotkeys.reset_all_ok')}
            icon={RotateCcw}
            onConfirm={() => void resetAll()}
            data-testid="hotkeys-reset-all"
          />
        }
      >
        {!hooksCap.available ? (
          <div className="st-card-note" data-tone="warning">
            <Icon icon={TriangleAlert} size={14} />
            <span>
              {(hooksCap.reason_key && tKey(hooksCap.reason_key)) ||
                t('settings.hotkeys.unavailable')}
            </span>
          </div>
        ) : null}
        {ACTIONS.map((action) => (
          <HotkeyRow
            key={action}
            target={{ kind: 'action', action }}
            label={t(`hotkeys.action.${action}`)}
            status={statuses.find((s) => sameTarget(s.target, { kind: 'action', action }))}
            disabled={!hooksCap.available}
          />
        ))}
      </Section>

      {/* 首帧只排全局快捷键这一组（首屏看得见的部分）；其余分组在首帧之后的后台渲染里补上（useDeferredValue 的初值），
          第一次进入这一页时不再有一帧 37 ms 的长任务（侧栏选中背景的弹簧会在那一帧跳一截）。它们在首屏下方，
          补上时页面正在 pageSwitch 淡入，看不出先后 */}
      {rest ? (
        <>
          <StyleHotkeys statuses={statuses} />
          <ExtTriggers />
          <AdvancedHotkeys />
          <KnownLimits />
        </>
      ) : null}
    </Page>
  )
}

function StyleHotkeys({ statuses }: { statuses: readonly HotkeyStatus[] }) {
  const t = useT()
  const styles = useStyles()
  const nav = useSettingsNav()
  const bound = styles.filter((s) => s.hotkey)
  return (
    <Section
      title={t('settings.hotkeys.styles')}
      description={t('settings.hotkeys.styles_desc')}
      action={
        <Button size="sm" variant="ghost" onClick={() => nav.go('translate', 'styles')}>
          {t('settings.hotkeys.manage_styles')}
        </Button>
      }
      data-testid="hotkeys-styles"
    >
      {bound.length === 0 ? (
        <div className="st-card-empty">{t('settings.hotkeys.styles_empty')}</div>
      ) : (
        bound.map((s) => {
          const status = statuses.find((h) =>
            sameTarget(h.target, { kind: 'style', style_id: s.id }),
          )
          return (
            <div key={s.id} className="st-list-row">
              <span className="st-list-main">
                <span className="st-list-title">{styleName(s)}</span>
              </span>
              {stateBadge(status, t)}
              {s.hotkey ? <Kbd combo={s.hotkey} /> : null}
              <Button
                size="sm"
                variant="ghost"
                onClick={() => nav.go('translate', `style:${s.id}`)}
              >
                {t('common.action.edit')}
              </Button>
            </div>
          )
        })
      )}
    </Section>
  )
}

const TRIGGER_TONE: Record<ExtTriggerState, 'success' | 'warning' | 'neutral' | null> = {
  active: 'success',
  waiting_permission: 'warning',
  unavailable: 'neutral',
  off: null,
}

function TriggerBadge({ state }: { state: ExtTriggerState }) {
  const t = useT()
  const tone = TRIGGER_TONE[state]
  if (!tone) return null
  return (
    <Badge tone={tone} dot>
      {t(`settings.hotkeys.trigger_state.${state}`)}
    </Badge>
  )
}

const MODIFIER_KEYS: Record<'mac' | 'pc', readonly ModifierKey[]> = {
  mac: ['right_alt', 'left_alt', 'right_cmd', 'left_cmd', 'right_ctrl', 'left_ctrl'],
  pc: ['right_ctrl', 'left_ctrl', 'right_alt', 'left_alt'],
}

function defaultModifier(os: OsKind): ModifierKey {
  return os === 'macos' ? 'right_alt' : 'right_ctrl'
}

function ExtTriggers() {
  const t = useT()
  const tKey = useTKey()
  const os = usePlatform()
  const settings = useSettings()
  const runtime = useRuntime()
  const expand = usePreset('sectionExpand')
  if (!settings || !runtime) return null
  const h = settings.hotkeys
  const status = runtime.ext_triggers
  const cap = runtime.platform.capabilities.input_hooks
  const disabled = !cap.available
  const x11 = os === 'linux' && runtime.platform.session === 'x11'
  const perms = runtime.permissions

  /** 某个触发需要、但还没有的权限（macOS） */
  const missing = (kinds: readonly PermissionKind[]): PermissionKind | null =>
    kinds.find((k) => perms[k] !== 'granted' && perms[k] !== 'not_applicable') ?? null
  const needs: Record<'bobber' | 'double_tap' | 'mouse', PermissionKind | null> = {
    bobber: missing(['accessibility']),
    double_tap: missing(['input_monitoring']),
    mouse: missing(['accessibility']),
  }
  /** 某项在等授权时要申请的权限（Rust 说在等、但前端的权限快照已经齐了时，按该项需要的默认一项） */
  const waitingFor = (k: 'bobber' | 'double_tap' | 'mouse'): PermissionKind | null =>
    status[k] === 'waiting_permission'
      ? (needs[k] ?? (k === 'double_tap' ? 'input_monitoring' : 'accessibility'))
      : null
  /** 等授权的那一项下面：说明 + 授权按钮 + 重新启动（授权后部分系统要重启进程才认） */
  const permissionNote = (k: 'bobber' | 'double_tap' | 'mouse') => {
    const kind = waitingFor(k)
    return (
      <AnimatePresence initial={false}>
        {kind ? (
          <m.div key="perm" className="st-clip" {...expand}>
            <div
              className="st-row-note"
              data-tone="warning"
              data-testid={`hotkeys-ext-permission-${k}`}
            >
              <Icon icon={TriangleAlert} size={14} />
              <span className="st-grow">{t(`settings.permission.need.${kind}`)}</span>
              <Button size="sm" variant="primary" onClick={() => void grantPermission(kind, perms)}>
                {t(`settings.permission.grant.${kind}`)}
              </Button>
              <Button
                size="sm"
                variant="ghost"
                // 重启后回到快捷键页（输入监控授权后要重启才生效）
                onClick={() => void relaunchSafely({ kind: 'settings', section: 'hotkeys' })}
              >
                {t('settings.permission.relaunch')}
              </Button>
            </div>
          </m.div>
        ) : null}
      </AnimatePresence>
    )
  }

  const modOptions = MODIFIER_KEYS[os === 'macos' ? 'mac' : 'pc'].map((k) => ({
    value: k,
    label: t(`settings.hotkeys.modifier.${os === 'macos' ? 'mac' : 'pc'}.${k}`),
  }))
  const actions = ACTIONS.map((a) => ({ value: a, label: t(`hotkeys.action.${a}`) }))
  const busy = status.reason_key === 'errors.platform.x11_mouse_button_busy'
  /**
   * 某一项不可用时的平台原因（ExtTriggerStatus 只带第一个不正常项的 reason_key）：整块已置灰时不重复；
   * 中键的原因显示在中键那一行
   */
  const reasonFor = (k: 'bobber' | 'double_tap' | 'mouse'): string | null => {
    const key = status.reason_key
    if (disabled || status[k] !== 'unavailable' || !key) return null
    if (key === 'errors.platform.x11_middle_button') return null
    return tKey(key)
  }

  return (
    <Section
      title={t('settings.hotkeys.ext')}
      description={t('settings.hotkeys.ext_desc')}
      data-testid="hotkeys-ext"
    >
      {disabled ? (
        <div className="st-card-note" data-tone="neutral" data-testid="hotkeys-ext-unavailable">
          <Icon icon={CircleDashed} size={14} />
          <span>
            {(cap.reason_key && tKey(cap.reason_key)) || t('settings.hotkeys.ext_unavailable')}
          </span>
        </div>
      ) : null}
      <TriggerSwitch
        label={t('settings.hotkeys.bobber')}
        description={t('settings.hotkeys.bobber_desc')}
        checked={h.bobber.enabled}
        state={status.bobber}
        message={reasonFor('bobber')}
        disabled={disabled}
        onChange={(v) => void patchSettings(at('hotkeys.bobber.enabled', v))}
        testId="ext-bobber"
      />
      {permissionNote('bobber')}

      <TriggerSwitch
        label={t('settings.hotkeys.double_tap')}
        description={t('settings.hotkeys.double_tap_desc', {
          key: t(
            `settings.hotkeys.modifier.${os === 'macos' ? 'mac' : 'pc'}.${h.double_tap.key ?? defaultModifier(os)}`,
          ),
        })}
        checked={h.double_tap.enabled}
        state={status.double_tap}
        message={reasonFor('double_tap')}
        disabled={disabled}
        onChange={(v) => void patchSettings(at('hotkeys.double_tap.enabled', v))}
        testId="ext-double-tap"
      />
      {permissionNote('double_tap')}
      <AnimatePresence initial={false}>
        {h.double_tap.enabled ? (
          <m.div key="dt" className="st-clip st-subrows" {...expand}>
            <Field label={t('settings.hotkeys.double_tap_key')}>
              <Select<ModifierKey>
                label={t('settings.hotkeys.double_tap_key')}
                value={h.double_tap.key ?? defaultModifier(os)}
                options={modOptions}
                disabled={disabled}
                onChange={(v) => void patchSettings(at('hotkeys.double_tap.key', v))}
                className="st-select"
              />
            </Field>
            <Field
              label={t('settings.hotkeys.double_tap_interval')}
              description={t('settings.hotkeys.double_tap_interval_desc')}
            >
              <SegmentedControl<'200' | '300' | '400'>
                label={t('settings.hotkeys.double_tap_interval')}
                value={String(h.double_tap.interval_ms) as '200' | '300' | '400'}
                options={(['200', '300', '400'] as const).map((v) => ({
                  value: v,
                  label: t('common.unit.ms', { n: v }),
                }))}
                disabled={disabled}
                onChange={(v) =>
                  void patchSettings(at('hotkeys.double_tap.interval_ms', Number(v)))
                }
                className="st-segmented"
              />
            </Field>
          </m.div>
        ) : null}
      </AnimatePresence>

      <TriggerSwitch
        label={t('settings.hotkeys.mouse')}
        description={t('settings.hotkeys.mouse_desc')}
        checked={h.mouse.enabled}
        state={status.mouse}
        disabled={disabled}
        onChange={(v) => void patchSettings(at('hotkeys.mouse.enabled', v))}
        message={busy ? tKey('errors.platform.x11_mouse_button_busy') : reasonFor('mouse')}
        testId="ext-mouse"
      />
      {permissionNote('mouse')}
      <AnimatePresence initial={false}>
        {h.mouse.enabled ? (
          <m.div key="mouse" className="st-clip st-subrows" {...expand}>
            {(['middle', 'back', 'forward'] as const).map((button: MouseButtonKind) => {
              const b = h.mouse[button]
              const blocked = button === 'middle' && x11
              return (
                <Field
                  key={button}
                  label={
                    <Checkbox
                      checked={b.enabled && !blocked}
                      disabled={disabled || blocked}
                      label={t(`settings.hotkeys.mouse_button.${button}`)}
                      onChange={(v) => void patchSettings(at(`hotkeys.mouse.${button}.enabled`, v))}
                      data-testid={`ext-mouse-${button}`}
                    />
                  }
                  message={blocked ? tKey('errors.platform.x11_middle_button') : null}
                  messageTone="default"
                >
                  <Select<HotkeyAction>
                    label={t('settings.hotkeys.mouse_action', {
                      button: t(`settings.hotkeys.mouse_button.${button}`),
                    })}
                    value={b.action}
                    placeholder={t('settings.hotkeys.mouse_action_none')}
                    options={actions}
                    disabled={disabled || blocked || !b.enabled}
                    onChange={(v) => void patchSettings(at(`hotkeys.mouse.${button}.action`, v))}
                    className="st-select"
                  />
                </Field>
              )
            })}
            <Field
              label={t('settings.hotkeys.long_press')}
              description={t('settings.hotkeys.long_press_desc')}
            >
              <Switch
                checked={h.mouse.long_press}
                aria-label={t('settings.hotkeys.long_press')}
                disabled={disabled}
                onChange={(v) => void patchSettings(at('hotkeys.mouse.long_press', v))}
              />
            </Field>
          </m.div>
        ) : null}
      </AnimatePresence>
    </Section>
  )
}

function TriggerSwitch({
  label,
  description,
  checked,
  state,
  disabled,
  onChange,
  message,
  testId,
}: {
  label: string
  description: string
  checked: boolean
  state: ExtTriggerState
  disabled: boolean
  onChange: (v: boolean) => void
  message?: string | null
  testId: string
}) {
  return (
    <Field
      label={
        <span className="st-label-with-badge">
          {label}
          {checked ? <TriggerBadge state={state} /> : null}
        </span>
      }
      description={description}
      message={message}
      messageTone="warning"
      data-testid={testId}
    >
      <Switch
        checked={checked}
        onChange={onChange}
        disabled={disabled}
        aria-label={label}
        data-testid={`${testId}-switch`}
      />
    </Field>
  )
}

function AdvancedHotkeys() {
  const t = useT()
  const settings = useSettings()
  if (!settings) return null
  return (
    <Section>
      <Disclosure title={t('settings.hotkeys.advanced')} data-testid="hotkeys-advanced">
        <Field label={t('hotkeys.copy_wait_ms')} description={t('settings.hotkeys.copy_wait_desc')}>
          <NumberField
            value={settings.hotkeys.copy_wait_ms}
            min={50}
            max={500}
            step={10}
            unit={t('settings.unit.ms')}
            aria-label={t('hotkeys.copy_wait_ms')}
            className="st-number"
            onChange={(v) => void patchSettings(at('hotkeys.copy_wait_ms', v))}
          />
        </Field>
      </Disclosure>
    </Section>
  )
}

/** 已知限制（macOS 三条；其他平台只在 AltGr 布局时提示一条） */
function KnownLimits() {
  const t = useT()
  const os = usePlatform()
  if (os !== 'macos') return null
  return (
    <Section>
      <Disclosure title={t('settings.hotkeys.limits')} data-testid="hotkeys-limits">
        <ul className="st-bullets">
          <li>{t('settings.hotkeys.limit_option_chars')}</li>
          <li>{t('settings.hotkeys.limit_secure_input')}</li>
          <li>{t('settings.hotkeys.limit_option_meta')}</li>
        </ul>
      </Disclosure>
    </Section>
  )
}
