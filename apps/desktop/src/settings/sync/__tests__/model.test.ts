/**
 * 同步页的纯逻辑：地址规则（与 Rust 的账号校验一致：http 只给局域网地址）、预设、同步密码规则（NFC、码点计数、
 * 不与应用密码 / 账号 / 主机名相同）、状态行归类、错误对应的操作与测试报告的分步归属；以及 `errors.sync.*` 能被翻译。
 */
import { describe, expect, it } from 'vitest'
import { WispIpcError } from '../../../shared/ipc'
import type { SyncStatus } from '../../../shared/ipc/types'
import { ipcErrorText, isKeyLike } from '../../lib/errors'
import { loadSettingsMessages } from '../../i18n'
import {
  checkServerUrl,
  errorAction,
  failedStep,
  hostOf,
  isLocalHost,
  normalizePassphrase,
  passModeOf,
  passphraseChars,
  passphraseIssue,
  presetFor,
  progressRatio,
  statusView,
  urlForPreset,
} from '../model'

loadSettingsMessages()

const idle: SyncStatus = {
  phase: 'idle',
  last_ok_ms: null,
  pending: false,
  error: null,
  transient: false,
  error_detail: null,
  progress: null,
  device_count: 0,
}

describe('服务器地址', () => {
  it.each([
    ['', 'empty'],
    ['   ', 'empty'],
    ['https://dav.jianguoyun.com/dav/', 'ok'],
    ['https://cloud.example.com:8443/remote.php/dav/files/me/', 'ok'],
    ['http://192.168.1.20:5005/', 'ok'],
    ['http://10.0.0.2/dav', 'ok'],
    ['http://172.20.1.1/', 'ok'],
    ['http://127.0.0.1:8080/', 'ok'],
    ['http://169.254.10.1/', 'ok'],
    ['http://nas.local/', 'ok'],
    ['http://nas:5005/', 'ok'],
    ['http://localhost/', 'ok'],
    ['http://[::1]:8080/', 'ok'],
    ['http://[fd12:3456::1]/', 'ok'],
    ['http://[fe80::1]/', 'ok'],
    ['http://dav.example.com/', 'https_required'],
    ['http://172.32.0.1/', 'https_required'],
    ['http://8.8.8.8/', 'https_required'],
    ['http://[2001:db8::1]/', 'https_required'],
    ['ftp://dav.example.com/', 'invalid'],
    ['dav.example.com', 'invalid'],
    ['https://user:pw@dav.example.com/', 'invalid'],
    ['https://dav.example.com/?', 'invalid'],
    ['https://dav.example.com/dav?x=1', 'invalid'],
    ['https://dav.example.com/#a', 'invalid'],
  ] as const)('%s → %s', (url, want) => {
    expect(checkServerUrl(url)).toBe(want)
  })

  it('超过 1 KB 的地址无效', () => {
    expect(checkServerUrl(`https://dav.example.com/${'a'.repeat(1024)}`)).toBe('invalid')
  })

  it('局域网判断按规范化之后的主机名', () => {
    expect(isLocalHost('192.168.0.1')).toBe(true)
    expect(isLocalHost('192.169.0.1')).toBe(false)
    expect(isLocalHost('[::]')).toBe(false)
    expect(isLocalHost('NAS.LOCAL')).toBe(true)
  })

  it('主机含端口；解析不了时原样返回', () => {
    expect(hostOf('https://nas.local:5006/dav/')).toBe('nas.local:5006')
    expect(hostOf(' not a url ')).toBe('not a url')
  })
})

describe('服务预设', () => {
  it('按地址认出预设；还没填时不预选', () => {
    expect(presetFor('')).toBeNull()
    expect(presetFor('https://dav.jianguoyun.com/dav/')).toBe('nutstore')
    expect(presetFor('https://webdav.123pan.cn/webdav')).toBe('pan123')
    expect(presetFor('https://cloud.example.com/remote.php/dav/files/me/')).toBe('nextcloud')
    expect(presetFor('https://nas.local:5006/')).toBe('synology')
    expect(presetFor('https://dav.example.com/')).toBe('custom')
  })

  it('固定地址的预设直接填；换到其他预设时只清掉别的预设的固定地址', () => {
    expect(urlForPreset('nutstore', '')).toBe('https://dav.jianguoyun.com/dav/')
    expect(urlForPreset('nextcloud', 'https://dav.jianguoyun.com/dav/')).toBe('')
    expect(urlForPreset('custom', 'https://my.example.com/dav')).toBe('https://my.example.com/dav')
  })
})

