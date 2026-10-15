/**
 * 组件展示页（`?scenario=shell.gallery`，只在浏览器 mock 中加载）：
 * 设计令牌、全部组件 × 全部状态（悬停 / 按下 / 焦点用 `data-force` 冻结）、每个动效 preset 的演示（可重播），
 * 以及 panelEnter 三种 placement 的中间帧（冻结在 t = 60 ms，红点为变换原点）。
 * 各视图的视觉评审以这里和 `design.*` 为准。
 *
 * i18n-ignore-file：只在浏览器 mock 中加载的参照页（生产包里没有），说明文字与示例文案是写死的参照；
 * 视图与设置的实现里文案一律走 t()。
 */
import './gallery.css'
import {
  ArrowLeftRight,
  Bold,
  ClipboardList,
  Copy,
  Ellipsis,
  Maximize2,
  Pin,
  Plus,
  RotateCw,
  Settings,
  Sparkles,
  Trash2,
  X,
} from 'lucide-react'
import { AnimatePresence, m } from 'motion/react'
import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react'
import { boot } from '../../../shared/ipc'
import type { ErrorCode, LlmError, NotReady } from '../../../shared/ipc/types'
import { useT } from '../../../shared/i18n/useT'
import { adaptPreset, presets, usePreset, type PresetName } from '../../../shared/motion/presets'
import { duration, ease, spring } from '../../../shared/motion/tokens'
import { useReducedMotionPref } from '../../../shared/motion/useReducedMotionPref'
import { StreamView, type StreamHandle } from '../../../shared/stream/StreamView'
import {
  Badge,
  Banner,
  Button,
  Checkbox,
  Chip,
  CopyButton,
  CursorPulse,
  Divider,
  DropdownMenu,
  EmptyState,
  ErrorCard,
  Field,
  Highlight,
  HotkeyRecorder,
  IconButton,
  InlineConfirm,
  InlineToast,
  Kbd,
  LangBar,
  ListRow,
  Menu,
  NotReadyBanner,
  NotReadyCard,
  NumberField,
  RadioGroup,
  ScrollArea,
  SearchField,
  Section,
  SegmentedControl,
  Select,
  Skeleton,
  Slider,
  Spinner,
  StatusLine,
  StyleChip,
  Switch,
  TextArea,
  TextField,
  Tooltip,
  VirtualList,
  type MenuEntry,
} from '../../../shared/ui'
import { LEARN_SAMPLE, EDIT_SAMPLE } from '../design/samples'

// ---------------------------------------------------------------------------
// 版式小件

function GSection({
  id,
  title,
  desc,
  children,
}: {
  id: string
  title: string
  desc?: string
  children: ReactNode
}) {
  return (
    <section className="gal-section" id={id} data-testid={`gallery-${id}`}>
      <h2>{title}</h2>
      {desc ? <p>{desc}</p> : <p />}
      {children}
    </section>
  )
}

function Spec({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="gal-spec">
      <span className="gal-spec-label">{label}</span>
      {children}
    </div>
  )
}

function State({ name, children }: { name: string; children: ReactNode }) {
  return (
    <div className="gal-state">
      {children}
      <span>{name}</span>
    </div>
  )
}

const STATES = [
  ['default', undefined],
  ['hover', 'hover'],
  ['pressed', 'active'],
  ['focus', 'focus'],
] as const

// ---------------------------------------------------------------------------

function useGalleryPage() {
  useLayoutEffect(() => {
    document.documentElement.dataset.window = 'gallery'
  }, [])
}

export function Gallery() {
  useGalleryPage()
  const t = useT()
  return (
    <main className="gal" data-testid="gallery">
      <header className="gal-head">
        <div>
          <h1 className="gal-title">Wisp · 组件与动效</h1>
          <p className="gal-sub">
            设计令牌、组件库全部状态与动效预设。视图实现以此页与 design.* 视觉稿为准。
          </p>
        </div>
        <div className="gal-env">
          <Badge tone="neutral">{boot.os}</Badge>
          <Badge tone="neutral">{boot.theme}</Badge>
          <Badge tone="neutral">frame: {boot.frame_mode}</Badge>
          <Badge tone={boot.reduce_motion ? 'warning' : 'neutral'}>
            reduced: {String(boot.reduce_motion)}
          </Badge>
          <Badge tone="neutral">{boot.ui_language}</Badge>
        </div>
      </header>
      <nav className="gal-nav" aria-label="sections">
        {[
          ['tokens', '令牌'],
          ['buttons', '按钮'],
          ['selection', '选择'],
          ['inputs', '输入'],
          ['overlays', '浮层'],
          ['feedback', '反馈'],
          ['errors', '错误与未就绪'],
          ['lists', '列表'],
          ['settings', '设置布局'],
          ['stream', '流式文字'],
          ['motion', '动效'],
        ].map(([id, name]) => (
          <a key={id} href={`#${id}`}>
            {name}
          </a>
        ))}
      </nav>
      <Tokens />
      <Buttons />
      <SelectionControls />
      <Inputs />
      <Overlays />
      <Feedback />
      <Errors />
      <Lists />
      <SettingsLayout />
      <Streams />
      <Motion />
      <p className="gal-sub" style={{ marginTop: 48 }}>
        {t('common.app.name')} · 组件与动效
      </p>
    </main>
  )
}

// ---------------------------------------------------------------------------
// 令牌

const COLORS: Array<[string, string]> = [
  ['--fg', '正文'],
  ['--fg-2', '次要文字'],
  ['--fg-3', '占位 / 图标'],
  ['--accent', '强调色'],
  ['--accent-text', '强调色文字'],
  ['--accent-fill', '强调色底'],
  ['--danger', '危险'],
  ['--warning', '警告'],
  ['--success', '成功'],
  ['--surface-solid', '纯色表面'],
  ['--surface-raised', '浮起表面'],
  ['--surface-tint', '材质着色'],
  ['--fill-hover', '悬停'],
  ['--fill-pressed', '按下'],
  ['--fill-selected', '选中'],
  ['--fill-control', '控件底'],
  ['--hairline', '描边'],
  ['--mark-bg', '搜索高亮'],
]

