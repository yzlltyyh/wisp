/**
 * 设置 → 关于：版本与平台；更新（检查 / 下载并安装 / 重启；`checking` 与 `error` 也可能
 * 带着已知的新版本——继续显示版本、变更日志与安装按钮；Windows 的 ready =「已下载、空闲时自动安装并重启」，
 * macOS / Linux 的 ready =「已安装、重启后生效」）；配置 / 数据 / 日志目录（一键打开、复制路径）；
 * 诊断导出（脱敏、不含 Key 与正文）；开源许可：Wisp 本身（MIT）与第三方依赖（`src/generated/third-party-licenses.json`，
 * 按许可证分组、可展开，展开时才加载）；致谢。
 */
import {
  ArrowUpCircle,
  CircleCheck,
  Copy,
  ExternalLink,
  FileArchive,
  FolderOpen,
  RefreshCw,
  RotateCw,
  TriangleAlert,
} from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import { useCopy } from '../../shared/hooks/useCopy'
import { usePlatform } from '../../shared/hooks/usePlatform'
import { useT, useTKey } from '../../shared/i18n/useT'
import { api } from '../../shared/ipc'
import type { AppMeta, PathKind, UpdateStatus } from '../../shared/ipc/types'
import { parseMdLite } from '../../shared/markdown/mdlite'
import { renderBlocks } from '../../shared/markdown/render'
import { usePreset } from '../../shared/motion/presets'
import { Badge, Button, Field, IconButton, Icon, Section, Skeleton, Spinner } from '../../shared/ui'
import { Disclosure } from '../components/Disclosure'
import { Page } from '../components/Page'
import { ipcErrorText } from '../lib/errors'
import { isKeyLike } from '../lib/errors'
import { notify, setUpdate, useRuntime } from '../store'
import appIcon from '../../../src-tauri/icons/src/app-icon.svg'
import { relaunchSafely } from '../lib/writes'

const REPO_URL = 'https://github.com/yzlltyyh/wisp'
const PERCENT = 100

interface LicensePackage {
  name: string
  version: string
  license: string
  repository: string | null
}

export default function AboutPage() {
  const t = useT()
  const runtime = useRuntime()
  const [meta, setMeta] = useState<AppMeta | null>(null)
  const [exporting, setExporting] = useState(false)
  useEffect(() => {
    void api
      .appInfo()
      .then(setMeta)
      .catch(() => undefined)
  }, [])
  if (!runtime) return null
  const p = runtime.platform

  const exportDiagnostics = async () => {
    setExporting(true)
    try {
      const path = await api.diagnosticsExport()
      if (path) notify(t('settings.about.diag_done', { path }), 'success', { sticky: true })
    } catch (error) {
      notify(ipcErrorText(error, 'diagnostics_export'), 'danger')
    } finally {
      setExporting(false)
    }
  }

  return (
    <Page title={t('settings.nav.about')}>
      <div className="st-about-hero" data-testid="about-hero">
        <img className="st-about-icon" src={appIcon} alt="" width={64} height={64} />
        <div className="st-about-id">
          <b>{t('common.app.name')}</b>
          <span className="st-tabular">
            {t('settings.about.version', { version: runtime.version })}
            {meta?.dev_mode ? <Badge tone="neutral">{t('settings.about.dev_build')}</Badge> : null}
          </span>
          <span className="st-about-platform">
            {t(`settings.about.os.${p.os}`)} {p.os_version} · {p.arch}
            {p.os === 'linux' ? ` · ${p.session}` : ''}
          </span>
        </div>
      </div>

      <UpdateSection status={runtime.update} />

      <Section title={t('settings.about.folders')}>
        {(['config', 'data', 'log'] as const).map((kind) => (
          <FolderRow
            key={kind}
            kind={kind}
            path={
              meta
                ? kind === 'config'
                  ? meta.config_dir
                  : kind === 'data'
                    ? meta.data_dir
                    : meta.log_dir
                : null
            }
          />
        ))}
        <Field
          label={t('settings.about.diagnostics')}
          description={t('settings.about.diagnostics_desc')}
        >
          <Button
            size="sm"
            icon={FileArchive}
            loading={exporting}
            onClick={() => void exportDiagnostics()}
            data-testid="about-diagnostics"
          >
            {t('settings.about.diagnostics_export')}
          </Button>
        </Field>
      </Section>

      <Section title={t('settings.about.licenses')}>
        <Field
          label={t('settings.about.wisp_license')}
          description={t('settings.about.wisp_license_desc')}
        >
          <Button
            size="sm"
            variant="ghost"
            icon={ExternalLink}
            onClick={() => void api.openUrl(REPO_URL)}
          >
            {t('settings.about.source')}
          </Button>
        </Field>
        <ThirdParty />
      </Section>

      <p className="st-about-thanks">{t('settings.about.thanks')}</p>
    </Page>
  )
}

