/**
 * 同步 → 连接子视图（草稿，与新建模型服务同一种流程：填写 → 测试 → 提交，提交之前什么都不写）。
 *
 * - 服务预设（坚果云、123 云盘、Nextcloud、群晖 NAS、自定义）只用来预填地址、提示应用密码在哪里申请；
 * - 服务器地址就地检查：http 只允许局域网地址（否则应用密码会以明文经过网络），失焦或测试之后才提示，输入中不打扰；
 * - 应用密码只在这次请求里交给 Rust（`sync_probe` / `sync_connect`），连接成功后由 Rust 写入凭据存储；
 *   「更新密码…」时地址与账号只读，密码留空表示沿用已保存的那一份；
 * - 「测试连接」分步报告（ProbeReport）决定同步密码一节的样子：服务器没有数据 → 设置并确认（可生成）；已有数据 →
 *   输入并说明合并规则，忘记时可就地确认清除服务器上的数据重新开始；本机就是这份数据 → 重新连接；
 *   连接信息改过之后旧结论作废，需要重新测试才能连接；
 * - 同步密码交给 Rust 之前做 NFC 规范化；规则（≥ 12 个字符、不与应用密码 / 账号 / 主机名相同）就地提示，Rust 再校验一次；
 * - 连接成功后不弹提示，直接回到已连接页（后台的上传 / 下载进度在状态行里）；
 * - Esc：输入框里先退出输入，否则返回；有改动时在底栏原地确认放弃。再点一次侧栏的「同步」同样走返回。
 */
import {
  ArrowLeft,
  Cloud,
  Eye,
  EyeOff,
  KeyRound,
  RefreshCw,
  ShieldAlert,
  Sparkles,
  Trash2,
  X,
} from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useEffect, useLayoutEffect, useRef, useState, type ReactNode, type RefObject } from 'react'
import { usePlatform } from '../../shared/hooks/usePlatform'
import { useT } from '../../shared/i18n/useT'
import { api, WispIpcError } from '../../shared/ipc'
import type { SyncConnectOutcome, SyncContent } from '../../shared/ipc/types'
import { useKeyScope } from '../../shared/keys/useKeyScope'
import { usePreset } from '../../shared/motion/presets'
import { useReducedMotionPref } from '../../shared/motion/useReducedMotionPref'
import { Button, Field, Icon, IconButton, Section, Select, TextField } from '../../shared/ui'
import { DangerRow, useConfirmKeys } from '../components/Confirm'
import { useReselect, useSubviewTitle } from '../context'
import { ipcErrorText } from '../lib/errors'
import { revealGrowing } from '../lib/reveal'
import { trackWrite } from '../lib/writes'
import { applySnapshot, notify, setSync, useSettings } from '../store'
import { ContentRows } from './ContentRows'
import {
  checkServerUrl,
  hostOf,
  normalizePassphrase,
  passModeOf,
  passphraseIssue,
  presetById,
  presetFor,
  SYNC_PRESETS,
  SYNOLOGY_PACKAGE,
  urlForPreset,
  type PassMode,
  type SyncPresetId,
} from './model'
import { ProbeReport, type ProbeRun } from './ProbeReport'
import { useNow } from './useNow'

/** new：未连接时新建；update：已连接时更新密码 / 重新连接（地址与账号只读） */
export type ConnectMode = 'new' | 'update'

export interface ConnectViewProps {
  mode: ConnectMode
  onBack: () => void
  onConnected: (outcome: SyncConnectOutcome) => void
}

/** 连接 / 清除改了 settings.sync（Rust 写入并广播）：先取一份最新快照，返回已连接页时不会闪一下旧状态 */
async function refreshSettings(): Promise<void> {
  try {
    applySnapshot(await api.settingsGet())
  } catch {
    // 随后的 settings:changed 同样会带来新快照
  }
}