function Tokens() {
  return (
    <GSection
      id="tokens"
      title="设计令牌"
      desc="颜色只能用令牌；深色模式单独调校（阴影更深、hairline 用白色、强调色提亮）。"
    >
      <div className="gal-grid" data-cols="3">
        <Spec label="颜色">
          <div className="gal-col">
            {COLORS.slice(0, 9).map(([v, name]) => (
              <span key={v} className="gal-swatch">
                <i style={{ background: `var(${v})` }} />
                {name} <code>{v}</code>
              </span>
            ))}
          </div>
        </Spec>
        <Spec label="表面与填充">
          <div className="gal-col">
            {COLORS.slice(9).map(([v, name]) => (
              <span key={v} className="gal-swatch">
                <i style={{ background: `var(${v})` }} />
                {name} <code>{v}</code>
              </span>
            ))}
          </div>
        </Spec>
        <Spec label="字号（五档 + 页面标题）">
          <div className="gal-type">
            <code>page 20</code>
            <span style={{ fontSize: 'var(--text-page)', fontWeight: 600 }}>设置 Settings</span>
            <code>title 15</code>
            <span style={{ fontSize: 'var(--text-title)', fontWeight: 600 }}>翻译风格</span>
            <code>body 14/1.6</code>
            <span style={{ fontSize: 'var(--text-body)', lineHeight: 'var(--lh-body)' }}>
              译文正文 Wisp 让翻译跟手出现。
            </span>
            <code>ui 13/18</code>
            <span style={{ fontSize: 'var(--text-ui)' }}>按钮、菜单、标签</span>
            <code>meta 12/16</code>
            <span style={{ fontSize: 'var(--text-meta)', color: 'var(--fg-2)' }}>
              gpt-5.4-mini · 1.2 s
            </span>
            <code>caption 11/14</code>
            <span style={{ fontSize: 'var(--text-caption)', color: 'var(--fg-2)' }}>
              分组标题 · 键帽
            </span>
          </div>
        </Spec>
        <Spec label="圆角">
          <div className="gal-row">
            {['panel', 'card', 'menu', 'row', 'control', 'kbd'].map((r) => (
              <State key={r} name={r}>
                <div
                  className="gal-box"
                  style={{ borderRadius: `var(--r-${r})`, boxShadow: 'var(--shadow-control)' }}
                />
              </State>
            ))}
          </div>
        </Spec>
        <Spec label="阴影">
          <div className="gal-row" style={{ gap: 24, padding: 8 }}>
            {['popover', 'menu', 'pinned', 'control'].map((s) => (
              <State key={s} name={s}>
                <div
                  className="gal-box"
                  style={{ borderRadius: 'var(--r-card)', boxShadow: `var(--shadow-${s})` }}
                />
              </State>
            ))}
          </div>
        </Spec>
        <Spec label="间距（8 pt 栅格）">
          <div className="gal-col">
            {[1, 2, 3, 4, 5, 6, 8].map((n) => (
              <span key={n} className="gal-swatch">
                <i
                  style={{
                    width: `var(--sp-${n})`,
                    height: 12,
                    background: 'var(--accent)',
                    borderRadius: 2,
                  }}
                />
                <code>--sp-{n}</code>
              </span>
            ))}
          </div>
        </Spec>
      </div>
    </GSection>
  )
}

// ---------------------------------------------------------------------------
// 按钮

function Buttons() {
  const variants = ['primary', 'secondary', 'ghost', 'danger'] as const
  return (
    <GSection
      id="buttons"
      title="按钮"
      desc="悬停 80 ms 背景过渡；按下 scale 0.98；焦点 2 px 强调色环（只在键盘导航时出现）；加载时宽度不变。"
    >
      <div className="gal-grid" data-cols="2">
        {variants.map((v) => (
          <Spec key={v} label={`Button · ${v}`}>
            <div className="gal-row">
              {STATES.map(([name, force]) => (
                <State key={name} name={name}>
                  <Button variant={v} data-force={force} icon={v === 'danger' ? Trash2 : undefined}>
                    {v === 'danger' ? '删除' : v === 'primary' ? '翻译' : '复制'}
                  </Button>
                </State>
              ))}
              <State name="disabled">
                <Button variant={v} disabled>
                  {v === 'primary' ? '翻译' : '复制'}
                </Button>
              </State>
              <State name="loading">
                <Button variant={v} loading>
                  {v === 'primary' ? '翻译' : '复制'}
                </Button>
              </State>
            </div>
            <div className="gal-row">
              <Button variant={v} size="md" icon={v === 'primary' ? Sparkles : Copy}>
                md · 32
              </Button>
              <Button variant={v} size="sm" trailingIcon={v === 'ghost' ? undefined : undefined}>
                sm · 28
              </Button>
            </div>
          </Spec>
        ))}
        <Spec label="IconButton（命中区 28；tooltip 显示快捷键）">
          <div className="gal-row">
            {STATES.map(([name, force]) => (
              <State key={name} name={name}>
                <IconButton icon={Copy} label="复制" shortcut="Mod+KeyC" data-force={force} />
              </State>
            ))}
            <State name="active">
              <IconButton icon={Pin} label="固定" active />
            </State>
            <State name="danger hover">
              <IconButton icon={Trash2} label="删除" tone="danger" data-force="hover" />
            </State>
            <State name="disabled">
              <IconButton icon={RotateCw} label="重新生成" disabled />
            </State>
            <State name="sm · 24">
              <IconButton icon={X} label="关闭" size="sm" />
            </State>
          </div>
        </Spec>
        <Spec label="CopyButton · Kbd">
          <div className="gal-row">
            <State name="icon">
              <CopyButton text="Wisp" />
            </State>
            <State name="button">
              <CopyButton text="Wisp" variant="button" />
            </State>
          </div>
          <div className="gal-row">
            <Kbd combo="Mod+KeyC" />
            <Kbd combo="Alt+Shift+KeyD" />
            <Kbd combo="Control+Alt+KeyV" />
            <Kbd combo="Escape" />
            <Kbd combo="Mod+Enter" />
            <Kbd combo="Shift+Space" />
            <Kbd combo="Mod+Shift+Digit1" variant="inline" />
          </div>
        </Spec>
      </div>
    </GSection>
  )
}

// ---------------------------------------------------------------------------
// 选择类

