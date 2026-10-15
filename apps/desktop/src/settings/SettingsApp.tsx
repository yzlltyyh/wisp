/**
 * 设置窗口：左侧分组导航 + 右侧分组页。
 *
 * - 分组 id 只来自 `SETTINGS_SECTIONS`；首次打开读 `boot.route`，已打开时订阅 `window:navigate`；
 * - 各分组页 `lazy()`，首屏之后空闲时预取其余分组，切换时不出现加载态；切换用 `startTransition`：
 *   新分组的代码还没到时继续显示旧分组，而不是闪一下骨架；
 * - 切换动效：侧栏的选中背景是单个元素，在行间 spring.snappy 滑动——按下（pointerdown，与 macOS 侧栏一样在按下时选中）
 *   就在**事件处理里**命令式启动（MotionValue + animate，起点就是按下的这一刻），位置按各行的 offsetTop 直接算出，
 *   不经任何 React 提交：经 state → animate 属性启动的话，起点时间戳与 startTransition 的分片渲染交错，Chromium（≈
 *   WebView2）里逐帧位移忽快忽停（27 → 4.7 → 18.5 px）；
 *   内容区旧页立即卸载、新页从 styleSwitchOutPlain 的透明度（0.3）淡入到 1（pageSwitch 的 120 ms 进入）——
 *   任何时刻只有一页、也没有一整帧的空白；减少动效时 ≤ 100 ms 纯淡入；
 * - 在子页里再点一次当前分组：回到该分组的根视图（context.ts 的 reselect）；子页上报标题，紧凑标题显示它；
 * - 改动即时生效、没有「保存」按钮；保存失败时底部轻提示并回到服务器状态（store.ts）；⌘W / Ctrl+W 先提交所有延迟写入、
 *   等在途写入返回再关窗（lib/writes.ts）；
 * - macOS 标题栏是 Overlay（隐藏标题）：侧栏顶部给交通灯留出空间，顶部条带是拖动区域（data-tauri-drag-region）；
 *   内容滚动后顶部条带显示紧凑标题与分隔线（只改 data 属性，不重渲染）；
 * - 页面底色只取 `--window-bg`，顶部条带等遮盖内容的浮层取 `--window-chrome-bg`（为 Win11 Mica 预留，
 *   见 settings.css）。
 */
import './settings.css'
import { FileWarning, FolderOpen } from 'lucide-react'
import { animate, m, useMotionValue } from 'motion/react'
import {
  lazy,
  startTransition,
  Suspense,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ComponentType,
  type KeyboardEvent as ReactKeyboardEvent,
  type LazyExoticComponent,
  type UIEvent,
} from 'react'
import { usePlatform } from '../shared/hooks/usePlatform'
import { useT } from '../shared/i18n/useT'
import { api, boot, isSettingsSection, on, type SettingsSection } from '../shared/ipc'
import { isSettingsAnchor } from '../shared/ipc/sections'
import type { WindowTarget } from '../shared/ipc/types'
import { useKeyScope } from '../shared/keys/useKeyScope'
import { MotionRoot } from '../shared/motion/MotionRoot'
import { adaptPreset, presets, reducedPolicy } from '../shared/motion/presets'
import { useReducedMotionPref } from '../shared/motion/useReducedMotionPref'
import { Button, EmptyState, Icon, Skeleton } from '../shared/ui'
import { prefetchAdvice } from './components/AdviceBanners'
import { IssueBanners } from './components/IssueBanners'
import { NoticeToast } from './components/NoticeToast'
import { SettingsNavContext, type SettingsNav } from './context'
import { loadSettingsMessages } from './i18n'
import { ipcErrorText } from './lib/errors'
import { closeWindowSafely } from './lib/writes'
import { SECTION_ICONS, SECTIONS } from './nav'
import {
  loadSettings,
  notify,
  setSaveErrorHandler,
  startSettingsSync,
  useSettingsStore,
} from './store'

loadSettingsMessages()

type PageModule = { default: ComponentType }

const LOADERS: Record<SettingsSection, () => Promise<PageModule>> = {
  general: () => import('./pages/GeneralPage'),
  hotkeys: () => import('./pages/HotkeysPage'),
  providers: () => import('./pages/ProvidersPage'),
  translate: () => import('./pages/TranslatePage'),
  ocr: () => import('./pages/OcrPage'),
  clipboard: () => import('./pages/ClipboardPage'),
  appearance: () => import('./pages/AppearancePage'),
  sync: () => import('./pages/SyncPage'),
  about: () => import('./pages/AboutPage'),
}