export default function ConnectView({ mode, onBack, onConnected }: ConnectViewProps) {
  const t = useT()
  const os = usePlatform()
  const settings = useSettings()
  const now = useNow()
  const reduced = useReducedMotionPref()
  const fade = usePreset('footerIn')
  const expand = usePreset('sectionExpand')
  const swap = usePreset('cardIn')
  const readOnly = mode === 'update'
  const saved = settings?.sync
  const [initial] = useState(() => ({
    url: saved?.server_url ?? '',
    username: saved?.username ?? '',
    content: saved?.content ?? null,
  }))

  const [preset, setPreset] = useState<SyncPresetId | null>(() => presetFor(initial.url))
  const [url, setUrl] = useState(initial.url)
  const [username, setUsername] = useState(initial.username)
  const [password, setPassword] = useState('')
  const [content, setContent] = useState<SyncContent | null>(initial.content)
  const [urlTouched, setUrlTouched] = useState(false)

  const [run, setRun] = useState<ProbeRun>({ phase: 'idle' })
  const [probedFor, setProbedFor] = useState<string | null>(null)
  const seq = useRef(0)

  const [pass, setPass] = useState('')
  const [confirm, setConfirm] = useState('')
  const [reveal, setReveal] = useState(false)
  const [generated, setGenerated] = useState<string | null>(null)
  const [passTouched, setPassTouched] = useState(false)
  const [passError, setPassError] = useState<string | null>(null)
  const [connectError, setConnectError] = useState<string | null>(null)
  const [connecting, setConnecting] = useState(false)
  const [resetting, setResetting] = useState(false)
  const [confirmDiscard, setConfirmDiscard] = useState(false)

  const urlInput = useRef<HTMLInputElement>(null)
  const passwordInput = useRef<HTMLInputElement>(null)
  const passInput = useRef<HTMLInputElement>(null)
  const footRef = useRef<HTMLDivElement>(null)
  const discardCancel = useRef<HTMLButtonElement>(null)

  // 打开时焦点给第一个要填的输入框（layout 阶段：子视图导航看到输入框已有焦点，就不再把焦点移到标题）
  useLayoutEffect(() => {
    const target = readOnly ? passwordInput.current : urlInput.current
    target?.focus({ preventScroll: true })
  }, [readOnly])
  // 卸载后迟到的测试结果不再写回
  useEffect(
    () => () => {
      seq.current += 1
    },
    [],
  )

  const account = { server_url: url.trim(), username: username.trim() }
  const secret = password.trim()
  /** 交给 Rust 的应用密码：更新时留空 = 用凭据存储里已保存的那一份 */
  const passwordArg = secret || (readOnly ? null : '')
  const urlCheck = checkServerUrl(url)
  const fingerprint = JSON.stringify([account.server_url, account.username, secret])
  const filled = account.server_url !== '' && account.username !== '' && (readOnly || secret !== '')
  const canTest = filled && urlCheck === 'ok'
  const stale = (run.phase === 'done' || run.phase === 'error') && probedFor !== fingerprint
  const passMode: PassMode | null =
    run.phase === 'done' && run.probe.ok ? passModeOf(run.probe.vault) : null

  const startProbe = async () => {
    if (!canTest) {
      setUrlTouched(true)
      return
    }
    const id = ++seq.current
    setRun({ phase: 'running' })
    setProbedFor(fingerprint)
    setPassError(null)
    setConnectError(null)
    try {
      const probe = await api.syncProbe(account, passwordArg)
      if (seq.current !== id) return
      // 服务器状态变了（例如清除之后变成「没有数据」）：同步密码一节换了样子，已填的内容不再适用
      const next = probe.ok ? passModeOf(probe.vault) : null
      if (next !== passMode) {
        setPass('')
        setConfirm('')
        setGenerated(null)
        setPassTouched(false)
      }
      setRun({ phase: 'done', probe })
    } catch (error) {
      if (seq.current === id) setRun({ phase: 'error', message: ipcErrorText(error, 'sync_probe') })
    }
  }

  // 测试通过、同步密码一节出现：焦点直接给同步密码（下一步就是它）。它在「同步内容」下面、常在视野外：展开的同时
  // 跟随滚动露出它（lib/reveal.ts），不等弹簧结束再滚；减少动效时直接到位后滚入视野
  const lastMode = useRef<PassMode | null>(null)
  const passClip = useRef<HTMLDivElement>(null)
  const revealing = useRef(false)
  useEffect(() => {
    if (passMode === lastMode.current) return
    lastMode.current = passMode
    if (!passMode) return
    revealing.current = true
    if (passMode !== 'unsupported') passInput.current?.focus({ preventScroll: true })
  }, [passMode])
  // 同步密码不对：把错误与下面的「忘记了？」一起滚入视野（它们常被粘性底栏挡住）
  useEffect(() => {
    if (!passError) return
    passClip.current
      ?.querySelector('[data-testid="sync-forgot-row"]')
      ?.scrollIntoView?.({ block: 'nearest', behavior: reduced ? 'auto' : 'smooth' })
  }, [passError, reduced])
  const revealDone = () => {
    if (!revealing.current) return
    revealing.current = false
    if (reduced) passClip.current?.scrollIntoView?.({ block: 'nearest' })
  }

  const issue =
    passMode === 'create' ? passphraseIssue(pass, { password: secret, username, url }) : null
  const mismatch =
    passMode === 'create' && normalizePassphrase(confirm) !== normalizePassphrase(pass)
  /** 输入已有的同步密码时还没填：点「连接」或回车之后就地说明，而不是什么都不发生 */
  const passMissing = (passMode === 'join' || passMode === 'restore') && pass === ''
  const passOk =
    passMode === 'create'
      ? issue === null && !mismatch
      : passMode === 'join' || passMode === 'restore'
        ? !passMissing
        : false
  const ready = passMode !== null && passMode !== 'unsupported' && !stale

  const connect = async () => {
    setPassTouched(true)
    if (ready && passMissing) {
      passInput.current?.focus()
      return
    }
    if (!ready || !passOk || connecting || resetting) return
    setConnecting(true)
    setPassError(null)
    setConnectError(null)
    try {
      const result = await trackWrite(
        api.syncConnect(
          account,
          passwordArg,
          normalizePassphrase(pass),
          (readOnly ? saved?.content : content) ?? DEFAULT_CONTENT,
        ),
      )
      setSync(result.status)
      await refreshSettings()
      onConnected(result.outcome)
    } catch (error) {
      const e = WispIpcError.from('sync_connect', error)
      const text = ipcErrorText(e, 'sync_connect')
      if (e.message.startsWith('errors.sync.passphrase')) setPassError(text)
      else setConnectError(text)
    } finally {
      setConnecting(false)
    }
  }

  const generate = async () => {
    try {
      const value = await api.syncGeneratePassphrase()
      setPass(value)
      setPassError(null)
      setConfirm(value)
      setGenerated(value)
      setReveal(true)
    } catch (error) {
      notify(ipcErrorText(error, 'sync_generate_passphrase'), 'danger')
    }
  }

  /**
   * 忘记了同步密码：清除服务器上的数据（所有设备随之停止同步），再测一次——同步密码一节变成「设置」。清除本机正在同步的
   * 服务器时 Rust 留着刚用过的应用密码，所以「更新密码…」里应用密码留空也能接着测试、连接
   */
  const resetRemote = async () => {
    setResetting(true)
    setPassError(null)
    setConnectError(null)
    try {
      await trackWrite(api.syncResetRemote(account, passwordArg))
      await refreshSettings()
      await startProbe()
    } catch (error) {
      setConnectError(ipcErrorText(error, 'sync_reset_remote'))
    } finally {
      setResetting(false)
    }
  }

  const choosePreset = (id: SyncPresetId) => {
    setPreset(id)
    const next = urlForPreset(id, url)
    setUrl(next)
    // 没有固定地址的预设：直接去填地址
    if (!presetById(id).url) requestAnimationFrame(() => urlInput.current?.focus())
  }

  const dirty =
    account.server_url !== initial.url.trim() ||
    account.username !== initial.username.trim() ||
    secret !== '' ||
    pass !== '' ||
    JSON.stringify(content) !== JSON.stringify(initial.content)
  const requestBack = () => {
    if (dirty && !connecting) setConfirmDiscard(true)
    else onBack()
  }
  useKeyScope('sync-connect', [
    {
      combo: 'Escape',
      run: (_e, ctx) => {
        if (ctx.inInput) {
          const el = document.activeElement
          if (el instanceof HTMLElement) el.blur()
          return true
        }
        requestBack()
        return true
      },
    },
  ])
  useReselect(requestBack)
  useConfirmKeys(confirmDiscard, () => setConfirmDiscard(false), {
    cancel: discardCancel,
    container: footRef,
  })

  const title = readOnly
    ? t('settings.sync.connect.title_update')
    : t('settings.sync.connect.title')
  useSubviewTitle(title)

  // http 的公网地址立即提示（「测试连接」因此不可点，得说明为什么；逐字输入 https:// 的过程中不会出现这种状态）；
  // 其余格式问题等失焦或测试之后再说，输入中不打扰
  const showUrlIssue =
    urlCheck === 'https_required' ||
    ((urlTouched || run.phase !== 'idle') && urlCheck === 'invalid')
  const presetInfo = preset ? presetById(preset) : null
  const urlPlaceholder =
    presetInfo?.url ??
    t(
      `settings.sync.connect.url_placeholder.${preset === 'nextcloud' || preset === 'synology' ? preset : 'custom'}`,
    )

  const passField = (
    <PassInput
      inputRef={passInput}
      value={pass}
      onChange={(v) => {
        setPass(v)
        setPassError(null)
      }}
      onEnter={() => void connect()}
      onBlur={() => pass !== '' && setPassTouched(true)}
      reveal={reveal}
      onReveal={passMode === 'create' ? () => setReveal((r) => !r) : null}
      invalid={passError !== null || (passTouched && (issue !== null || passMissing))}
      label={t('settings.sync.connect.pass.field')}
      testId="sync-pass"
    />
  )

  let passSection: ReactNode = null
  if (passMode === 'create') {
    passSection = (
      <Section
        title={t('settings.sync.connect.pass.create_title')}
        description={t('settings.sync.connect.pass.create_desc')}
        action={
          <Button
            size="sm"
            variant="ghost"
            icon={Sparkles}
            onClick={() => void generate()}
            data-testid="sync-pass-generate"
          >
            {t('settings.sync.connect.pass.generate')}
          </Button>
        }
        data-testid="sync-pass-section"
        className="st-sync-pass"
      >
        <Field label={t('settings.sync.connect.pass.field')} stacked>
          {passField}
          {/* Rust 拒绝了这个同步密码（例如另一台设备抢先建好了同步数据）：说明原因，盖过本机的规则提示 */}
          {passError ? (
            <p className="st-field-error" role="alert" data-testid="sync-pass-error">
              {passError}
            </p>
          ) : passTouched && issue ? (
            <p className="st-field-error" role="alert" data-testid="sync-pass-issue">
              {t(`settings.sync.connect.pass.${issue}`)}
            </p>
          ) : generated !== null && generated === pass ? (
            <m.p
              className="st-field-hint"
              data-tone="success"
              data-testid="sync-pass-generated"
              {...swap}
            >
              {t('settings.sync.connect.pass.generated')}
            </m.p>
          ) : (
            <p className="st-field-hint">{t('settings.sync.connect.pass.rules')}</p>
          )}
        </Field>
        <Field label={t('settings.sync.connect.pass.confirm')} stacked>
          <TextField
            type={reveal ? 'text' : 'password'}
            value={confirm}
            onChange={setConfirm}
            onEnter={() => void connect()}
            aria-label={t('settings.sync.connect.pass.confirm')}
            invalid={passTouched && mismatch && confirm !== ''}
            spellCheck={false}
            className="st-mono"
            data-testid="sync-pass-confirm"
          />
          {passTouched && mismatch && pass !== '' ? (
            <p className="st-field-error" role="alert" data-testid="sync-pass-mismatch">
              {t('settings.sync.connect.pass.mismatch')}
            </p>
          ) : null}
        </Field>
        <p className="st-sync-note" data-tone="warning">
          <Icon icon={ShieldAlert} size={14} />
          <span>{t('settings.sync.connect.pass.forget')}</span>
        </p>
        {os === 'macos' ? <KeychainHint /> : null}
      </Section>
    )
  } else if (passMode === 'join' || passMode === 'restore') {
    const join = passMode === 'join'
    passSection = (
      <Section
        title={
          join
            ? t('settings.sync.connect.pass.join_title')
            : t('settings.sync.connect.pass.restore_title')
        }
        description={
          join
            ? t('settings.sync.connect.pass.join_desc')
            : t('settings.sync.connect.pass.restore_desc')
        }
        data-testid="sync-pass-section"
        className="st-sync-pass"
      >
        <Field label={t('settings.sync.connect.pass.field')} stacked>
          {passField}
          {passError || (passTouched && passMissing) ? (
            <p className="st-field-error" role="alert" data-testid="sync-pass-error">
              {passError ?? t('settings.sync.connect.pass.required')}
            </p>
          ) : null}
        </Field>
        {resetting ? (
          <Field
            label={t('settings.sync.connect.pass.forgot')}
            description={t('settings.sync.connect.pass.forgot_desc')}
          >
            <Button size="sm" loading className="st-btn-danger">
              {t('settings.sync.connect.pass.forgot_action')}
            </Button>
          </Field>
        ) : (
          <DangerRow
            label={t('settings.sync.connect.pass.forgot')}
            description={t('settings.sync.connect.pass.forgot_desc')}
            action={t('settings.sync.connect.pass.forgot_action')}
            confirm={t('settings.sync.connect.pass.forgot_confirm')}
            confirmLabel={t('settings.sync.danger.reset_action')}
            icon={Trash2}
            onConfirm={() => void resetRemote()}
            disabled={connecting}
            data-testid="sync-forgot"
          />
        )}
        {os === 'macos' ? <KeychainHint /> : null}
      </Section>
    )
  } else if (passMode === 'unsupported') {
    passSection = (
      <Section data-testid="sync-pass-section">
        <p className="st-sync-note" data-tone="warning" role="alert">
          <Icon icon={ShieldAlert} size={14} />
          <span>{t('settings.sync.connect.pass.unsupported')}</span>
        </p>
      </Section>
    )
  }

  const host = urlCheck === 'ok' ? hostOf(url) : null
  const needTest = !ready && passMode !== 'unsupported'

  return (
    <div className="st-editor" data-testid="sync-connect-view" data-mode={mode}>
      <div className="st-editor-head">
        <IconButton
          icon={readOnly ? ArrowLeft : X}
          label={readOnly ? t('settings.sync.connect.back') : t('common.action.cancel')}
          onClick={requestBack}
          data-testid="sync-connect-back"
        />
        <span className="st-logo" data-tone="accent" aria-hidden="true">
          <Icon icon={Cloud} size={16} />
        </span>
        <span className="st-editor-title">
          <h2 tabIndex={-1} data-subview-title>
            {title}
          </h2>
          <span>{host ?? t('settings.sync.connect.subtitle')}</span>
        </span>
      </div>

      <Section title={t('settings.sync.connect.server')}>
        {readOnly ? null : (
          <Field
            label={t('settings.sync.connect.preset')}
            description={
              preset
                ? t(`settings.sync.connect.preset_hint.${preset}`, { package: SYNOLOGY_PACKAGE })
                : undefined
            }
          >
            <Select<SyncPresetId>
              label={t('settings.sync.connect.preset')}
              value={preset}
              placeholder={t('settings.sync.connect.preset_placeholder')}
              options={SYNC_PRESETS.map((p) => ({
                value: p.id,
                label: t(`settings.sync.connect.preset_option.${p.id}`),
              }))}
              onChange={choosePreset}
              className="st-select"
              data-testid="sync-preset"
            />
          </Field>
        )}
        <Field label={t('settings.sync.connect.url')} stacked={!readOnly}>
          {readOnly ? (
            <span className="st-mono st-sync-readonly" data-testid="sync-url-readonly">
              {account.server_url}
            </span>
          ) : (
            <>
              <TextField
                ref={urlInput}
                value={url}
                onChange={(v) => {
                  setUrl(v)
                  setPreset((p) =>
                    p && presetById(p).url && presetById(p).url !== v.trim() ? 'custom' : p,
                  )
                }}
                onBlur={() => url.trim() !== '' && setUrlTouched(true)}
                onEnter={() => void startProbe()}
                placeholder={urlPlaceholder}
                aria-label={t('settings.sync.connect.url')}
                invalid={showUrlIssue}
                spellCheck={false}
                className="st-mono"
                data-testid="sync-url"
              />
              {showUrlIssue ? (
                <p className="st-field-error" role="alert" data-testid="sync-url-issue">
                  {urlCheck === 'https_required'
                    ? t('settings.sync.connect.url_https')
                    : t('settings.sync.connect.url_invalid')}
                </p>
              ) : null}
            </>
          )}
        </Field>
        <Field label={t('settings.sync.connect.username')} stacked={!readOnly}>
          {readOnly ? (
            <span className="st-sync-readonly" data-testid="sync-username-readonly">
              {account.username}
            </span>
          ) : (
            <TextField
              value={username}
              onChange={setUsername}
              onEnter={() => void startProbe()}
              placeholder={t('settings.sync.connect.username_placeholder')}
              aria-label={t('settings.sync.connect.username')}
              spellCheck={false}
              data-testid="sync-username"
            />
          )}
        </Field>
        <Field
          label={t('settings.sync.connect.password')}
          description={t(`settings.sync.connect.password_saved.${os}`)}
          stacked
        >
          <TextField
            ref={passwordInput}
            type="password"
            value={password}
            onChange={setPassword}
            onEnter={() => void startProbe()}
            placeholder={
              readOnly
                ? t('settings.sync.connect.password_keep')
                : t('settings.sync.connect.password_placeholder')
            }
            aria-label={t('settings.sync.connect.password')}
            leading={<Icon icon={KeyRound} size={14} />}
            className="st-mono"
            data-testid="sync-password"
          />
        </Field>
        <Field
          label={t('settings.providers.test')}
          description={t('settings.sync.connect.test_desc')}
          stacked
        >
          <div className="st-test-bar">
            <Button
              size="sm"
              variant={passMode === null || stale ? 'primary' : 'secondary'}
              icon={RefreshCw}
              // 进行中由下面报告的三步转圈表示，按钮只换成「测试中…」并暂不可点（不再叠一个转圈）
              // 地址格式不对时仍可点：点了才就地说明原因（http 的公网地址已经在输入框下提示，不可点）
              disabled={!filled || urlCheck === 'https_required' || run.phase === 'running'}
              onClick={() => void startProbe()}
              data-testid="sync-test"
            >
              {run.phase === 'running'
                ? t('settings.providers.testing')
                : run.phase === 'idle'
                  ? t('settings.providers.test_run')
                  : t('settings.providers.test_again')}
            </Button>
          </div>
          <ProbeReport run={run} stale={stale} now={now} onRetry={() => void startProbe()} />
        </Field>
      </Section>

      {readOnly || !content ? null : (
        <Section
          title={t('settings.sync.content.title')}
          description={t('settings.sync.connect.content_desc')}
        >
          <ContentRows
            content={content}
            onChange={(key, value) => setContent({ ...content, [key]: value })}
            ocrHistoryOn={settings?.ocr.history ?? false}
            connected={false}
          />
        </Section>
      )}

      <AnimatePresence initial={false}>
        {passSection ? (
          <m.div
            key={passMode}
            ref={passClip}
            className="st-clip st-sync-pass-clip"
            {...expand}
            onUpdate={(latest) => {
              if (revealing.current)
                revealGrowing(passClip.current, latest.height, passClip.current)
            }}
            onAnimationComplete={revealDone}
          >
            {passSection}
          </m.div>
        ) : null}
      </AnimatePresence>

      <div
        ref={footRef}
        className="st-editor-foot"
        data-confirm={confirmDiscard || undefined}
        data-testid="sync-connect-foot"
      >
        {confirmDiscard ? (
          <m.span
            key="discard"
            className="st-foot-confirm"
            role="alert"
            data-testid="sync-discard-confirm"
            {...fade}
          >
            <span className="st-foot-confirm-text">
              {t('settings.sync.connect.discard_confirm')}
            </span>
            <Button ref={discardCancel} onClick={() => setConfirmDiscard(false)}>
              {t('settings.providers.keep_editing')}
            </Button>
            <Button variant="danger" onClick={onBack} data-testid="sync-discard-ok">
              {t('settings.providers.discard')}
            </Button>
          </m.span>
        ) : (
          <>
            <Button variant="ghost" onClick={requestBack} data-testid="sync-connect-cancel">
              {t('common.action.cancel')}
            </Button>
            <span
              className="st-grow st-sync-foot-note"
              data-tone={connectError ? 'danger' : undefined}
              role={connectError ? 'alert' : undefined}
              data-testid="sync-connect-note"
            >
              {connectError ?? (needTest ? t('settings.sync.connect.test_needed') : null)}
            </span>
            <Button
              variant="primary"
              size="md"
              loading={connecting}
              disabled={!ready || resetting}
              onClick={() => void connect()}
              data-testid="sync-connect"
            >
              {passMode === 'restore'
                ? t('settings.sync.connect.reconnect')
                : t('settings.sync.connect.connect')}
            </Button>
          </>
        )}
      </div>
    </div>
  )
}