function SelectionControls() {
  const [seg, setSeg] = useState<'lines' | 'paragraphs' | 'markdown'>('lines')
  const [on, setOn] = useState(true)
  const [checked, setChecked] = useState(true)
  const [radio, setRadio] = useState<'system' | 'light' | 'dark'>('system')
  const [sel, setSel] = useState<string>('gpt-5.4-mini')
  const [slider, setSlider] = useState(60)
  const [num, setNum] = useState(14)
  const t = useT()
  return (
    <GSection
      id="selection"
      title="选择类控件"
      desc="Chip / StyleChip / SegmentedControl / Switch / Checkbox / Radio / Select / Slider / NumberField。"
    >
      <div className="gal-grid" data-cols="2">
        <Spec label="Chip · StyleChip">
          <div className="gal-row">
            {STATES.map(([name, force]) => (
              <State key={name} name={name}>
                <Chip data-force={force} dropdown>
                  全部
                </Chip>
              </State>
            ))}
            <State name="selected">
              <Chip selected>图片</Chip>
            </State>
            <State name="disabled">
              <Chip disabled>文件</Chip>
            </State>
          </div>
          <div className="gal-row">
            <State name="StyleChip">
              <StyleChip name={t('styles.natural')} />
            </State>
            <State name="StyleChip hover">
              <StyleChip name={t('styles.learn')} data-force="hover" />
            </State>
          </div>
        </Spec>
        <Spec label="SegmentedControl">
          <SegmentedControl
            label="OCR 模式"
            value={seg}
            onChange={setSeg}
            options={[
              { value: 'lines', label: '纯文本' },
              { value: 'paragraphs', label: '段落' },
              { value: 'markdown', label: '结构化' },
            ]}
          />
          <SegmentedControl
            label="大小"
            size="md"
            value={seg}
            onChange={setSeg}
            options={[
              { value: 'lines', label: 'S' },
              { value: 'paragraphs', label: 'M' },
              { value: 'markdown', label: 'L' },
            ]}
          />
        </Spec>
        <Spec label="Switch · Checkbox · Radio">
          <div className="gal-row">
            <State name="on">
              <Switch checked={on} onChange={setOn} aria-label="开关" />
            </State>
            <State name="off">
              <Switch checked={false} onChange={() => {}} aria-label="开关" />
            </State>
            <State name="focus">
              <Switch checked onChange={() => {}} aria-label="开关" data-force="focus" />
            </State>
            <State name="disabled">
              <Switch checked onChange={() => {}} aria-label="开关" disabled />
            </State>
            <State name="sm">
              <Switch checked onChange={() => {}} aria-label="开关" size="sm" />
            </State>
          </div>
          <Switch
            checked={on}
            onChange={setOn}
            label="开机自启"
            description="登录后在后台启动，托盘可见"
          />
          <div className="gal-row">
            <Checkbox checked={checked} onChange={setChecked} label="记录图片" />
            <Checkbox checked={false} onChange={() => {}} label="未勾选" />
            <Checkbox checked={false} indeterminate onChange={() => {}} label="部分" />
            <Checkbox checked onChange={() => {}} label="焦点" data-force="focus" />
            <Checkbox checked onChange={() => {}} label="禁用" disabled />
          </div>
          <RadioGroup
            label="主题"
            orientation="horizontal"
            value={radio}
            onChange={setRadio}
            options={[
              { value: 'system', label: '跟随系统' },
              { value: 'light', label: '浅色' },
              { value: 'dark', label: '深色' },
            ]}
          />
        </Spec>
        <Spec label="Select · Slider · NumberField">
          <div className="gal-row">
            <State name="default">
              <Select
                label="翻译模型"
                value={sel}
                onChange={setSel}
                options={[
                  { value: 'gpt-5.4-mini', label: 'gpt-5.4-mini', hint: 'OpenAI' },
                  { value: 'claude-haiku-4-5', label: 'claude-haiku-4-5', hint: 'Anthropic' },
                  { value: 'deepseek-chat', label: 'deepseek-chat', hint: 'DeepSeek' },
                ]}
              />
            </State>
            <State name="hover">
              <Select
                label="翻译模型"
                value={null}
                placeholder="选择模型"
                onChange={() => {}}
                options={[]}
                data-force="hover"
              />
            </State>
            <State name="invalid">
              <Select
                label="OCR 模型"
                value={null}
                placeholder="未选择"
                invalid
                onChange={() => {}}
                options={[]}
              />
            </State>
          </div>
          <Slider
            label="高度上限"
            min={40}
            max={80}
            value={slider}
            onChange={setSlider}
            valueLabel={`${slider}%`}
          />
          <Slider
            label="禁用"
            min={0}
            max={100}
            value={30}
            onChange={() => {}}
            disabled
            valueLabel="30%"
          />
          <div className="gal-row">
            <NumberField
              value={num}
              onChange={setNum}
              min={12}
              max={18}
              unit="px"
              aria-label="字号"
            />
            <NumberField
              value={300}
              onChange={() => {}}
              min={200}
              max={400}
              step={100}
              unit="ms"
              aria-label="间隔"
              data-force="focus"
            />
          </div>
        </Spec>
      </div>
    </GSection>
  )
}

// ---------------------------------------------------------------------------
// 输入

function Inputs() {
  const [text, setText] = useState('')
  const [url, setUrl] = useState('https://api.openai.com/v1')
  const [area, setArea] = useState('')
  const [q, setQ] = useState('wisp')
  return (
    <GSection
      id="inputs"
      title="输入"
      desc="输入框 28 / 32；聚焦 2 px 强调色环；错误为危险色描边；输入法组合中回车不提交。"
    >
      <div className="gal-grid" data-cols="2">
        <Spec label="TextField">
          <div className="gal-col">
            <TextField value={text} onChange={setText} placeholder="服务名称" aria-label="名称" />
            <TextField value={url} onChange={setUrl} aria-label="base_url" data-force="focus" />
            <TextField value="sk-••••••••3f2a" onChange={() => {}} aria-label="Key" invalid />
            <TextField value="只读" onChange={() => {}} aria-label="禁用" disabled />
            <TextField
              value="2048"
              onChange={() => {}}
              aria-label="长边"
              size="md"
              trailing={<span className="ui-field-unit">px</span>}
            />
          </div>
        </Spec>
        <Spec label="SearchField（field / bare）">
          <SearchField value={q} onChange={setQ} placeholder="搜索剪贴板" />
          <SearchField
            value=""
            onChange={() => {}}
            placeholder="搜索剪贴板历史"
            variant="bare"
            accessory={
              <Chip variant="subtle" dropdown>
                全部
              </Chip>
            }
          />
        </Spec>
        <Spec label="TextArea（自动增高，⌘↩ 提交）">
          <TextArea
            value={area}
            onChange={setArea}
            placeholder="输入或粘贴要翻译的文字"
            minRows={2}
            maxRows={6}
          />
          <TextArea value={'第一行\n第二行'} onChange={() => {}} minRows={2} data-force="focus" />
          <TextArea value="错误态" onChange={() => {}} minRows={1} invalid />
        </Spec>
        <Spec label="HotkeyRecorder">
          <Field label="划词翻译" stacked={false}>
            <HotkeyRecorder
              target={{ kind: 'action', action: 'translate_selection' }}
              value="Alt+KeyD"
              onChange={() => {}}
              label="划词翻译的快捷键"
            />
          </Field>
          <Field label="录制中">
            <HotkeyRecorder
              target={{ kind: 'action', action: 'ocr' }}
              value="Alt+KeyS"
              onChange={() => {}}
              label="截图 OCR 的快捷键"
              forceRecording
            />
          </Field>
          <Field label="未设置">
            <HotkeyRecorder
              target={{ kind: 'action', action: 'clipboard' }}
              value={null}
              onChange={() => {}}
              label="剪贴板的快捷键"
            />
          </Field>
          <Field label="冲突（红）">
            <HotkeyRecorder
              target={{ kind: 'action', action: 'translate_input' }}
              value="Alt+Shift+KeyD"
              onChange={() => {}}
              label="输入翻译的快捷键"
              initialReport={{
                combo: 'Super+Space',
                display: '⌘Space',
                acceptable: false,
                conflicts: [
                  {
                    level: 'forbidden',
                    source: 'reserved',
                    message_key: 'hotkeys.conflict.reserved',
                    params: { combo: '⌘Space' },
                  },
                ],
              }}
            />
          </Field>
          <Field label="可能冲突（黄）">
            <HotkeyRecorder
              target={{ kind: 'action', action: 'ocr_translate' }}
              value="Alt+Shift+KeyS"
              onChange={() => {}}
              label="截图翻译的快捷键"
              initialReport={{
                combo: 'Alt+KeyA',
                display: '⌥A',
                acceptable: true,
                conflicts: [
                  {
                    level: 'warning',
                    source: 'neighbor',
                    message_key: 'hotkeys.conflict.neighbor',
                    params: { app: 'Easydict', combo: '⌥A' },
                  },
                ],
              }}
            />
          </Field>
        </Spec>
      </div>
    </GSection>
  )
}