/** 每个分组只请求一次模块：lazy() 与空闲预取共用同一个 Promise */
const loaded = new Map<SettingsSection, Promise<PageModule>>()
function load(section: SettingsSection): Promise<PageModule> {
  let p = loaded.get(section)
  if (!p) {
    p = LOADERS[section]()
    loaded.set(section, p)
  }
  return p
}

const PAGES = Object.fromEntries(SECTIONS.map((s) => [s, lazy(() => load(s))])) as Record<
  SettingsSection,
  LazyExoticComponent<ComponentType>
>

/** 空闲时执行（WebKit 没有 requestIdleCallback，退回计时器） */
function idle(): Promise<void> {
  return new Promise<void>((resolve) => {
    if (typeof requestIdleCallback === 'function') requestIdleCallback(() => resolve())
    else setTimeout(resolve, 1)
  })
}

/** 首屏之后空闲时依次预取其余分组与快捷键页的提示（邻居冲突 / 键盘布局，避免进页面后再插入横幅把内容往下推） */
function prefetchAll(): () => void {
  let cancelled = false
  const run = async () => {
    await idle()
    if (cancelled) return
    prefetchAdvice()
    // 有意串行：每个分组各占一个空闲时段，不与首屏和交互抢主线程
    for (const section of SECTIONS) {
      // oxlint-disable-next-line no-await-in-loop
      await idle()
      if (cancelled) return
      // oxlint-disable-next-line no-await-in-loop
      await load(section).catch(() => undefined)
    }
  }
  void run()
  return () => {
    cancelled = true
  }
}

function initialSection(): SettingsSection {
  const route = boot.route
  if (route?.kind === 'settings' && isSettingsSection(route.section)) return route.section
  return 'general'
}

/** 分组内的锚点：只接受 `SETTINGS_ANCHORS` 里登记的组合，其余停在分组顶部 */
function anchorOf(target: WindowTarget | null): string | null {
  if (target?.kind !== 'settings' || !isSettingsSection(target.section)) return null
  return isSettingsAnchor(target.section, target.anchor) ? target.anchor : null
}

export function SettingsApp() {
  return (
    <MotionRoot>
      <SettingsWindow />
    </MotionRoot>
  )
}

function SettingsWindow() {
  const t = useT()
  const os = usePlatform()
  const reduced = useReducedMotionPref()
  const [section, setSection] = useState<SettingsSection>(initialSection)
  const [focus, setFocus] = useState<string | null>(() => anchorOf(boot.route))
  const loadedSnap = useSettingsStore((s) => s.snap !== null)
  const loadError = useSettingsStore((s) => s.loadError)
  const topbar = useRef<HTMLDivElement>(null)
  /** 切换过分组：之后的分组页才淡入（窗口第一次显示时不淡入，窗口本身由 Rust 在首帧之后显示） */
  const [switched, setSwitched] = useState(false)
  // 只有新页的进入：旧页随 key 变化立即卸载（不叠放、不交叉淡入）。pageSwitch 从「变淡」（0.3）回到 1，没有一整帧
  // 内容区全空；减少动效时 ≤ 100 ms 纯淡入
  const pageIn = adaptPreset(presets.pageSwitch, reducedPolicy.pageSwitch, reduced)
  const [reselect, setReselect] = useState(0)
  const [subTitle, setSubTitle] = useState<string | null>(null)

  useEffect(() => {
    setSaveErrorHandler((error) =>
      notify(t('settings.notice.save_failed', { reason: ipcErrorText(error) }), 'danger'),
    )
  }, [t])

  useEffect(() => {
    const stop = startSettingsSync()
    void loadSettings()
    const cancelPrefetch = prefetchAll()
    return () => {
      stop()
      cancelPrefetch()
    }
  }, [])

  const go = useCallback((next: SettingsSection, nextFocus: string | null = null) => {
    if (topbar.current) topbar.current.dataset.scrolled = 'false'
    startTransition(() => {
      setSwitched(true)
      setSection(next)
      setFocus(nextFocus)
    })
  }, [])

  // 已打开时的深链（托盘「设置 → 某分组」、面板里的「去设置」）
  useEffect(() => {
    let disposed = false
    let unlisten: (() => void) | null = null
    void on('window:navigate', (target) => {
      if (target.kind !== 'settings') return
      go(isSettingsSection(target.section) ? target.section : 'general', anchorOf(target))
    }).then((un) => {
      if (disposed) un()
      else unlisten = un
    })
    return () => {
      disposed = true
      unlisten?.()
    }
  }, [go])

  // ⌘W / Ctrl+W：先提交所有延迟写入（焦点所在的输入框、合并写入、术语表、风格）并等它们返回，再关窗
  useKeyScope(
    'settings-window',
    [{ combo: 'Mod+KeyW', allowInInput: true, run: () => void closeWindowSafely() }],
    { layer: 'global' },
  )

  const nav: SettingsNav = useMemo(
    () => ({
      section,
      go,
      focus,
      consumeFocus: () => setFocus(null),
      reselect,
      setTitle: setSubTitle,
    }),
    [section, go, focus, reselect],
  )
  /** 侧栏选中某个分组：已经是当前分组时回到它的根视图（子页自己处理，见 context.ts） */
  const select = (s: SettingsSection) => {
    if (s === section) setReselect((n) => n + 1)
    else go(s)
  }

  const onScroll = (e: UIEvent<HTMLDivElement>) => {
    const el = topbar.current
    if (!el) return
    const scrolled = e.currentTarget.scrollTop > COMPACT_TITLE_AFTER_PX
    if ((el.dataset.scrolled === 'true') !== scrolled) el.dataset.scrolled = String(scrolled)
  }

  const Page = PAGES[section]
  const failed = loadError !== null && !loadedSnap

  return (
    <SettingsNavContext.Provider value={nav}>
      <div className="st-window" data-os={os} data-testid="settings-app">
        <Sidebar section={section} onSelect={select} disabled={!loadedSnap} />
        <main className="st-main">
          <div ref={topbar} className="st-topbar" data-tauri-drag-region data-scrolled="false">
            <span className="st-topbar-title" data-tauri-drag-region aria-hidden="true">
              {subTitle ?? t(`settings.nav.${section}`)}
            </span>
          </div>
          <div className="st-pages">
            {failed ? (
              <LoadError error={loadError} />
            ) : (
              <m.div
                key={section}
                id={PANEL_ID}
                role="tabpanel"
                aria-labelledby={tabId(section)}
                className="st-page-scroll"
                data-section={section}
                data-testid={`settings-page-${section}`}
                onScroll={onScroll}
                initial={switched ? pageIn.initial : false}
                animate={pageIn.animate}
              >
                {loadedSnap ? (
                  <>
                    <IssueBanners />
                    <Suspense fallback={<PageSkeleton />}>
                      <Page />
                    </Suspense>
                  </>
                ) : (
                  <PageSkeleton />
                )}
              </m.div>
            )}
          </div>
          <NoticeToast />
        </main>
      </div>
    </SettingsNavContext.Provider>
  )
}

