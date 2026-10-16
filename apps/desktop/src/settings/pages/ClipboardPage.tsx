/**
 * 设置 → 剪贴板：监听开关与暂停、记录类型、保留（条数不含收藏 / 天数 / 单条上限）、隐私（疑似密钥跳过、托盘提示）、
 * 排除应用（从最近复制过的应用里选，或手动填应用标识）、智能识别（哪些片段显示为芯片、验证码到时自动删除）、面板（位置、
 * 单击 / 双击粘贴、提示条）、应用内粘贴键映射（终端等）、清空历史（保留收藏，二次确认）。
 */
import { AppWindow, Plus, Trash2, X } from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from 'react'
import { usePlatform } from '../../shared/hooks/usePlatform'
import { useT, useTKey } from '../../shared/i18n/useT'
import { api, resolveAssetUrl } from '../../shared/ipc'
import type {
  AppKeymap,
  ClipPanelPosition,
  ExcludedApp,
  OsKind,
  PasteOn,
  SmartSettings,
  SourceApp,
} from '../../shared/ipc/types'
import { presets, usePreset } from '../../shared/motion/presets'
import {
  AppIcon,
  AutoHeight,
  Button,
  Checkbox,
  Field,
  Icon,
  IconButton,
  Kbd,
  NumberField,
  Popover,
  SearchField,
  Section,
  TextField,
} from '../../shared/ui'
import { formatHotkeyRange } from '../../shared/keys/formatHotkey'
import { plainNthCombo } from '../../views/clipboard/keys'
import { DangerRow } from '../components/Confirm'
import { KeyCapture } from '../components/KeyCapture'
import { Page, SegmentRow, SelectRow, SwitchRow } from '../components/Page'
import { ipcErrorText } from '../lib/errors'
import { at } from '../lib/patch'
import { notify, patchSettings, useRuntime, useSettings, writeSettings } from '../store'

const KB = 1024
const MB = KB * KB
const TEXT_LIMITS = [256 * KB, MB, 4 * MB, 16 * MB] as const
const IMAGE_LIMITS = [5 * MB, 20 * MB, 50 * MB, 100 * MB] as const
const MAX_ITEMS = { min: 10, max: 100_000, step: 100 } as const
/** 从最近多少条剪贴板记录里收集来源应用 */
const RECENT_SCAN = 200