// ---------------------------------------------------------------------------
// 浮层

const STYLE_ITEMS = (
  t: ReturnType<typeof useT>,
  current: string,
  pick: (id: string) => void,
): MenuEntry[] => [
  { type: 'label', id: 'l', label: t('styles.menu.title') },
  ...(['natural', 'technical', 'academic', 'casual', 'learn', 'literal'] as const).map((id, i) => ({
    id,
    label: t(`styles.${id}`),
    hint: t(`styles.desc.${id}`),
    checked: id === current,
    shortcut: `Mod+Digit${i + 1}`,
    onSelect: () => pick(id),
  })),
  { type: 'separator', id: 's' },
  { id: 'edit', label: t('styles.menu.edit'), icon: Settings },
]

function Overlays() {
  const t = useT()
  const [style, setStyle] = useState('natural')
  const anchor = useRef<HTMLDivElement>(null)
  const more: MenuEntry[] = [
    { id: 'polish', label: t('panel.task.polish'), icon: Sparkles, shortcut: 'Mod+Shift+KeyP' },
    { id: 'proofread', label: t('panel.task.proofread'), icon: Bold },
    { id: 'copy-src', label: '复制原文', icon: Copy },
    { type: 'separator', id: 's1' },
    { id: 'disabled', label: '提升译文', disabled: true },
    { id: 'delete', label: '删除', icon: Trash2, danger: true, shortcut: 'Mod+Backspace' },
  ]
  return (
    <GSection
      id="overlays"
      title="浮层"
      desc="菜单 menuIn / menuOut（变换原点在触发器一侧）；Tooltip 500 ms 延迟、80 ms 淡入；在面板里挂到卡片内并预留高度。"
    >
      <div className="gal-grid" data-cols="3">
        <Spec label="DropdownMenu · StyleChip">
          <div className="gal-row">
            <DropdownMenu
              items={STYLE_ITEMS(t, style, setStyle)}
              highlightSelected
              label={t('styles.menu.title')}
              renderTrigger={(p) => (
                <StyleChip {...p} name={t(`styles.${style}` as 'styles.natural')} />
              )}
            />
            <DropdownMenu
              items={more}
              align="end"
              renderTrigger={(p) => (
                <IconButton
                  {...p}
                  icon={Ellipsis}
                  label={t('common.a11y.more_actions')}
                  noTooltip
                />
              )}
            />
          </div>
          <div ref={anchor} style={{ height: 0 }} />
          <div style={{ height: 300 }} />
          {/* 静态展示：锚点在长页面里、挂载时可能在视口外，不翻转（否则会翻到上方盖住本节标题） */}
          <Menu
            open
            anchor={anchor}
            items={STYLE_ITEMS(t, style, setStyle)}
            onClose={() => {}}
            initialActive="none"
            flip={false}
            data-testid="menu-static"
          />
        </Spec>
        <Spec label="Tooltip（静态展示）">
          <div className="gal-row" style={{ paddingTop: 36 }}>
            <Tooltip content="复制" shortcut="Mod+KeyC" forceOpen>
              <IconButton icon={Copy} label="复制" noTooltip />
            </Tooltip>
          </div>
          <div className="gal-row" style={{ paddingTop: 48 }}>
            <Tooltip content="固定到桌面" shortcut="Mod+KeyP" forceOpen side="bottom">
              <IconButton icon={Pin} label="固定" noTooltip />
            </Tooltip>
          </div>
          <div style={{ height: 40 }} />
        </Spec>
        <Spec label="交互演示">
          <div className="gal-row">
            <IconButton
              icon={ArrowLeftRight}
              label={t('common.action.show_source')}
              shortcut="Mod+KeyO"
            />
            <IconButton icon={Maximize2} label={t('common.action.expand')} />
            <IconButton icon={RotateCw} label={t('common.action.regenerate')} shortcut="Mod+KeyR" />
          </div>
          <p className="gal-sub">悬停按钮查看 tooltip；扫过相邻按钮时立即出现。</p>
        </Spec>
      </div>
    </GSection>
  )
}

// ---------------------------------------------------------------------------
// 反馈

function Feedback() {
  const t = useT()
  const [confirm, setConfirm] = useState(true)
  return (
    <GSection
      id="feedback"
      title="反馈"
      desc="Badge / Banner / InlineToast / InlineConfirm / EmptyState / Skeleton / CursorPulse / StatusLine / LangBar。"
    >
      <div className="gal-grid" data-cols="2">
        <Spec label="Badge · Spinner · CursorPulse">
          <div className="gal-row">
            <Badge>neutral</Badge>
            <Badge tone="accent">accent</Badge>
            <Badge tone="success" dot>
              已连接
            </Badge>
            <Badge tone="warning" dot>
              可能冲突
            </Badge>
            <Badge tone="danger" dot>
              未授权
            </Badge>
            <Spinner />
            <span style={{ fontSize: 'var(--text-body)' }}>
              首 token 前
              <CursorPulse />
            </span>
          </div>
        </Spec>
        <Spec label="Banner">
          <Banner
            tone="info"
            action={
              <Button size="sm" variant="ghost">
                了解
              </Button>
            }
          >
            检测到 Easydict 也使用 ⌥D，建议二选一
          </Banner>
          <Banner tone="warning" onDismiss={() => {}}>
            辅助功能权限在更新后失效，需要重新授权
          </Banner>
          <Banner tone="paused" action={<Button size="sm">恢复</Button>}>
            剪贴板监听已暂停
          </Banner>
        </Spec>
        <Spec label="InlineToast · InlineConfirm">
          <div className="gal-row">
            <InlineToast show>{t('common.state.copied')}</InlineToast>
          </div>
          <InlineConfirm
            open={confirm}
            message={
              <>
                再按一次 <Kbd combo="Backspace" /> 删除置顶项
              </>
            }
            onConfirm={() => setConfirm(false)}
            onCancel={() => setConfirm(false)}
            confirmLabel={t('common.action.delete')}
            timeoutMs={null}
          />
          {!confirm ? (
            <Button size="sm" onClick={() => setConfirm(true)}>
              {t('common.action.replay')}
            </Button>
          ) : null}
        </Spec>
        <Spec label="EmptyState · Skeleton">
          <EmptyState
            icon={ClipboardList}
            title="剪贴板还是空的"
            detail="复制的文字、图片和文件会出现在这里"
            compact
          />
          <Skeleton lines={3} />
        </Spec>
        <Spec label="StatusLine">
          <StatusLine
            model="gpt-5.4-mini"
            elapsedMs={1240}
            usage={{
              input_tokens: 42,
              output_tokens: 86,
              reasoning_tokens: null,
              total_tokens: 128,
            }}
          />
          <StatusLine state="cancelled" model="gpt-5.4-mini" />
          <StatusLine
            model="claude-haiku-4-5"
            elapsedMs={640}
            note={{ text: t('panel.status.truncated'), tone: 'warning' }}
          />
          <StatusLine
            model="deepseek-chat"
            elapsedMs={980}
            trailing={<InlineToast show>{t('common.state.copied')}</InlineToast>}
          />
        </Spec>
        <Spec label="LangBar">
          <LangBar source="en" target="zh-CN" onSwap={() => {}} onPickTarget={() => {}} />
          <LangBar source={null} target="zh-CN" detecting />
          <LangBar
            source="ja"
            target="en"
            lowConfidence
            onSwap={() => {}}
            onPickTarget={() => {}}
          />
        </Spec>
      </div>
    </GSection>
  )
}