/** 内容滚动超过页标题后，顶部条带显示紧凑标题 */
const COMPACT_TITLE_AFTER_PX = 36
const PANEL_ID = 'st-panel'
const tabId = (s: SettingsSection) => `st-tab-${s}`

/** 读取失败：说明、配置目录（路径 + 打开）、重试 */
function LoadError({ error }: { error: Parameters<typeof ipcErrorText>[0] }) {
  const t = useT()
  const [dir, setDir] = useState<string | null>(null)
  useEffect(() => {
    let alive = true
    void api
      .appInfo()
      .then((meta) => alive && setDir(meta.config_dir))
      .catch(() => undefined)
    return () => {
      alive = false
    }
  }, [])
  return (
    <div className="st-page-scroll" data-testid="settings-load-error">
      {/* 内容区里垂直居中（与引导窗口的读取失败页、面板的 EmptyState 同一种排版） */}
      <div className="st-page st-page-center">
        <EmptyState
          icon={FileWarning}
          title={t('settings.load_failed')}
          detail={
            <span className="st-load-error">
              <span>{ipcErrorText(error)}</span>
              <span>{t('settings.load_failed_detail')}</span>
              {dir ? (
                <span className="st-path" data-testid="settings-load-error-path">
                  {dir}
                </span>
              ) : null}
            </span>
          }
          action={
            <span className="st-empty-actions">
              <Button
                icon={FolderOpen}
                onClick={() =>
                  void api
                    .openPath('config')
                    .catch((e) => notify(ipcErrorText(e, 'open_path'), 'danger'))
                }
                data-testid="settings-open-config"
              >
                {t('settings.issue.open_folder')}
              </Button>
              <Button variant="primary" onClick={() => void loadSettings()}>
                {t('common.action.retry')}
              </Button>
            </span>
          }
        />
      </div>
    </div>
  )
}