export default function ClipboardPage() {
  const t = useT()
  const tKey = useTKey()
  const settings = useSettings()
  const runtime = useRuntime()
  if (!settings || !runtime) return null
  const c = settings.clipboard
  const paused = runtime.clipboard_paused
  const watchCap = runtime.platform.capabilities.clipboard_watch

  const setPaused = async (v: boolean) => {
    try {
      await api.clipboardSetPaused(v)
    } catch (error) {
      notify(ipcErrorText(error, 'clipboard_set_paused'), 'danger')
    }
  }
  const clear = async () => {
    try {
      const n = await api.clipboardClear(true)
      notify(t('settings.clipboard.cleared', { count: n, n }), 'success')
    } catch (error) {
      notify(ipcErrorText(error, 'clipboard_clear'), 'danger')
    }
  }
  const mb = (bytes: number) => t('settings.unit.mb', { n: bytes / MB })
  const sizeLabel = (bytes: number) =>
    bytes < MB ? t('settings.unit.kb', { n: bytes / KB }) : mb(bytes)
  const withCurrent = (list: readonly number[], v: number) =>
    list.includes(v) ? list : [...list, v].sort((a, b) => a - b)

  return (
    <Page title={t('settings.nav.clipboard')} description={t('settings.clipboard.description')}>
      <Section title={t('settings.clipboard.history')}>
        <SwitchRow
          label={t('settings.clipboard.enabled')}
          description={
            watchCap.available
              ? t('settings.clipboard.enabled_desc')
              : tKey(
                  watchCap.reason_key ?? 'errors.ipc.platform',
                  undefined,
                  t('errors.ipc.platform'),
                )
          }
          checked={c.enabled}
          disabled={!watchCap.available}
          onChange={(v) => void patchSettings(at('clipboard.enabled', v))}
          ariaLabel={t('settings.clipboard.enabled')}
          data-testid="clip-enabled"
        />
        {c.enabled ? (
          <SwitchRow
            label={t('settings.clipboard.paused')}
            description={t('settings.clipboard.paused_desc')}
            checked={paused}
            onChange={(v) => void setPaused(v)}
            ariaLabel={t('settings.clipboard.paused')}
            data-testid="clip-paused"
          />
        ) : null}
        <Field
          label={t('settings.clipboard.types')}
          description={t('settings.clipboard.types_desc')}
          stacked
        >
          <div className="st-checks">
            {(
              [
                ['record_text', 'text'],
                ['record_rich_text', 'rich_text'],
                ['record_images', 'images'],
                ['record_files', 'files'],
              ] as const
            ).map(([key, label]) => (
              <Checkbox
                key={key}
                checked={c[key]}
                disabled={!c.enabled}
                label={t(`settings.clipboard.type.${label}`)}
                onChange={(v) => void patchSettings(at(`clipboard.${key}`, v))}
                data-testid={`clip-${key}`}
              />
            ))}
          </div>
        </Field>
      </Section>

      <Section
        title={t('settings.clipboard.retention')}
        description={t('settings.clipboard.retention_desc')}
      >
        <Field
          label={t('settings.clipboard.max_items')}
          description={t('settings.clipboard.max_items_desc')}
        >
          <NumberField
            value={c.max_items}
            min={MAX_ITEMS.min}
            max={MAX_ITEMS.max}
            step={MAX_ITEMS.step}
            unit={t('settings.unit.items')}
            aria-label={t('settings.clipboard.max_items')}
            className="st-number st-number-wide"
            onChange={(v) => void patchSettings(at('clipboard.max_items', v))}
            data-testid="clip-max-items"
          />
        </Field>
        <SelectRow<string>
          label={t('settings.clipboard.max_age')}
          value={c.max_age_days === null ? 'forever' : String(c.max_age_days)}
          options={[
            { value: 'forever', label: t('settings.clipboard.forever') },
            ...[7, 30, 90].map((d) => ({
              value: String(d),
              label: t('settings.unit.days', { n: d }),
            })),
          ]}
          onChange={(v) =>
            void patchSettings(at('clipboard.max_age_days', v === 'forever' ? null : Number(v)))
          }
          ariaLabel={t('settings.clipboard.max_age')}
          data-testid="clip-max-age"
        />
        <SelectRow<string>
          label={t('settings.clipboard.max_text')}
          value={String(c.max_text_bytes)}
          options={withCurrent(TEXT_LIMITS, c.max_text_bytes).map((b) => ({
            value: String(b),
            label: sizeLabel(b),
          }))}
          onChange={(v) => void patchSettings(at('clipboard.max_text_bytes', Number(v)))}
          ariaLabel={t('settings.clipboard.max_text')}
        />
        <SelectRow<string>
          label={t('settings.clipboard.max_image')}
          value={String(c.max_image_bytes)}
          options={withCurrent(IMAGE_LIMITS, c.max_image_bytes).map((b) => ({
            value: String(b),
            label: sizeLabel(b),
          }))}
          onChange={(v) => void patchSettings(at('clipboard.max_image_bytes', Number(v)))}
          ariaLabel={t('settings.clipboard.max_image')}
        />
      </Section>

      <Section title={t('settings.clipboard.privacy')}>
        <SwitchRow
          label={t('settings.clipboard.secret')}
          description={t('settings.clipboard.secret_desc')}
          checked={c.secret_detection}
          onChange={(v) => void patchSettings(at('clipboard.secret_detection', v))}
          ariaLabel={t('settings.clipboard.secret')}
          data-testid="clip-secret"
        />
        <SwitchRow
          label={t('settings.clipboard.secret_flash')}
          description={t('settings.clipboard.secret_flash_desc')}
          checked={c.secret_tray_flash}
          disabled={!c.secret_detection}
          onChange={(v) => void patchSettings(at('clipboard.secret_tray_flash', v))}
          ariaLabel={t('settings.clipboard.secret_flash')}
        />
        <ExcludedApps apps={c.excluded_apps} />
      </Section>

      <SmartSection smart={c.smart} />

      <Section title={t('settings.clipboard.panel')}>
        <SegmentRow<ClipPanelPosition>
          label={t('settings.clipboard.position')}
          value={c.panel_position}
          options={(['center', 'cursor', 'remember'] as const).map((v) => ({
            value: v,
            label: t(`settings.clipboard.positions.${v}`),
          }))}
          onChange={(v) => void patchSettings(at('clipboard.panel_position', v))}
          ariaLabel={t('settings.clipboard.position')}
        />
        <SegmentRow<PasteOn>
          label={t('settings.clipboard.paste_on')}
          description={t('settings.clipboard.paste_on_desc')}
          value={c.paste_on}
          options={[
            { value: 'single_click', label: t('settings.clipboard.single_click') },
            { value: 'double_click', label: t('settings.clipboard.double_click') },
          ]}
          onChange={(v) => void patchSettings(at('clipboard.paste_on', v))}
          ariaLabel={t('settings.clipboard.paste_on')}
        />
        <QuickPasteRow />
        <SwitchRow
          label={t('settings.clipboard.hint_bar')}
          description={t('settings.clipboard.hint_bar_desc')}
          checked={c.show_hint_bar}
          onChange={(v) => void patchSettings(at('clipboard.show_hint_bar', v))}
          ariaLabel={t('settings.clipboard.hint_bar')}
        />
        <Keymaps keymaps={settings.hotkeys.app_keymaps} />
      </Section>

      <Section>
        <DangerRow
          label={t('settings.clipboard.clear')}
          description={t('settings.clipboard.clear_desc')}
          action={t('settings.clipboard.clear_button')}
          confirm={t(
            // 同步剪贴板历史时，清空会作为一次改动同步到其他设备
            settings.sync.enabled && settings.sync.content.clipboard
              ? 'settings.clipboard.clear_confirm_synced'
              : 'settings.clipboard.clear_confirm',
          )}
          confirmLabel={t('settings.clipboard.clear_ok')}
          icon={Trash2}
          onConfirm={() => void clear()}
          data-testid="clip-clear"
        />
      </Section>
    </Page>
  )
}