// ---------------------------------------------------------------------------
// 错误与未就绪

function llmError(code: ErrorCode, extra: Partial<LlmError> = {}): LlmError {
  return {
    code,
    subcode: null,
    message: 'Incorrect API key provided: sk-proj-***3f2a.',
    retryable: true,
    hint: null,
    status: null,
    url: null,
    retry_after_s: null,
    upstream_request_id: 'req_8f2c41e0',
    ...extra,
  }
}

const ERRORS: Array<[string, LlmError, { provider?: string }]> = [
  [
    'auth',
    llmError('auth', { status: 401, url: 'api.openai.com/v1/chat/completions' }),
    { provider: 'OpenAI' },
  ],
  [
    'not_found',
    llmError('not_found', {
      status: 404,
      url: 'api.example.com/chat/completions',
      hint: 'errors.hint.try_v1',
    }),
    {},
  ],
  ['rate_limited', llmError('rate_limited', { status: 429, retry_after_s: 18 }), {}],
  ['quota', llmError('quota', { status: 402 }), {}],
  ['upstream', llmError('upstream', { status: 503 }), {}],
  [
    'network',
    llmError('network', { status: null, upstream_request_id: null, subcode: 'connection_lost' }),
    {},
  ],
  ['timeout', llmError('timeout', { upstream_request_id: null }), {}],
  ['content_filter', llmError('content_filter', { status: 400 }), {}],
  ['no_vision', llmError('no_vision', { status: 400 }), {}],
  ['image_too_large', llmError('image_too_large', { status: 413 }), {}],
  ['too_long', llmError('too_long', { status: 400 }), {}],
  [
    'protocol',
    llmError('protocol', { status: 200, subcode: 'html_body', url: 'example.com/' }),
    {},
  ],
  ['keychain', llmError('keychain', { upstream_request_id: null }), {}],
  ['internal', llmError('internal', { upstream_request_id: null }), {}],
]

const NOT_READY: NotReady[] = [
  { reason: 'no_provider' },
  { reason: 'no_default_model', task: 'translate' },
  { reason: 'no_vision_model' },
  { reason: 'permission_missing', permission: 'screen_recording' },
  { reason: 'capture_failed', message: 'errors.capture.backend' },
  { reason: 'capture_failed', message: 'errors.platform.wayland_capture' },
  { reason: 'keychain_unavailable' },
]

/** 取词失败以输入视图打开，原因显示在输入框上方的提示条里 */
const INPUT_HINTS: NotReady[] = [
  { reason: 'permission_missing', permission: 'accessibility' },
  { reason: 'wayland_copy_first' },
  { reason: 'elevated' },
  { reason: 'selection_unavailable', reason_key: 'errors.platform.clipboard_too_large' },
  { reason: 'selection_unavailable', reason_key: 'errors.platform.x11_missing_xtest' },
]

function Errors() {
  const noop = () => {}
  return (
    <GSection
      id="errors"
      title="错误卡片与未就绪"
      desc="按错误码取舍按钮；rate_limited 倒计时；quota 不重试；cancelled 与 param_unsupported 不出卡片。"
    >
      <div className="gal-grid" data-cols="2">
        {ERRORS.map(([name, err, o]) => (
          <Spec key={name} label={`ErrorCard · ${name}`}>
            <ErrorCard
              error={err}
              providerName={o.provider}
              onRetry={noop}
              onOpenSettings={noop}
              onChooseModel={noop}
              onSwitchModel={noop}
              defaultExpanded={name === 'auth'}
            />
          </Spec>
        ))}
        {NOT_READY.map((nr) => (
          <Spec key={JSON.stringify(nr)} label={`NotReadyCard · ${nr.reason}`}>
            <NotReadyCard notReady={nr} onOpenSettings={noop} onOpenSystemSettings={noop} />
          </Spec>
        ))}
        {INPUT_HINTS.map((nr) => (
          <Spec key={JSON.stringify(nr)} label={`NotReadyBanner（输入视图）· ${nr.reason}`}>
            <NotReadyBanner notReady={nr} onOpenSystemSettings={noop} />
          </Spec>
        ))}
      </div>
    </GSection>
  )
}

// ---------------------------------------------------------------------------
// 列表

const LIST_ITEMS = Array.from({ length: 120 }, (_, i) => ({
  id: i,
  image: i % 7 === 3,
  text:
    i % 7 === 3
      ? `截图 ${i}.png`
      : `剪贴板条目 ${i + 1} · The quick brown fox jumps over the lazy dog`,
}))