function FolderRow({ kind, path }: { kind: PathKind; path: string | null }) {
  const t = useT()
  const copy = useCopy()
  return (
    <Field
      label={t(`settings.about.folder.${kind}`)}
      description={
        <span className="st-path" data-loading={path ? undefined : true}>
          {path ?? t('common.state.loading')}
        </span>
      }
      data-testid={`about-folder-${kind}`}
    >
      <IconButton
        icon={Copy}
        size="sm"
        label={t('settings.about.copy_path')}
        disabled={!path}
        onClick={() =>
          path &&
          void copy(path).then((ok) =>
            notify(
              ok ? t('common.state.copied') : t('common.state.copy_failed'),
              ok ? 'success' : 'danger',
            ),
          )
        }
      />
      <Button
        size="sm"
        icon={FolderOpen}
        onClick={() =>
          void api.openPath(kind).catch((e) => notify(ipcErrorText(e, 'open_path'), 'danger'))
        }
      >
        {t('settings.about.open')}
      </Button>
    </Field>
  )
}

function UpdateSection({ status }: { status: UpdateStatus }) {
  const t = useT()
  const tKey = useTKey()
  const os = usePlatform()
  const cardIn = usePreset('cardIn')
  const [busy, setBusy] = useState(false)
  const check = async () => {
    setBusy(true)
    try {
      setUpdate(await api.updateCheck())
    } catch (error) {
      notify(ipcErrorText(error, 'update_check'), 'danger')
    } finally {
      setBusy(false)
    }
  }
  const install = async () => {
    try {
      await api.updateInstall()
    } catch (error) {
      notify(ipcErrorText(error, 'update_install'), 'danger')
    }
  }
  const s = status.state
  const known = status.version !== null && (s === 'available' || s === 'checking' || s === 'error')

  let line
  switch (s) {
    case 'checking':
      line = (
        <span className="st-update-line">
          <Spinner size={14} />
          {t('settings.about.update.checking')}
        </span>
      )
      break
    case 'up_to_date':
      line = (
        <span className="st-update-line" data-tone="success">
          <Icon icon={CircleCheck} size={14} />
          {t('settings.about.update.up_to_date')}
        </span>
      )
      break
    case 'available':
      line = (
        <span className="st-update-line" data-tone="accent">
          <Icon icon={ArrowUpCircle} size={14} />
          {t('settings.about.update.available', { version: status.version ?? '' })}
        </span>
      )
      break
    case 'downloading':
      line = (
        <span className="st-update-line">
          <Spinner size={14} />
          {t('settings.about.update.downloading', {
            n: Math.round((status.progress ?? 0) * PERCENT),
          })}
        </span>
      )
      break
    case 'ready':
      line = (
        <span className="st-update-line" data-tone="success">
          <Icon icon={CircleCheck} size={14} />
          {os === 'windows'
            ? t('settings.about.update.ready_windows', { version: status.version ?? '' })
            : t('settings.about.update.ready', { version: status.version ?? '' })}
        </span>
      )
      break
    case 'error':
      line = (
        <span className="st-update-line" data-tone="danger">
          <Icon icon={TriangleAlert} size={14} />
          {t('settings.about.update.error')}
        </span>
      )
      break
    case 'disabled':
      line = <span className="st-update-line">{t('settings.about.update.disabled')}</span>
      break
    default:
      line = <span className="st-update-line">{t('settings.about.update.idle')}</span>
  }

  return (
    <Section title={t('settings.about.updates')} data-testid="about-update">
      <div className="st-update" data-state={s}>
        <div className="st-update-row">
          <m.div key={s} className="st-update-status" {...cardIn}>
            {line}
            {s === 'error' && status.error && isKeyLike(status.error) ? (
              // Rust 下发的是文案 key 时直接翻译；开发者文本不直接给用户看（放进下面可折叠的详情）
              <span className="st-update-error">{tKey(status.error, undefined, '')}</span>
            ) : null}
            {known && s !== 'available' ? (
              <span className="st-update-known">
                {t('settings.about.update.available', { version: status.version ?? '' })}
              </span>
            ) : null}
          </m.div>
          <span className="st-update-actions">
            {s === 'ready' ? (
              <Button
                size="sm"
                variant="primary"
                icon={RotateCw}
                onClick={() => void relaunchSafely()}
                data-testid="update-relaunch"
              >
                {os === 'windows'
                  ? t('settings.about.update.install_now')
                  : t('settings.about.update.relaunch')}
              </Button>
            ) : known ? (
              <Button
                size="sm"
                variant="primary"
                icon={ArrowUpCircle}
                onClick={() => void install()}
                data-testid="update-install"
              >
                {t('settings.about.update.install')}
              </Button>
            ) : null}
            {s === 'downloading' || s === 'ready' ? null : known ? (
              // 已经知道有新版本：「检查更新」退成图标按钮（与「下载并安装」意义重叠，不再和它抢宽度）
              <IconButton
                icon={RefreshCw}
                label={t('settings.about.update.check')}
                disabled={busy || s === 'checking'}
                glyph={busy || s === 'checking' ? <Spinner size={14} /> : undefined}
                onClick={() => void check()}
                data-testid="update-check"
              />
            ) : (
              <Button
                size="sm"
                icon={RefreshCw}
                loading={busy || s === 'checking'}
                disabled={s === 'disabled'}
                onClick={() => void check()}
                data-testid="update-check"
              >
                {t('settings.about.update.check')}
              </Button>
            )}
          </span>
        </div>
        <AnimatePresence initial={false}>
          {s === 'downloading' ? (
            <m.div key="progress" className="st-progress" {...cardIn}>
              <span
                className="st-progress-bar"
                style={{ transform: `scaleX(${Math.max(0, Math.min(1, status.progress ?? 0))})` }}
              />
            </m.div>
          ) : null}
        </AnimatePresence>
        {s === 'error' && status.error && !isKeyLike(status.error) ? (
          <Disclosure title={t('settings.about.update.error_details')} variant="plain">
            <pre className="st-notes st-error-detail" data-testid="update-error-detail">
              {status.error}
            </pre>
          </Disclosure>
        ) : null}
        {status.notes && (known || s === 'downloading' || s === 'ready') ? (
          <Disclosure
            title={t('settings.about.update.notes')}
            variant="plain"
            defaultOpen={s === 'available'}
          >
            <ReleaseNotes text={status.notes} />
          </Disclosure>
        ) : null}
      </div>
    </Section>
  )
}

