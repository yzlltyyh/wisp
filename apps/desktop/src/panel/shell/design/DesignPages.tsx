/**
 * 高保真视觉稿：设置一页（模型与服务）、引导一步（权限）。只在浏览器 mock 中加载。
 *
 * 画在假桌面上的一个模拟窗口里（macOS：隐藏标题栏 + 交通灯 + 侧栏材质；Windows：自绘标题栏与标题按钮）。
 * 是设置与引导窗口的视觉基准：侧栏、分组卡片、字段行、按钮层级与间距以这里为准。
 *
 * i18n-ignore-file：只在浏览器 mock 中加载的参照页（生产包里没有），说明文字与示例文案是写死的参照；
 * 视图与设置的实现里文案一律走 t()。
 */
import './design.css'
import {
  Accessibility,
  Check,
  ClipboardList,
  Info,
  Keyboard,
  Languages,
  Minus,
  Palette,
  Plus,
  ScanText,
  ScreenShare,
  Server,
  Settings2,
  Square,
  X,
} from 'lucide-react'
import { useLayoutEffect, useState, type ReactNode } from 'react'
import { usePlatform } from '../../../shared/hooks/usePlatform'
import {
  Badge,
  Button,
  Field,
  Icon,
  SegmentedControl,
  Section,
  Select,
  TextField,
} from '../../../shared/ui'

function usePage() {
  useLayoutEffect(() => {
    document.documentElement.dataset.window = 'design-page'
  }, [])
}

function Window({
  width,
  height,
  title,
  children,
}: {
  width: number
  height: number
  title: string
  children: ReactNode
}) {
  const os = usePlatform()
  return (
    <div className="dz-stage">
      <div className="dz-window" style={{ width, height }} data-testid="design-window">
        {os === 'macos' ? (
          <span className="dz-traffic" aria-hidden="true">
            <i />
            <i />
            <i />
          </span>
        ) : (
          <div className="dz-titlebar">
            {title}
            <span className="dz-caption" aria-hidden="true">
              <i>
                <Icon icon={Minus} size={14} />
              </i>
              <i>
                <Icon icon={Square} size={12} />
              </i>
              <i>
                <Icon icon={X} size={14} />
              </i>
            </span>
          </div>
        )}
        {children}
      </div>
    </div>
  )
}

const NAV = [
  ['通用', Settings2],
  ['快捷键', Keyboard],
  ['模型与服务', Server],
  ['翻译', Languages],
  ['OCR', ScanText],
  ['剪贴板', ClipboardList],
  ['外观', Palette],
  ['关于', Info],
] as const