function Lists() {
  const [selected, setSelected] = useState(2)
  return (
    <GSection
      id="lists"
      title="列表"
      desc="行高 44 / 64；悬停淡底；选中由滚动内容层里的单一指示器绘制（spring.snappy），首次打开前 8 行 stagger。"
    >
      <div className="gal-grid" data-cols="2">
        <Spec label="ListRow 状态">
          <ListRow title="默认" subtitle="次要信息" trailing="3 分钟" />
          <ListRow title="悬停" trailing="1 小时" data-force="hover" />
          <ListRow title="按下" trailing="2 天" data-force="active" />
          <ListRow title="选中" trailing="刚刚" selected />
          <ListRow title="禁用" trailing="—" disabled />
          <ListRow
            size="lg"
            title={
              <Highlight
                text="搜索命中高亮 wisp 与 Wisp"
                ranges={[
                  { start: 7, end: 11 },
                  { start: 14, end: 18 },
                ]}
              />
            }
            subtitle="2560 × 1440 · 1.8 MB"
            trailing="5 分钟"
          />
        </Spec>
        <Spec label="VirtualList + 选中指示器（点击行）">
          <VirtualList
            height={300}
            count={LIST_ITEMS.length}
            estimateSize={(i) => (LIST_ITEMS[i]?.image ? 64 : 44)}
            selectedIndex={selected}
            stagger
            renderItem={(i) => {
              const it = LIST_ITEMS[i]
              return (
                <ListRow
                  indicator
                  selected={i === selected}
                  size={it?.image ? 'lg' : 'md'}
                  title={it?.text}
                  trailing={`${i} 分钟`}
                  onClick={() => setSelected(i)}
                />
              )
            }}
          />
        </Spec>
        <Spec label="ScrollArea（边缘渐隐）">
          <ScrollArea maxHeight={160}>
            {Array.from({ length: 14 }, (_, i) => (
              <p key={i} style={{ margin: '0 0 8px', fontSize: 'var(--text-ui)' }}>
                第 {i + 1} 段：滚动到中间时上下边缘都有 12 px 渐隐。
              </p>
            ))}
          </ScrollArea>
          <Divider />
          <div className="gal-row">
            <span>左</span>
            <Divider orientation="vertical" />
            <span>右</span>
          </div>
        </Spec>
      </div>
    </GSection>
  )
}

// ---------------------------------------------------------------------------
// 设置布局

function SettingsLayout() {
  const [a, setA] = useState(true)
  return (
    <GSection
      id="settings"
      title="设置布局"
      desc="Section（分组卡片）+ Field（左标签右控件 / stacked 上下排列）。"
    >
      <div className="gal-grid" data-cols="2">
        <div>
          <Section title="通用" description="启动与语言">
            <Field label="开机自启" description="登录后在后台启动">
              <Switch checked={a} onChange={setA} aria-label="开机自启" />
            </Field>
            <Field label="界面语言">
              <Select
                label="界面语言"
                value="system"
                onChange={() => {}}
                options={[{ value: 'system', label: '跟随系统' }]}
              />
            </Field>
            <Field label="Base URL" stacked message="地址需要以 https:// 开头">
              <TextField
                value="api.example.com"
                onChange={() => {}}
                invalid
                aria-label="Base URL"
              />
            </Field>
          </Section>
          <Section
            title="服务"
            action={
              <Button size="sm" icon={Plus}>
                添加
              </Button>
            }
          >
            <Field label="OpenAI" description="gpt-5.4-mini · 5 个模型">
              <Badge tone="success" dot>
                已连接
              </Badge>
            </Field>
          </Section>
        </div>
      </div>
    </GSection>
  )
}

// ---------------------------------------------------------------------------
// 流式文字

function LiveStream({ format, text }: { format: 'plain' | 'mdlite' | 'edit_split'; text: string }) {
  const handle = useRef<StreamHandle>(null)
  const [run, setRun] = useState(0)
  useEffect(() => {
    if (run === 0) return
    const h = handle.current
    if (!h) return
    h.reset()
    let i = 0
    const timer = setInterval(() => {
      const step = 3
      h.append(text.slice(i, i + step))
      i += step
      if (i >= text.length) {
        clearInterval(timer)
        h.end()
      }
    }, 20)
    return () => clearInterval(timer)
  }, [run, text])
  return (
    <>
      <StreamView ref={handle} format={format} initialText={text} ended maxHeight={240} />
      <div className="gal-row">
        <Button size="sm" icon={RotateCw} onClick={() => setRun((r) => r + 1)}>
          流式重播
        </Button>
      </div>
    </>
  )
}

function Streams() {
  return (
    <GSection
      id="stream"
      title="流式文字"
      desc="每个 rAF 合并一次；新文字块 120 ms 淡入；末尾光标 1 Hz；mdlite 只重解析尾块；edit_split 正文 + 淡色改动。"
    >
      <div className="gal-grid" data-cols="3">
        <Spec label="plain">
          <LiveStream
            format="plain"
            text={
              'Wisp 从不打扰你：空闲时几乎不占 CPU，唤出只需几毫秒，译文就在光标旁边逐字流出。\n\n保留原文的换行与    空格。'
            }
          />
        </Spec>
        <Spec label="mdlite（学习风格）">
          <LiveStream format="mdlite" text={LEARN_SAMPLE} />
        </Spec>
        <Spec label="edit_split（润色）">
          <LiveStream format="edit_split" text={EDIT_SAMPLE} />
        </Spec>
      </div>
    </GSection>
  )
}

// ---------------------------------------------------------------------------
// 动效

function Replay({ children, label }: { label: string; children: (key: number) => ReactNode }) {
  const [k, setK] = useState(0)
  return (
    <Spec label={label}>
      <div className="gal-demo">{children(k)}</div>
      <div className="gal-row">
        <Button size="sm" icon={RotateCw} onClick={() => setK((n) => n + 1)}>
          重播
        </Button>
      </div>
    </Spec>
  )
}

function PresetBox({ name, children }: { name: PresetName; children?: ReactNode }) {
  const motion = usePreset(name)
  return (
    <m.div className="gal-chip-target" {...motion}>
      {children ?? name}
    </m.div>
  )
}

/** 阻尼弹簧从 0 到 1 的进度（初速度 0），与 Motion / Rust 的 spring 同一公式 */
function springProgress(
  p: { stiffness: number; damping: number; mass: number },
  t: number,
): number {
  const w0 = Math.sqrt(p.stiffness / p.mass)
  const zeta = p.damping / (2 * Math.sqrt(p.stiffness * p.mass))
  if (zeta >= 1) return 1 - Math.exp(-w0 * t) * (1 + w0 * t)
  const wd = w0 * Math.sqrt(1 - zeta * zeta)
  const decay = Math.exp(-zeta * w0 * t)
  return 1 - decay * (Math.cos(wd * t) + ((zeta * w0) / wd) * Math.sin(wd * t))
}

/** 三次贝塞尔缓动在时间比例 x 处的进度（二分求解） */
function bezierProgress(c: readonly number[], x: number): number {
  const [x1 = 0, y1 = 0, x2 = 1, y2 = 1] = c
  const bez = (a: number, b: number, s: number) =>
    3 * a * s * (1 - s) ** 2 + 3 * b * s * s * (1 - s) + s ** 3
  let lo = 0
  let hi = 1
  for (let i = 0; i < 32; i++) {
    const mid = (lo + hi) / 2
    if (bez(x1, x2, mid) < x) lo = mid
    else hi = mid
  }
  return bez(y1, y2, (lo + hi) / 2)
}

/** 冻结帧的时刻（秒） */
const MID_T = 0.06