/**
 * 更新内容：发布说明是 Markdown（「- 新增：…」的列表、粗体、行内代码），用 mdlite 渲染（与面板正文同一套解析与样式），
 * 不再把「- 」原样显示
 */
function ReleaseNotes({ text }: { text: string }) {
  const ref = useRef<HTMLDivElement>(null)
  useLayoutEffect(() => {
    if (ref.current) renderBlocks(parseMdLite(text), ref.current)
  }, [text])
  return <div ref={ref} className="st-notes ui-md" data-testid="update-notes" />
}

/** 第三方许可清单（动态加载，只在展开时请求；加载失败时可以重试） */
function loadLicenses(): Promise<Array<[string, LicensePackage[]]>> {
  return import('../../generated/third-party-licenses.json').then((mod) => {
    const map = new Map<string, LicensePackage[]>()
    for (const pkg of mod.default as LicensePackage[]) {
      const arr = map.get(pkg.license) ?? []
      arr.push(pkg)
      map.set(pkg.license, arr)
    }
    return [...map.entries()].sort((a, b) => b[1].length - a[1].length || a[0].localeCompare(b[0]))
  })
}

function ThirdParty() {
  const t = useT()
  const [open, setOpen] = useState(false)
  const [groups, setGroups] = useState<Array<[string, LicensePackage[]]> | null>(null)
  const [failed, setFailed] = useState(false)
  const total = groups?.reduce((n, [, pkgs]) => n + pkgs.length, 0) ?? 0
  // 展开时才加载；失败后点「重试」（failed 回到 false）再加载一次
  useEffect(() => {
    if (!open || groups || failed) return
    let alive = true
    loadLicenses()
      .then((g) => alive && setGroups(g))
      .catch(() => alive && setFailed(true))
    return () => {
      alive = false
    }
  }, [open, groups, failed])
  return (
    <Disclosure
      title={t('settings.about.third_party')}
      meta={groups ? t('settings.about.third_party_count', { count: total, n: total }) : null}
      open={open}
      onOpenChange={setOpen}
      data-testid="about-licenses"
    >
      {failed ? (
        <div className="st-card-empty" data-testid="licenses-failed">
          <span>{t('settings.about.licenses_failed')}</span>
          <Button size="sm" variant="ghost" icon={RefreshCw} onClick={() => setFailed(false)}>
            {t('common.action.retry')}
          </Button>
        </div>
      ) : groups === null ? (
        <div className="st-card-pad">
          <Skeleton lines={3} />
        </div>
      ) : (
        <div className="st-licenses">
          {groups.map(([license, pkgs]) => (
            <Disclosure
              key={license}
              variant="plain"
              title={<span className="st-mono">{license}</span>}
              meta={t('settings.about.packages', { count: pkgs.length, n: pkgs.length })}
              data-testid="license-group"
            >
              <ul className="st-license-list">
                {pkgs.map((pkg) => (
                  <li key={`${pkg.name}@${pkg.version}`}>
                    <span className="st-license-name">{pkg.name}</span>
                    <span className="st-license-version st-tabular">{pkg.version}</span>
                    {pkg.repository ? (
                      <button
                        type="button"
                        className="st-link"
                        onClick={() => pkg.repository && void api.openUrl(pkg.repository)}
                        aria-label={t('settings.about.open_repo', { name: pkg.name })}
                      >
                        <Icon icon={ExternalLink} size={12} />
                      </button>
                    ) : null}
                  </li>
                ))}
              </ul>
            </Disclosure>
          ))}
        </div>
      )}
    </Disclosure>
  )
}