/**
 * 智能识别：四类片段各一个开关（证件与银行卡号是敏感信息，默认关闭，说明里写明），以及验证码到时自动删除。识别本身
 * 在 Rust 里按需进行，这里只改设置（随设置同步）
 */
function SmartSection({ smart }: { smart: SmartSettings }) {
  const t = useT()
  const rows = [
    ['contacts', 'smart_contacts'],
    ['codes', 'smart_codes'],
    ['numbers', 'smart_numbers'],
    ['identity', 'smart_identity'],
    ['otp_auto_delete', 'otp_auto_delete'],
  ] as const
  return (
    <Section title={t('settings.clipboard.smart')} description={t('settings.clipboard.smart_desc')}>
      {rows.map(([key, label]) => (
        <SwitchRow
          key={key}
          label={t(`settings.clipboard.${label}`)}
          description={t(`settings.clipboard.${label}_desc`)}
          checked={smart[key]}
          onChange={(v) => void patchSettings(at(`clipboard.smart.${key}`, v))}
          ariaLabel={t(`settings.clipboard.${label}`)}
          data-testid={`clip-smart-${key}`}
        />
      ))}
    </Section>
  )
}

/**
 * 面板里的快速粘贴键（说明，不可改）：与剪贴板视图的键位表同一来源——macOS 的纯文本是 ⌥⌘1–9（⌘⇧3–5 是系统截屏键），
 * Windows / Linux 是 Ctrl+Shift+1–9（views/clipboard/keys.ts 的 `plainNthCombo`）
 */