export function SettingsModelsPage() {
  usePage()
  const [protocol, setProtocol] = useState<'chat' | 'responses'>('chat')
  const [url, setUrl] = useState('https://api.openai.com/v1')
  const [translateModel, setTranslateModel] = useState('gpt-5.4-mini')
  const [ocrModel, setOcrModel] = useState('qwen3-vl-plus')
  return (
    <Window width={780} height={720} title="Wisp 设置">
      <div className="dz-body" data-testid="design-settings-models">
        <nav className="dz-sidebar" aria-label="设置分组">
          {NAV.map(([name, icon]) => (
            <div key={name} className="dz-nav" data-active={name === '模型与服务' || undefined}>
              <Icon icon={icon} size={16} />
              {name}
            </div>
          ))}
        </nav>
        <div className="dz-content">
          <h1 className="dz-page-title">模型与服务</h1>
          <p className="dz-page-desc">
            接入 OpenAI、Anthropic、Gemini 或任意兼容端点；Key 保存在系统凭据存储中。
          </p>
          <Section
            title="服务"
            action={
              <Button size="sm" icon={Plus}>
                添加服务
              </Button>
            }
          >
            <div className="dz-provider">
              <span className="dz-provider-logo">O</span>
              <span className="dz-provider-text">
                <b>OpenAI</b>
                <span>api.openai.com · 5 个模型</span>
              </span>
              <Badge tone="success" dot>
                已连接
              </Badge>
            </div>
            <div className="dz-provider">
              <span className="dz-provider-logo" data-tone="neutral">
                D
              </span>
              <span className="dz-provider-text">
                <b>DeepSeek</b>
                <span>api.deepseek.com · 2 个模型</span>
              </span>
              <Badge tone="neutral">未测试</Badge>
            </div>
          </Section>
          <Section title="默认模型">
            <Field label="翻译" description="划词翻译、输入翻译与润色">
              <Select
                label="翻译模型"
                value={translateModel}
                onChange={setTranslateModel}
                options={[
                  { value: 'gpt-5.4-mini', label: 'gpt-5.4-mini', hint: 'OpenAI' },
                  { value: 'deepseek-chat', label: 'deepseek-chat', hint: 'DeepSeek' },
                ]}
              />
            </Field>
            <Field label="OCR" description="需要支持图片输入">
              <Select
                label="OCR 模型"
                value={ocrModel}
                onChange={setOcrModel}
                options={[
                  { value: 'qwen3-vl-plus', label: 'qwen3-vl-plus', hint: 'DashScope' },
                  { value: 'gpt-5.4-mini', label: 'gpt-5.4-mini', hint: 'OpenAI' },
                ]}
              />
            </Field>
          </Section>
          <Section title="OpenAI" description="编辑服务">
            <Field label="协议">
              <SegmentedControl
                label="协议"
                value={protocol}
                onChange={setProtocol}
                options={[
                  { value: 'chat', label: 'Chat Completions' },
                  { value: 'responses', label: 'Responses' },
                ]}
              />
            </Field>
            <Field label="Base URL" stacked>
              <TextField value={url} onChange={setUrl} aria-label="Base URL" />
              <div className="dz-url-preview">→ https://api.openai.com/v1/chat/completions</div>
            </Field>
            <Field label="API Key" description="只写入系统凭据存储，界面不回显">
              <TextField
                value="sk-proj-••••••••••3f2a"
                onChange={() => {}}
                aria-label="API Key"
                style={{ width: 200 }}
              />
              <Button size="sm">测试连接</Button>
            </Field>
            <Field label="连接测试">
              <Badge tone="success" dot>
                连接正常 · 312 ms
              </Badge>
            </Field>
          </Section>
        </div>
      </div>
    </Window>
  )
}

export function OnboardingPermissionsPage() {
  usePage()
  const os = usePlatform()
  return (
    <Window width={620} height={460} title="欢迎使用 Wisp">
      <div className="dz-onb" data-testid="design-onboarding-permissions">
        <div className="dz-stepper" aria-label="引导步骤">
          <span className="dz-step" data-current>
            <b>1</b>权限
          </span>
          <span className="dz-step-line" />
          <span className="dz-step">
            <b>2</b>接入模型
          </span>
          <span className="dz-step-line" />
          <span className="dz-step">
            <b>3</b>试一试
          </span>
        </div>
        <h1 className="dz-page-title">授予两项权限</h1>
        <p className="dz-page-desc">
          Wisp 只在你按下快捷键时读取选中的文字或截图，不会在后台读取屏幕内容。
          {os !== 'macos' ? '（当前平台无需单独授权，此步骤只在 macOS 显示。）' : ''}
        </p>
        <div className="dz-perm" data-granted>
          <span className="dz-perm-icon">
            <Icon icon={Accessibility} size={18} />
          </span>
          <span className="dz-perm-text">
            <b>辅助功能</b>
            <span>读取选中的文字、把剪贴板条目粘贴回原应用</span>
          </span>
          <Badge tone="success">
            <Icon icon={Check} size={12} />
            已授权
          </Badge>
        </div>
        <div className="dz-perm">
          <span className="dz-perm-icon">
            <Icon icon={ScreenShare} size={18} />
          </span>
          <span className="dz-perm-text">
            <b>屏幕录制</b>
            <span>截图 OCR 需要读取屏幕画面</span>
          </span>
          <Button size="sm" variant="primary">
            前往系统设置
          </Button>
        </div>
        <div className="dz-onb-foot">
          <Button variant="ghost">跳过</Button>
          <span className="dz-spacer" />
          <Button variant="primary" size="md">
            下一步
          </Button>
        </div>
      </div>
    </Window>
  )
}