function Sidebar({
  section,
  onSelect,
  disabled,
}: {
  section: SettingsSection
  onSelect: (s: SettingsSection) => void
  /** 设置还没读到（加载中 / 读取失败）：导航不可用 */
  disabled: boolean
}) {
  const t = useT()
  const reduced = useReducedMotionPref()
  const refs = useRef<Partial<Record<SettingsSection, HTMLButtonElement | null>>>({})
  const nav = useRef<HTMLDivElement>(null)
  /** 各导航项的 offsetTop（行高、间距由 CSS 决定）：挂载时量一次，字号 / 缩放变化时重量 */
  const tops = useRef<Partial<Record<SettingsSection, number>>>({})
  /** 量过之后才显示选中背景 */
  const [measured, setMeasured] = useState(false)
  /** 选中背景的位置（命令式驱动，见文件头）与它要去的分组 */
  const y = useMotionValue(0)
  const target = useRef(section)
  /** 滑到某个分组：按下时在事件处理里调用（起点 = 这一刻）；减少动效时直接到位 */
  const slideTo = (s: SettingsSection) => {
    target.current = s
    const top = tops.current[s]
    if (top === undefined) return
    if (reduced) y.jump(top)
    else animate(y, top, presets.indicatorSlide)
  }
  useLayoutEffect(() => {
    const measure = () => {
      const next: Partial<Record<SettingsSection, number>> = {}
      for (const s of SECTIONS) {
        const el = refs.current[s]
        if (el) next[s] = el.offsetTop
      }
      tops.current = next
      // 行高变了（字号 / 缩放）：直接放到当前分组上，不播滑动
      const top = next[target.current]
      if (top !== undefined) y.jump(top)
      setMeasured(top !== undefined)
    }
    measure()
    const el = nav.current
    if (!el || typeof ResizeObserver === 'undefined') return
    const ro = new ResizeObserver(measure)
    ro.observe(el)
    return () => ro.disconnect()
  }, [y])
  // 分组从别处改变（深链、子页的跳转）：滑过去；按下时已经在滑的不重复启动
  useLayoutEffect(() => {
    if (target.current !== section) slideTo(section)
    // oxlint-disable-next-line react/exhaustive-deps -- 只在分组变化时执行
  }, [section])
  const choose = (s: SettingsSection) => {
    if (target.current !== s) slideTo(s)
    onSelect(s)
  }

  // ↑↓ / Home / End 在导航项之间移动并切换（roving tabindex：只有当前项能 Tab 到）。
  // 下一项按焦点所在的项计算——section 由 startTransition 延后更新，连按时读到的是旧值
  const onKeyDown = (e: ReactKeyboardEvent) => {
    const focused = SECTIONS.findIndex((s) => refs.current[s] === document.activeElement)
    const i = focused >= 0 ? focused : SECTIONS.indexOf(section)
    let next = -1
    if (e.key === 'ArrowDown') next = (i + 1) % SECTIONS.length
    else if (e.key === 'ArrowUp') next = (i - 1 + SECTIONS.length) % SECTIONS.length
    else if (e.key === 'Home') next = 0
    else if (e.key === 'End') next = SECTIONS.length - 1
    if (next < 0) return
    e.preventDefault()
    const s = SECTIONS[next] as SettingsSection
    refs.current[s]?.focus()
    choose(s)
  }
  return (
    <nav className="st-sidebar" aria-label={t('settings.nav.label')}>
      <div className="st-sidebar-drag" data-tauri-drag-region />
      <div
        ref={nav}
        className="st-nav"
        role="tablist"
        tabIndex={-1}
        aria-orientation="vertical"
        aria-disabled={disabled || undefined}
        data-disabled={disabled || undefined}
        onKeyDown={disabled ? undefined : onKeyDown}
      >
        {measured ? <m.span className="st-nav-indicator" aria-hidden="true" style={{ y }} /> : null}
        {SECTIONS.map((s) => (
          <button
            key={s}
            ref={(el) => {
              refs.current[s] = el
            }}
            id={tabId(s)}
            type="button"
            role="tab"
            className="st-nav-item"
            aria-selected={s === section}
            aria-controls={PANEL_ID}
            tabIndex={s === section ? 0 : -1}
            disabled={disabled}
            data-active={s === section || undefined}
            data-testid={`settings-nav-${s}`}
            // 与 macOS 侧栏一样在按下时选中（选中背景立即开始移动）；键盘的 Enter / Space 走 click（detail 为 0）
            onPointerDown={(e) => {
              if (e.button === 0 && !disabled) choose(s)
            }}
            onClick={(e) => {
              if (e.detail === 0) choose(s)
            }}
            onPointerEnter={() => {
              void load(s)
              if (s === 'hotkeys') prefetchAdvice()
            }}
          >
            <Icon icon={SECTION_ICONS[s]} size={16} />
            <span className="st-nav-label">{t(`settings.nav.${s}`)}</span>
          </button>
        ))}
      </div>
    </nav>
  )
}

function PageSkeleton() {
  return (
    <div className="st-page" data-testid="settings-page-loading" aria-busy="true">
      <div className="st-skeleton-title">
        <Skeleton lines={1} />
      </div>
      <div className="st-skeleton-card">
        <Skeleton lines={3} />
      </div>
      <div className="st-skeleton-card">
        <Skeleton lines={2} />
      </div>
    </div>
  )
}