function QuickPasteRow() {
  const t = useT()
  const os = usePlatform()
  return (
    <Field
      label={t('settings.clipboard.quick_paste')}
      description={t('settings.clipboard.quick_paste_desc', {
        paste: formatHotkeyRange('Mod+Digit1', 'Mod+Digit9', os),
        plain: formatHotkeyRange(plainNthCombo(os, 1), plainNthCombo(os, 9), os),
      })}
      data-testid="clip-quick-paste"
    >
      <span className="st-keymap-pair">
        <Kbd combo="Mod+Digit1" />
        <Kbd combo={plainNthCombo(os, 1)} />
      </span>
    </Field>
  )
}

/** 应用选择器的一组候选：最近复制过的在前、正在运行的在后 */
export interface AppGroups {
  recent: SourceApp[]
  running: SourceApp[]
}

/**
 * 合并最近来源与正在运行的应用（纯函数，单测覆盖）：按 id 去重（不区分大小写，最近来源优先）、去掉已经排除的、
 * 按关键字过滤（名称或 id 包含，不区分大小写）
 */
export function mergeAppCandidates(
  recent: readonly SourceApp[],
  running: readonly SourceApp[] | null,
  excluded: readonly string[],
  query = '',
): AppGroups {
  const seen = new Set(excluded.map((id) => id.toLowerCase()))
  const needle = query.trim().toLocaleLowerCase()
  const keep = (a: SourceApp) => {
    const key = a.id.toLowerCase()
    if (seen.has(key)) return false
    seen.add(key)
    return (
      needle === '' ||
      a.name.toLocaleLowerCase().includes(needle) ||
      a.id.toLocaleLowerCase().includes(needle)
    )
  }
  const r = recent.filter(keep)
  return { recent: r, running: (running ?? []).filter(keep) }
}

/** 排除名单与按键映射里的一项：按（系统，应用 id）区分 */
interface AppEntry {
  os: OsKind
  app_id: string
}

/** 按（系统，应用 id）写入一项：已有的原位替换，没有就追加（与 Rust 的 `*_upsert` 同一规则） */
function upsertApp<T extends AppEntry>(list: readonly T[], next: T): T[] {
  const same = (x: T) => x.os === next.os && x.app_id === next.app_id
  return list.some(same) ? list.map((x) => (same(x) ? next : x)) : [...list, next]
}

function withoutApp<T extends AppEntry>(list: readonly T[], os: OsKind, appId: string): T[] {
  return list.filter((x) => !(x.os === os && x.app_id === appId))
}

/*
 * 排除应用与按键映射的写入：按（系统，应用 id）只写这一项，不把整个数组发回去——设置窗口里的旧数组会把别处刚加进来的
 * 应用（例如另一台设备排除的密码管理器）当作删除写回去，那个应用里复制的内容就又开始被记录。本地乐观地做同样的改动，
 * 新到的快照上也会重放它
 */
function saveExcludedApp(entry: ExcludedApp) {
  return writeSettings(
    (s) => ({
      ...s,
      clipboard: { ...s.clipboard, excluded_apps: upsertApp(s.clipboard.excluded_apps, entry) },
    }),
    () => api.excludedAppUpsert(entry),
    { command: 'excluded_app_upsert' },
  )
}

function removeExcludedApp(os: OsKind, appId: string) {
  return writeSettings(
    (s) => ({
      ...s,
      clipboard: {
        ...s.clipboard,
        excluded_apps: withoutApp(s.clipboard.excluded_apps, os, appId),
      },
    }),
    () => api.excludedAppRemove(os, appId),
    { command: 'excluded_app_remove' },
  )
}