describe('同步密码', () => {
  it('交给 Rust 之前做 NFC；字符数按码点', () => {
    // 分解形式（e + 组合重音）13 个码点，规范化之后 11 个：按规范化之后计
    const accent = String.fromCodePoint(0x301)
    const decomposed = `cafe${accent}-cafe${accent}-x`
    expect([...decomposed]).toHaveLength(13)
    expect(normalizePassphrase(decomposed)).toBe(
      `caf${String.fromCodePoint(0xe9)}-caf${String.fromCodePoint(0xe9)}-x`,
    )
    expect(passphraseChars(decomposed)).toBe(11)
    expect(passphraseChars('😀'.repeat(12))).toBe(12)
  })

  it('至少 12 个字符；不能与应用密码、账号、主机名相同（忽略大小写）', () => {
    const ctx = {
      password: 'App-Password-123',
      username: 'Me@Example.com',
      url: 'https://Long-Host-Name.example/',
    }
    expect(passphraseIssue('short-one', ctx)).toBe('short')
    expect(passphraseIssue('app-password-123', ctx)).toBe('reused')
    expect(passphraseIssue('me@example.com', ctx)).toBe('reused')
    expect(passphraseIssue('long-host-name.example', ctx)).toBe('reused')
    expect(passphraseIssue('ABCD-EFGH-IJKL-MNOP', ctx)).toBeNull()
  })

  it('与 Rust 一样不计首尾空白：首尾的空格补不够字数，也绕不过「不能相同」', () => {
    const ctx = { password: 'app-password-0000', username: 'me', url: 'https://host.example/' }
    expect(passphraseChars('abcdefghijk ')).toBe(11)
    expect(passphraseIssue('abcdefghijk ', ctx)).toBe('short')
    expect(passphraseIssue('  App-Password-0000 ', ctx)).toBe('reused')
    expect(
      passphraseIssue(' Long-Host.Example ', { ...ctx, url: 'https://long-host.example/' }),
    ).toBe('reused')
  })

  it('同步密码一节按服务器状态变形', () => {
    expect(passModeOf(null)).toBe('create')
    const vault = { supported: true, device_count: 2, last_modified_ms: null, restorable: false }
    expect(passModeOf(vault)).toBe('join')
    expect(passModeOf({ ...vault, restorable: true })).toBe('restore')
    expect(passModeOf({ ...vault, supported: false })).toBe('unsupported')
  })
})

describe('状态行', () => {
  it('同步中优先；有总数时给出比例', () => {
    const view = statusView({ ...idle, phase: 'syncing', progress: { done: 120, total: 1000 } })
    expect(view).toEqual({ kind: 'syncing', progress: { done: 120, total: 1000 } })
    expect(progressRatio({ done: 120, total: 1000 })).toBeCloseTo(0.12)
    expect(progressRatio({ done: 5, total: 0 })).toBeNull()
    expect(progressRatio({ done: 9, total: 3 })).toBe(1)
  })

  it('等待上传 / 已同步 / 尚未同步', () => {
    expect(statusView({ ...idle, pending: true, last_ok_ms: 1 }).kind).toBe('pending')
    expect(statusView({ ...idle, last_ok_ms: 5 })).toEqual({ kind: 'synced', at: 5 })
    expect(statusView(idle).kind).toBe('never')
    expect(statusView({ ...idle, phase: 'off' }).kind).toBe('never')
  })

  it('暂时性的网络 / 服务器失败不标红；其余带上种类与是否暂时', () => {
    expect(statusView({ ...idle, phase: 'error', error: 'network', transient: true }).kind).toBe(
      'offline',
    )
    expect(statusView({ ...idle, phase: 'error', error: 'server', transient: true }).kind).toBe(
      'offline',
    )
    expect(statusView({ ...idle, phase: 'error', error: 'auth', transient: false })).toEqual({
      kind: 'problem',
      error: 'auth',
      transient: false,
    })
    expect(statusView({ ...idle, phase: 'error', error: null })).toEqual({
      kind: 'problem',
      error: 'internal',
      transient: false,
    })
  })

  it('需要用户处理的错误对应的操作', () => {
    expect(errorAction('auth')).toBe('update')
    expect(errorAction('passphrase')).toBe('update')
    expect(errorAction('credentials_missing')).toBe('update')
    expect(errorAction('vault_reset')).toBe('reconnect')
    expect(errorAction('network')).toBeNull()
    expect(errorAction('quota')).toBeNull()
  })

  it('测试报告里失败落在哪一步', () => {
    expect(failedStep('auth')).toBe(0)
    expect(failedStep('network')).toBe(0)
    expect(failedStep('forbidden')).toBe(1)
    expect(failedStep('quota')).toBe(1)
    expect(failedStep('format_too_new')).toBe(2)
  })
})

describe('同步命令的错误消息', () => {
  it.each([
    'errors.sync.passphrase',
    'errors.sync.passphrase_weak',
    'errors.sync.passphrase_reused',
    'errors.sync.https_required',
    'errors.sync.invalid_url',
    'errors.sync.not_connected',
    'errors.sync.busy',
    'errors.sync.credentials_missing',
  ])('%s 被当作文案 key 翻译', (key) => {
    expect(isKeyLike(key)).toBe(true)
    const text = ipcErrorText(
      new WispIpcError('sync_connect', { code: 'conflict', message: key, llm: null }),
    )
    expect(text).not.toBe(key)
    expect(text).not.toMatch(/errors\.sync/)
  })
})