/**
 * panelEnter 冻结在 t = 60 ms 的中间帧（红点为变换原点）。按 presets 的弹簧与缓动参数直接算出该时刻的样式，
 * 不用暂停的命令式动画（Motion 里暂停并设定 time 会干扰同页其他动画的提交）。
 */
function EnterMidFrame({ placement }: { placement: 'below' | 'above' | 'center' }) {
  const enter = presets.panelEnter(placement, 80, 'css')
  const p = springProgress(spring.snappy, MID_T)
  const opacity = bezierProgress(ease.standard, MID_T / duration.base)
  const from = {
    y: enter.initial.y,
    scale: ('scale' in enter.initial ? enter.initial.scale : 1) ?? 1,
  }
  const y = from.y * (1 - p)
  const scale = from.scale + (1 - from.scale) * p
  const originTop = placement === 'below' ? 0 : placement === 'above' ? '100%' : '50%'
  const originLeft = placement === 'center' ? '50%' : 80
  return (
    <div className="gal-stage">
      {placement !== 'center' ? <span className="gal-anchor" data-placement={placement} /> : null}
      <div
        className="gal-mini"
        data-placement={placement}
        style={{
          transformOrigin: enter.transformOrigin,
          opacity,
          transform: `translateY(${y.toFixed(2)}px) scale(${scale.toFixed(4)})`,
        }}
      >
        <span className="gal-origin" style={{ top: originTop, left: originLeft }} />
        <strong>{placement}</strong>
        <div
          style={{
            color: 'var(--fg-2)',
            fontSize: 'var(--text-meta)',
            marginTop: placement === 'center' ? 'var(--sp-3)' : 0,
          }}
        >
          t = 60 ms · opacity {opacity.toFixed(2)} · scale {scale.toFixed(3)} · y {y.toFixed(1)}
        </div>
      </div>
    </div>
  )
}

function EnterLive({ placement, run }: { placement: 'below' | 'above' | 'center'; run: number }) {
  const reduced = useReducedMotionPref()
  const raw = presets.panelEnter(placement, 80, 'css')
  const enter = adaptPreset(raw, 'fade', reduced)
  return (
    <div className="gal-stage">
      {placement !== 'center' ? <span className="gal-anchor" data-placement={placement} /> : null}
      <m.div
        key={run}
        className="gal-mini"
        data-placement={placement}
        style={{ transformOrigin: raw.transformOrigin }}
        initial={enter.initial}
        animate={enter.animate}
        transition={enter.transition}
      >
        <strong>panelEnter · {placement}</strong>
        <div style={{ color: 'var(--fg-2)', fontSize: 'var(--text-meta)' }}>
          spring.snappy 520 / 38 / 1
        </div>
      </m.div>
    </div>
  )
}

/** panelExit（Css 帧模式）：从显示状态播退出（fast + ease.exit；减少动效时只淡出） */
function ExitLive({ run }: { run: number }) {
  const reduced = useReducedMotionPref()
  const raw = presets.panelEnter('below', 80, 'css')
  const exit = reduced ? { opacity: 0, transition: presets.reduced.fade } : presets.panelExit
  return (
    <div className="gal-stage">
      <span className="gal-anchor" data-placement="below" />
      <m.div
        key={run}
        className="gal-mini"
        data-placement="below"
        style={{ transformOrigin: raw.transformOrigin }}
        initial={{ opacity: 1, scale: 1 }}
        animate={run > 0 ? exit : { opacity: 1, scale: 1 }}
      >
        <strong>panelExit</strong>
        <div style={{ color: 'var(--fg-2)', fontSize: 'var(--text-meta)' }}>
          opacity → 0 · scale 0.98 · fast + ease.exit
        </div>
      </m.div>
    </div>
  )
}

/** crossfade（ViewSwitch）：旧视图脱离布局原地淡出，新视图淡入，120 ms 交叉 */
function CrossfadeDemo() {
  const reduced = useReducedMotionPref()
  const [view, setView] = useState(0)
  const transition = reduced ? presets.reduced.fade : presets.crossfade
  return (
    <Spec label="crossfade（视图切换 120 ms）">
      <div className="gal-demo">
        <AnimatePresence mode="popLayout" initial={false}>
          <m.div
            key={view}
            className="gal-chip-target"
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            transition={transition}
          >
            {view % 2 === 0 ? '输入翻译' : '翻译结果 · 自然'}
          </m.div>
        </AnimatePresence>
      </div>
      <div className="gal-row">
        <Button size="sm" onClick={() => setView((v) => v + 1)}>
          切换视图
        </Button>
      </div>
    </Spec>
  )
}

/** overlayDim + flyTo（截图 Overlay）：暗化层 0 → 1（颜色 --dim-overlay），选区飞向面板 */
function OverlayDemo({ run }: { run: number }) {
  const reduced = useReducedMotionPref()
  return (
    <div className="gal-demo gal-overlay-demo" data-testid="overlay-demo">
      <m.div
        key={`dim-${run}`}
        className="gal-dim"
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={reduced ? presets.reduced.fade : presets.overlayDim}
      />
      <m.div
        key={`fly-${run}`}
        className="gal-selection"
        initial={{ x: -64, y: 18, scale: 1, opacity: 1 }}
        animate={run > 0 ? { x: 72, y: -22, scale: 0.4, opacity: 0 } : { x: -64, y: 18 }}
        transition={reduced ? presets.reduced.fade : presets.flyTo}
      />
      <div className="gal-fly-target">OCR</div>
    </div>
  )
}

/** listStagger + rowIn：打开时前 8 行依次进入（只作用于前 8 行，整体 ≤ 170 ms） */
function StaggerDemo({ run }: { run: number }) {
  const reduced = useReducedMotionPref()
  const rows = Array.from({ length: presets.listStagger.maxItems }, (_, i) => i)
  return (
    <div className="gal-demo gal-stagger" key={run}>
      {rows.map((i) =>
        reduced ? (
          <div key={i} className="gal-stagger-row" />
        ) : (
          <m.div
            key={i}
            className="gal-stagger-row"
            initial={presets.rowIn.initial}
            animate={presets.rowIn.animate}
            transition={{
              ...presets.rowIn.transition,
              delay: i * presets.listStagger.staggerChildren,
            }}
          />
        ),
      )}
    </div>
  )
}

/** menuIn / menuOut：弹出层进入与退出（退出 100 ms，ease.exit） */
function MenuInOutDemo() {
  const [open, setOpen] = useState(true)
  const menuIn = usePreset('menuIn')
  const reduced = useReducedMotionPref()
  const exit = reduced ? { opacity: 0, transition: presets.reduced.fade } : presets.menuOut
  return (
    <Spec label="menuIn / menuOut">
      <div className="gal-demo">
        <AnimatePresence initial={false}>
          {open ? (
            <m.div key="menu" className="gal-chip-target" {...menuIn} exit={exit}>
              菜单
            </m.div>
          ) : null}
        </AnimatePresence>
      </div>
      <div className="gal-row">
        <Button size="sm" onClick={() => setOpen((v) => !v)} data-testid="toggle-menu-demo">
          {open ? '关闭（menuOut）' : '打开（menuIn）'}
        </Button>
      </div>
    </Spec>
  )
}