function saveKeymap(entry: AppKeymap) {
  return writeSettings(
    (s) => ({
      ...s,
      hotkeys: { ...s.hotkeys, app_keymaps: upsertApp(s.hotkeys.app_keymaps, entry) },
    }),
    () => api.appKeymapUpsert(entry),
    { command: 'app_keymap_upsert' },
  )
}

function removeKeymap(os: OsKind, appId: string) {
  return writeSettings(
    (s) => ({
      ...s,
      hotkeys: { ...s.hotkeys, app_keymaps: withoutApp(s.hotkeys.app_keymaps, os, appId) },
    }),
    () => api.appKeymapRemove(os, appId),
    { command: 'app_keymap_remove' },
  )
}

/** 「正在运行」读取中的占位行数：没有上一次的结果时按这么多行占位 */
const RUNNING_PLACEHOLDER = 3
/** 占位行数的上限（列表本身最多显示约 8 行，再多也看不见） */
const RUNNING_PLACEHOLDER_MAX = 6

/**
 * 排除应用：只显示当前平台的条目；按项写入（`excluded_app_upsert` / `excluded_app_remove`），其他平台的条目与别处刚加进来的
 * 应用都不受影响。
 *
 * 应用选择器：
 * - 数据：最近来源整个窗口只读一次（第一次打开时）；正在运行的应用每次打开读一次（`apps_running`，只依赖 picker，不会
 *   因为最近来源读回而重读——Rust 会为没缓存图标的应用起线程取图标）；
 * - 布局不跳：过滤框一直在顶部（打开时就在，不因候选数晚到而插进来把内容往下推）；「正在运行」一组在读取期间是
 *   占位行（行数取上一次的结果），数据到达后占位与真实行交叉淡入、列表高度用 spring.gentle 过渡（AutoHeight 的
 *   swapKey：只在读取状态变化时过渡，输入过滤时高度同步跟随）；第一次打开时直接是内容的高度（不从 0 长出来）；
 * - 键盘全流程：打开时焦点在过滤框（定位完成之后，Popover 的 initialFocus），可直接输入过滤；↓ 进入选项、↑↓ 在选项间
 *   移动、第一个选项上 ↑ 回到过滤框；Enter（过滤框里）选中第一个候选；选中 / Esc / 点外面关闭后焦点回到「添加」按钮。
 */