const DEFAULT_CONTENT: SyncContent = {
  settings: true,
  api_keys: true,
  clipboard: true,
  images: true,
  ocr_history: true,
  verification_codes: true,
}

/** 同步密码输入框：设置时可以显示明文（生成的密码需要看清、存进密码管理器） */
function PassInput({
  inputRef,
  value,
  onChange,
  onEnter,
  onBlur,
  reveal,
  onReveal,
  invalid,
  label,
  testId,
}: {
  inputRef: RefObject<HTMLInputElement | null>
  value: string
  onChange: (v: string) => void
  onEnter: () => void
  onBlur: () => void
  reveal: boolean
  /** null：不提供显示明文（输入已有的同步密码时） */
  onReveal: (() => void) | null
  invalid: boolean
  label: string
  testId: string
}) {
  const t = useT()
  return (
    <TextField
      ref={inputRef}
      type={reveal && onReveal ? 'text' : 'password'}
      value={value}
      onChange={onChange}
      onEnter={onEnter}
      onBlur={onBlur}
      aria-label={label}
      invalid={invalid}
      spellCheck={false}
      className="st-mono"
      leading={<Icon icon={KeyRound} size={14} />}
      trailing={
        onReveal ? (
          <IconButton
            icon={reveal ? EyeOff : Eye}
            size="sm"
            label={
              reveal ? t('settings.sync.connect.pass.hide') : t('settings.sync.connect.pass.show')
            }
            onClick={onReveal}
            data-testid="sync-pass-reveal"
          />
        ) : undefined
      }
      data-testid={testId}
    />
  )
}

function KeychainHint() {
  const t = useT()
  return (
    <p className="st-sync-note" data-testid="sync-keychain-hint">
      <Icon icon={KeyRound} size={14} />
      <span>{t('settings.sync.connect.pass.keychain_hint')}</span>
    </p>
  )
}