/** confirmIn：InlineConfirm 的展开 / 收起（高度 spring.gentle + 内容交叉淡入） */
function ConfirmDemo() {
  const [open, setOpen] = useState(true)
  return (
    <Spec label="confirmIn（InlineConfirm）">
      <div className="gal-demo gal-confirm-demo">
        <InlineConfirm
          open={open}
          message="再按一次 ⌫ 删除置顶项"
          onConfirm={() => setOpen(false)}
          onCancel={() => setOpen(false)}
          timeoutMs={null}
          buttons={false}
        />
      </div>
      <div className="gal-row">
        <Button size="sm" onClick={() => setOpen((v) => !v)} data-testid="toggle-confirm-demo">
          {open ? '收起' : '展开'}
        </Button>
      </div>
    </Spec>
  )
}

function Motion() {
  const [run, setRun] = useState(0)
  const [exitRun, setExitRun] = useState(0)
  const [overlayRun, setOverlayRun] = useState(0)
  const [staggerRun, setStaggerRun] = useState(0)
  const reduced = useReducedMotionPref()
  const [toggle, setToggle] = useState(true)
  const [blur, setBlur] = useState(false)
  const [lift, setLift] = useState(0)
  return (
    <GSection
      id="motion"
      title="动效预设"
      desc="数值全部来自 shared/motion/tokens.ts；减少动效时按 reducedPolicy 改写（只淡入淡出或直接出现）。"
    >
      <div className="gal-grid" data-cols="3">
        <EnterMidFrame placement="below" />
        <EnterMidFrame placement="above" />
        <EnterMidFrame placement="center" />
      </div>
      <div className="gal-row" style={{ margin: '12px 0' }}>
        <Button
          size="sm"
          variant="primary"
          icon={RotateCw}
          onClick={() => setRun((r) => r + 1)}
          data-testid="replay-enter"
        >
          重播 panelEnter
        </Button>
      </div>
      <div className="gal-grid" data-cols="3">
        <EnterLive placement="below" run={run} />
        <EnterLive placement="above" run={run} />
        <EnterLive placement="center" run={run} />
      </div>
      <div className="gal-grid" data-cols="3" style={{ marginTop: 16 }}>
        <Spec label="panelExit（Css 帧模式；Native 的退出是窗口 alpha）">
          <ExitLive run={exitRun} />
          <div className="gal-row">
            <Button
              size="sm"
              icon={RotateCw}
              onClick={() => setExitRun((r) => r + 1)}
              data-testid="replay-exit"
            >
              播放 panelExit
            </Button>
          </div>
        </Spec>
        <Spec label="overlayDim + flyTo（截图 Overlay）">
          <OverlayDemo run={overlayRun} />
          <div className="gal-row">
            <Button
              size="sm"
              icon={RotateCw}
              onClick={() => setOverlayRun((r) => r + 1)}
              data-testid="replay-overlay"
            >
              重播
            </Button>
          </div>
        </Spec>
        <Spec label="listStagger + rowIn（前 8 行）">
          <StaggerDemo run={staggerRun} />
          <div className="gal-row">
            <Button
              size="sm"
              icon={RotateCw}
              onClick={() => setStaggerRun((r) => r + 1)}
              data-testid="replay-stagger"
            >
              重播
            </Button>
          </div>
        </Spec>
      </div>
      <div className="gal-grid" style={{ marginTop: 16 }}>
        {(
          [
            'menuIn',
            'tooltip',
            'popover',
            'previewIn',
            'toast',
            'cardIn',
            'footerIn',
            'iconSwap',
            'statusPulse',
            'rowInsert',
            'rowIn',
          ] as const
        ).map((name) => (
          <Replay key={name} label={name}>
            {(k) => <PresetBox key={k} name={name} />}
          </Replay>
        ))}
        <Replay label="sectionExpand">
          {(k) => (
            <div key={k} style={{ width: '80%' }}>
              <AnimatePresence initial={false}>
                <PresetBox name="sectionExpand">
                  <div style={{ padding: 8 }}>高度 spring.gentle + 内容交叉淡入 120 ms</div>
                </PresetBox>
              </AnimatePresence>
            </div>
          )}
        </Replay>
        <ConfirmDemo />
        <MenuInOutDemo />
        <CrossfadeDemo />
        <Spec label="pageSwitch / stepSwitch">
          <div className="gal-demo">
            <AnimatePresence mode="wait" initial={false}>
              <m.div
                key={String(toggle)}
                className="gal-chip-target"
                {...adaptPreset(presets.stepSwitch(toggle ? 1 : -1), 'fade', reduced)}
              >
                {toggle ? '步骤 2' : '步骤 1'}
              </m.div>
            </AnimatePresence>
          </div>
          <div className="gal-row">
            <Button size="sm" onClick={() => setToggle((v) => !v)}>
              切换（x ±8）
            </Button>
          </div>
        </Spec>
        <Spec label="styleSwitchOut / In（正文 2 px 模糊）">
          <div className="gal-demo">
            <m.div
              className="gal-chip-target"
              animate={
                blur
                  ? reduced
                    ? presets.styleSwitchOutPlain
                    : presets.styleSwitchOut
                  : reduced
                    ? presets.styleSwitchInPlain
                    : presets.styleSwitchIn
              }
            >
              Wisp 让翻译跟手出现。
            </m.div>
          </div>
          <div className="gal-row">
            <Button size="sm" onClick={() => setBlur((v) => !v)}>
              {blur ? '新内容到达' : '切换风格'}
            </Button>
          </div>
        </Spec>
        <Spec label="pinLift（固定后浮起）">
          <div className="gal-demo">
            <m.div
              key={lift}
              className="gal-chip-target"
              initial={{ y: -2 }}
              animate={{ y: 0 }}
              transition={presets.pinLift.transition}
              style={{ boxShadow: lift > 0 ? 'var(--shadow-pinned)' : 'var(--shadow-control)' }}
            >
              Pinned
            </m.div>
          </div>
          <div className="gal-row">
            <Button size="sm" onClick={() => setLift((v) => v + 1)}>
              重播
            </Button>
          </div>
        </Spec>
        <Spec label="press（whileTap）">
          <div className="gal-demo">
            <m.button
              type="button"
              className="ui-btn"
              data-variant="secondary"
              whileTap={reduced ? undefined : { scale: presets.press.scale }}
              transition={presets.press.transition}
            >
              按住我
            </m.button>
          </div>
        </Spec>
      </div>
    </GSection>
  )
}