function ExcludedApps({ apps }: { apps: readonly ExcludedApp[] }) {
  const t = useT()
  const os = usePlatform()
  const [picker, setPicker] = useState(false)
  const [manual, setManual] = useState(false)
  const [id, setId] = useState('')
  const [name, setName] = useState('')
  const [query, setQuery] = useState('')
  const [recent, setRecent] = useState<SourceApp[] | null>(null)
  /** 正在运行的应用：null = 读取中；读取失败时退回只列最近来源（空数组） */
  const [running, setRunning] = useState<SourceApp[] | null>(null)
  /** 上一次读到的正在运行的应用数（下一次读取期间的占位行数） */
  const [lastRunning, setLastRunning] = useState<number | null>(null)
  const recentAsked = useRef(false)
  const anchor = useRef<HTMLButtonElement>(null)
  const list = useRef<HTMLDivElement>(null)
  const filter = useRef<HTMLInputElement>(null)
  const expand = usePreset('sectionExpand')
  const swap = usePreset('footerIn')
  // 占位行淡出（menuOut：100 ms，ease.exit），与真实行的淡入交叉
  const fadeOut = presets.menuOut
  const mine = apps.filter((a) => a.os === os)

  // 最近来源：第一次打开时读一次
  useEffect(() => {
    if (!picker || recentAsked.current) return
    recentAsked.current = true
    void api
      .clipboardRecent(0, RECENT_SCAN, 'all')
      .then((page) => {
        const seen = new Map<string, SourceApp>()
        for (const hit of page.items) {
          const src = hit.item.source
          if (src && !seen.has(src.id)) seen.set(src.id, src)
        }
        setRecent([...seen.values()])
      })
      .catch(() => setRecent([]))
  }, [picker])

  // 正在运行的应用：每次打开读一次（随时在变）
  useEffect(() => {
    if (!picker) return
    let alive = true
    void api.appsRunning().then(
      (found) => {
        if (!alive) return
        setLastRunning(found.length)
        setRunning(found)
      },
      () => alive && setRunning([]),
    )
    return () => {
      alive = false
    }
  }, [picker])

  const open = () => {
    setQuery('')
    setRunning(null)
    setPicker(true)
  }
  const add = (appId: string, appName: string) => {
    const trimmed = appId.trim()
    if (!trimmed || mine.some((a) => a.app_id === trimmed)) return
    void saveExcludedApp({ os, app_id: trimmed, name: appName.trim() || trimmed })
  }
  const groups = mergeAppCandidates(
    recent ?? [],
    running,
    mine.map((a) => a.app_id),
    query,
  )
  const loadingRunning = running === null
  const empty =
    recent !== null && !loadingRunning && groups.recent.length === 0 && groups.running.length === 0
  const placeholders = Math.min(
    RUNNING_PLACEHOLDER_MAX,
    Math.max(1, lastRunning ?? RUNNING_PLACEHOLDER),
  )
  const pick = (s: SourceApp) => {
    add(s.id, s.name)
    setPicker(false)
  }

  /** ↑ / ↓ 在过滤框与选项之间移动焦点（Tab 照常） */
  const onPickerKey = (e: ReactKeyboardEvent<HTMLDivElement>) => {
    if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return
    const options = [...(list.current?.querySelectorAll<HTMLElement>('button.st-app-option') ?? [])]
    if (options.length === 0) return
    e.preventDefault()
    const cur = options.indexOf(document.activeElement as HTMLElement)
    if (e.key === 'ArrowUp' && cur <= 0) {
      filter.current?.focus()
      return
    }
    const next =
      e.key === 'ArrowDown'
        ? options[Math.min(options.length - 1, cur + 1)]
        : options[Math.max(0, cur - 1)]
    next?.focus()
  }

  const option = (s: SourceApp) => (
    <button
      key={s.id}
      type="button"
      className="st-app-option"
      data-testid={`clip-app-option-${s.id}`}
      onClick={() => pick(s)}
    >
      <AppIcon src={s.icon_url ? resolveAssetUrl(s.icon_url) : null} name={s.name} size={20} />
      <span className="st-app-option-text">
        <b>{s.name}</b>
        <span>{s.id}</span>
      </span>
    </button>
  )

  return (
    <Field
      label={t('settings.clipboard.excluded')}
      description={t('settings.clipboard.excluded_desc')}
      stacked
    >
      <div className="st-chips" data-testid="clip-excluded">
        {mine.map((a) => (
          <span key={a.app_id} className="st-app-chip" title={a.app_id}>
            <AppIcon name={a.name} size={16} />
            <span>{a.name}</span>
            <button
              type="button"
              className="st-app-chip-remove"
              aria-label={t('settings.clipboard.excluded_remove', { name: a.name })}
              onClick={() => void removeExcludedApp(os, a.app_id)}
            >
              <Icon icon={X} size={12} />
            </button>
          </span>
        ))}
        <Button
          ref={anchor}
          size="sm"
          variant="ghost"
          icon={Plus}
          onClick={() => (picker ? setPicker(false) : open())}
          aria-haspopup="dialog"
          aria-expanded={picker}
          data-testid="clip-excluded-add"
        >
          {t('settings.clipboard.excluded_add')}
        </Button>
      </div>
      <Popover
        open={picker}
        onClose={() => setPicker(false)}
        anchor={anchor}
        label={t('settings.clipboard.excluded_add')}
        initialFocus={filter}
        data-testid="clip-app-popover"
      >
        {/* oxlint-disable-next-line jsx-a11y/no-static-element-interactions -- ↑↓ 只是在过滤框与选项按钮之间移动焦点 */}
        <div className="st-app-picker" data-testid="clip-app-picker" onKeyDown={onPickerKey}>
          <div className="st-app-picker-filter">
            <SearchField
              ref={filter}
              value={query}
              onChange={setQuery}
              placeholder={t('settings.clipboard.excluded_filter')}
              onEnter={(e) => {
                const first = groups.recent[0] ?? groups.running[0]
                if (!first) return
                // 不让这次 Enter 的后续按键事件落到随后拿到焦点的「添加」按钮上（会再点开选择器）
                e.preventDefault()
                pick(first)
              }}
              data-testid="clip-app-filter"
            />
          </div>
          <div ref={list} className="st-app-picker-list">
            {/* 只有读取状态变化（最近来源读回、正在运行的应用读回）时高度过渡；输入过滤时同步跟随（跟手） */}
            <AutoHeight swapKey={`${recent === null ? 'r' : 'R'}${loadingRunning ? 'l' : 'L'}`}>
              {groups.recent.length > 0 ? (
                <>
                  <p className="st-app-picker-title">{t('settings.clipboard.excluded_recent')}</p>
                  {groups.recent.map(option)}
                </>
              ) : null}
              <AnimatePresence initial={false} mode="popLayout">
                {loadingRunning ? (
                  <m.div
                    key="loading"
                    className="st-app-picker-group"
                    {...swap}
                    exit={fadeOut}
                    aria-busy="true"
                  >
                    <p className="st-app-picker-title">
                      {t('settings.clipboard.excluded_running')}
                    </p>
                    {Array.from({ length: placeholders }, (_, i) => (
                      <span
                        key={i}
                        className="st-app-option st-app-option-placeholder"
                        aria-hidden="true"
                      >
                        <span className="st-app-placeholder-icon" />
                        <span className="st-app-option-text">
                          <span className="st-app-placeholder-line" />
                          <span className="st-app-placeholder-line" data-short="" />
                        </span>
                      </span>
                    ))}
                    <span className="ui-sr-only">{t('common.state.loading')}</span>
                  </m.div>
                ) : groups.running.length > 0 ? (
                  <m.div key="running" className="st-app-picker-group" {...swap}>
                    <p className="st-app-picker-title">
                      {t('settings.clipboard.excluded_running')}
                    </p>
                    {groups.running.map(option)}
                  </m.div>
                ) : null}
              </AnimatePresence>
              {empty ? (
                <p className="st-app-picker-empty">
                  {query.trim()
                    ? t('settings.clipboard.excluded_no_match')
                    : t('settings.clipboard.excluded_recent_empty')}
                </p>
              ) : null}
            </AutoHeight>
            <button
              type="button"
              className="st-app-option"
              onClick={() => {
                setPicker(false)
                setManual(true)
              }}
            >
              <span className="st-app-option-icon">
                <Icon icon={AppWindow} size={16} />
              </span>
              <span className="st-app-option-text">
                <b>{t('settings.clipboard.excluded_manual')}</b>
                <span>{t(`settings.clipboard.app_id_hint.${os}`)}</span>
              </span>
            </button>
          </div>
        </div>
      </Popover>
      <AnimatePresence initial={false}>
        {manual ? (
          <m.div key="manual" className="st-clip" {...expand}>
            <div className="st-inline-form">
              <TextField
                value={id}
                onChange={setId}
                placeholder={t(`settings.clipboard.app_id_placeholder.${os}`)}
                aria-label={t('settings.clipboard.app_id')}
                className="st-grow st-mono"
                // oxlint-disable-next-line jsx-a11y/no-autofocus -- 用户刚点了「添加」，表单展开后直接输入
                autoFocus
                onEnter={() => {
                  add(id, name)
                  setId('')
                  setName('')
                  setManual(false)
                }}
              />
              <TextField
                value={name}
                onChange={setName}
                placeholder={t('settings.clipboard.app_name')}
                aria-label={t('settings.clipboard.app_name')}
                className="st-input-sm"
              />
              <Button
                size="sm"
                variant="primary"
                disabled={!id.trim()}
                onClick={() => {
                  add(id, name)
                  setId('')
                  setName('')
                  setManual(false)
                }}
              >
                {t('common.action.add')}
              </Button>
              <Button size="sm" variant="ghost" onClick={() => setManual(false)}>
                {t('common.action.cancel')}
              </Button>
            </div>
          </m.div>
        ) : null}
      </AnimatePresence>
    </Field>
  )
}

