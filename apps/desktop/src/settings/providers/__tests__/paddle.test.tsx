/**
 * PaddleOCR 服务的编辑器：按地址识别接口类型，公网地址写了 http 时即时提示要用 https；表单精简（没有协议、高级与
 * 「拉取模型列表」）；Key 叫访问令牌，提示不要分享、开启同步时会加密同步到其他设备；模型表只有启用与删除。
 */
import { fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { getAppearance, setAppearance } from '../../../shared/hooks/useAppearance'
import { boot } from '../../../shared/ipc'
import type { Provider } from '../../../shared/ipc/types'
import { entry, installHarness, provider, snapshot } from '../../__tests__/harness'
import { resetWrites } from '../../lib/writes'
import { SettingsApp } from '../../SettingsApp'
import { resetSettingsStore, setQueueDelay } from '../../store'
import { isLocalHost, paddleApiOf, paddleUrlInsecure } from '../paddle'

const appearance = getAppearance()
const paddle: Provider = {
  ...provider('paddleocr'),
  name: 'PaddleOCR',
  protocol: 'paddle_ocr',
  base_url: 'https://paddleocr.aistudio-app.com/api/v2/ocr/jobs',
  models: ['PP-OCRv6', 'PP-StructureV3', 'PaddleOCR-VL-1.6'].map((id) => entry(id, 'unknown')),
}

beforeEach(() => {
  resetSettingsStore()
  resetWrites()
  setQueueDelay(0)
  setAppearance({ ...appearance, os: 'macos', uiLanguage: 'zh-CN' })
})
afterEach(() => {
  boot.route = null
  setAppearance(appearance)
})

describe('服务地址的接口类型', () => {
  it('任务接口 / 部署的服务 / 无法识别', () => {
    expect(paddleApiOf('https://paddleocr.aistudio-app.com/api/v2/ocr/jobs')).toBe('jobs')
    expect(paddleApiOf('https://paddleocr.aistudio-app.com/api/v2/ocr/jobs/')).toBe('jobs')
    expect(paddleApiOf('https://x.aistudio-hub.baidu.com/ocr')).toBe('serving')
    expect(paddleApiOf('http://192.168.1.20:8080/layout-parsing')).toBe('serving')
    expect(paddleApiOf('192.168.1.20:8080/layout-parsing')).toBe('serving')
    expect(paddleApiOf('https://example.com/v1')).toBe('unknown')
    expect(paddleApiOf('  ')).toBe('empty')
  })

  it('http 只允许本机与局域网（与 Rust 的 is_local_host 同一规则）；公网地址要 https', () => {
    for (const host of [
      '127.0.0.1',
      'localhost',
      '192.168.1.5',
      '10.0.0.2',
      '172.20.1.1',
      '169.254.3.4',
      '[::1]',
      '[fd12::1]',
      '[fe80::1]',
      'nas',
      'nas.',
      'paddle.local',
      'box.home.arpa',
      'ocr.lan',
    ])
      expect(isLocalHost(host), host).toBe(true)
    for (const host of ['8.8.8.8', '172.32.0.1', 'example.com', '[2001:db8::1]'])
      expect(isLocalHost(host), host).toBe(false)
    expect(paddleUrlInsecure('http://ocr.example.com/ocr')).toBe(true)
    expect(paddleUrlInsecure('http://192.168.1.20:8080/layout-parsing')).toBe(false)
    expect(paddleUrlInsecure('https://ocr.example.com/ocr')).toBe(false)
    // 没写协议按 https
    expect(paddleUrlInsecure('ocr.example.com/ocr')).toBe(false)
  })
})

describe('编辑器', () => {
  it('没有协议与高级、没有「拉取模型列表」；访问令牌的风险提示写明会随同步加密同步；模型表没有图片 / 推理两列', async () => {
    installHarness(snapshot({ providers: [paddle] }, { has_key: { paddleocr: true } }))
    boot.route = { kind: 'settings', section: 'providers' }
    render(<SettingsApp />)
    fireEvent.click(await screen.findByTestId('provider-row-paddleocr'))
    const editor = await screen.findByTestId('provider-editor')
    expect(within(editor).queryByTestId('provider-protocol')).toBeNull()
    expect(within(editor).queryByTestId('provider-advanced')).toBeNull()
    expect(within(editor).queryByTestId('provider-fetch-models')).toBeNull()
    expect(within(editor).getByTestId('paddle-url-kind')).toHaveAttribute('data-kind', 'jobs')
    expect(editor).toHaveTextContent('访问令牌')
    expect(within(editor).getByTestId('paddle-token-risk')).toHaveTextContent('不要分享给他人')
    expect(within(editor).getByTestId('paddle-token-risk')).toHaveTextContent('端到端加密')
    // 改成公网的 http 地址：地址下方即时提示要用 https
    fireEvent.change(within(editor).getByTestId('provider-base-url'), {
      target: { value: 'http://ocr.example.com/ocr' },
    })
    expect(within(editor).getByTestId('paddle-url-kind')).toHaveAttribute('data-kind', 'insecure')
    expect(within(editor).getByTestId('paddle-url-kind')).toHaveTextContent('公网地址需要 https')
    // 模型表没有「图片」「推理」两列（行本身是虚拟列表，jsdom 里不排版；较慢的说明见 mock 场景的截图）
    expect(within(editor).getByTestId('model-table')).not.toHaveTextContent('推理')
  })
})