/** 应用内的复制 / 粘贴键（终端等：Ctrl+Shift+V），只显示当前平台的条目；内置规则之外的补充 */
function Keymaps({ keymaps }: { keymaps: readonly AppKeymap[] }) {
  const t = useT()
  const os = usePlatform()
  const [adding, setAdding] = useState(false)
  const [appId, setAppId] = useState('')
  const [paste, setPaste] = useState<string | null>(null)
  const expand = usePreset('sectionExpand')
  const mine = keymaps.filter((k) => k.os === os)
  const submit = () => {
    const id = appId.trim()
    if (!id || !paste) return
    void saveKeymap({ os, app_id: id, copy: null, paste })
    setAppId('')
    setPaste(null)
    setAdding(false)
  }
  return (
    <Field
      label={t('settings.clipboard.keymaps')}
      description={t('settings.clipboard.keymaps_desc')}
      stacked
    >
      {mine.length > 0 ? (
        <div className="st-keymaps" data-testid="clip-keymaps">
          {mine.map((k) => (
            <div key={k.app_id} className="st-keymap-row">
              <span className="st-mono st-grow st-ellipsis">{k.app_id}</span>
              {k.copy ? (
                <span className="st-keymap-pair">
                  {t('common.action.copy')} <Kbd combo={k.copy} />
                </span>
              ) : null}
              {k.paste ? (
                <span className="st-keymap-pair">
                  {t('common.action.paste')} <Kbd combo={k.paste} />
                </span>
              ) : null}
              <IconButton
                icon={X}
                size="sm"
                label={t('common.action.remove')}
                onClick={() => void removeKeymap(os, k.app_id)}
              />
            </div>
          ))}
        </div>
      ) : null}
      <AnimatePresence initial={false}>
        {adding ? (
          <m.div key="add" className="st-clip" {...expand}>
            <div className="st-inline-form">
              <TextField
                value={appId}
                onChange={setAppId}
                placeholder={t(`settings.clipboard.app_id_placeholder.${os}`)}
                aria-label={t('settings.clipboard.app_id')}
                className="st-grow st-mono"
                // oxlint-disable-next-line jsx-a11y/no-autofocus -- 用户刚点了「添加」，表单展开后直接输入
                autoFocus
              />
              <KeyCapture
                value={paste}
                onChange={setPaste}
                label={t('settings.clipboard.paste_key')}
              />
              <Button
                size="sm"
                variant="primary"
                disabled={!appId.trim() || !paste}
                onClick={submit}
              >
                {t('common.action.add')}
              </Button>
              <Button size="sm" variant="ghost" onClick={() => setAdding(false)}>
                {t('common.action.cancel')}
              </Button>
            </div>
          </m.div>
        ) : null}
      </AnimatePresence>
      {adding ? null : (
        <div>
          <Button size="sm" variant="ghost" icon={Plus} onClick={() => setAdding(true)}>
            {t('settings.clipboard.keymap_add')}
          </Button>
        </div>
      )}
    </Field>
  )
}
