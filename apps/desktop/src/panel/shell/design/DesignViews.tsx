/**
 * 高保真视觉稿：面板视图（design.*，只在浏览器 mock 中加载）。
 *
 * 这些是用组件库拼出的**静态组合**（不含业务逻辑），挂在真实的面板外壳里（PanelCard、进入动效、高度跟随、假桌面），
 * 是翻译 / 输入、OCR、剪贴板视图的视觉基准：头部 / 正文 / 底栏的结构、间距、控件与文案以这里为准。
 *
 * i18n-ignore-file：只在浏览器 mock 中加载的参照页（生产包里没有），说明文字与示例文案是写死的参照；
 * 视图与设置的实现里文案一律走 t()。
 */
import './design.css'
import {
  ArrowLeftRight,
  CircleStop,
  Code,
  Ellipsis,
  File,
  Image as ImageIcon,
  Languages,
  Link,
  Maximize2,
  Pin,
  RotateCw,
  Type,
  X,
} from 'lucide-react'
import { m } from 'motion/react'
import { useMemo, useState } from 'react'
import { boot } from '../../../shared/ipc'
import { createFixtures } from '../../../shared/ipc/mock/fixtures'
import type { ClipHit, LlmError } from '../../../shared/ipc/types'
import { relativeTime } from '../../../shared/i18n/relativeTime'
import { useT } from '../../../shared/i18n/useT'
import { usePreset } from '../../../shared/motion/presets'
import { StreamView } from '../../../shared/stream/StreamView'
import {
  AppIcon,
  Badge,
  Button,
  Chip,
  CopyButton,
  EmptyState,
  ErrorCard,
  Highlight,
  IconButton,
  Kbd,
  LangBar,
  ListRow,
  SearchField,
  Skeleton,
  StatusLine,
  StyleChip,
  TextArea,
  Thumbnail,
  VirtualList,
} from '../../../shared/ui'
import type { ViewSwitchRenderProps } from '../ViewSwitch'
import {
  INPUT_DRAFT,
  OCR_TEXT,
  OCR_TRANSLATION,
  TRANSLATION_LONG_ZH,
  TRANSLATION_ZH,
} from './samples'

/** 视觉稿的样例数据：整页只构造一次（每次渲染都 createFixtures 会在渲染里重复生成全部样例与占位图） */
let fixtures: ReturnType<typeof createFixtures> | null = null
function designFixtures(): ReturnType<typeof createFixtures> {
  fixtures ??= createFixtures(boot)
  return fixtures
}

export type DesignViewId =
  | 'translate-streaming'
  | 'translate-done'
  | 'translate-dict'
  | 'translate-error'
  | 'ocr-recognizing'
  | 'ocr-done'
  | 'clipboard-list'
  | 'clipboard-search'
  | 'clipboard-empty'
  | 'input'

const USAGE = { input_tokens: 42, output_tokens: 86, reasoning_tokens: null, total_tokens: 128 }

/** 翻译视图头部：语言条 · 风格 chip · ⋯ · ✕ */
function TranslateHeader() {
  const t = useT()
  return (
    <div className="dz-header">
      <LangBar source="en" target="zh-CN" onSwap={() => {}} onPickTarget={() => {}} />
      <span className="dz-spacer" />
      <StyleChip name={t('styles.natural')} />
      <IconButton icon={Ellipsis} label={t('common.a11y.more_actions')} />
      <IconButton icon={X} label={t('common.action.close')} shortcut="Escape" />
    </div>
  )
}

/** 完成后的底栏：状态行 · 复制 · 固定 · 原文 · 展开 */
function DoneFooter({ text }: { text: string }) {
  const t = useT()
  const motion = usePreset('footerIn')
  return (
    <m.div className="dz-footer" {...motion}>
      <StatusLine model="gpt-5.4-mini" elapsedMs={1240} usage={USAGE} />
      <CopyButton text={text} shortcut="Mod+KeyC" />
      <IconButton icon={Pin} label={t('common.action.pin')} shortcut="Mod+KeyP" />
      <IconButton icon={ArrowLeftRight} label={t('common.action.show_source')} />
      <IconButton icon={Maximize2} label={t('common.action.expand')} />
    </m.div>
  )
}

function TranslateStreaming() {
  const t = useT()
  const partial = TRANSLATION_LONG_ZH.slice(0, 58)
  return (
    <section data-testid="design-translate-streaming">
      <TranslateHeader />
      <StreamView format="plain" initialText={partial} />
      <div className="dz-footer">
        <StatusLine model="gpt-5.4-mini" note={{ text: t('common.state.translating') }} />
        <Button size="sm" variant="ghost" icon={CircleStop}>
          {t('common.action.stop')}
        </Button>
      </div>
    </section>
  )
}

function TranslateDone() {
  return (
    <section data-testid="design-translate-done">
      <TranslateHeader />
      <StreamView format="plain" initialText={TRANSLATION_LONG_ZH} ended />
      <DoneFooter text={TRANSLATION_LONG_ZH} />
    </section>
  )
}

function TranslateDict() {
  return (
    <section data-testid="design-translate-dict">
      <TranslateHeader />
      <div className="ui-selectable">
        <div className="dz-dict-head">
          <span className="dz-dict-word">serendipity</span>
          <span className="dz-dict-phon">/ˌser.ənˈdɪp.ə.ti/</span>
        </div>
        <div className="dz-dict-sense">
          <Badge>n.</Badge>
          <span>意外发现美好事物的运气；机缘巧合</span>
          <p className="dz-dict-example">
            It was pure <b>serendipity</b> that we met.
            <br />
            我们的相遇纯属机缘巧合。
          </p>
          <Badge>adj.</Badge>
          <span>（serendipitous）偶然发现的；碰巧得到的</span>
        </div>
      </div>
      <DoneFooter text="serendipity n. 意外发现美好事物的运气；机缘巧合" />
    </section>
  )
}

function TranslateError() {
  const error: LlmError = {
    code: 'auth',
    subcode: null,
    message: 'Incorrect API key provided: sk-proj-***3f2a.',
    retryable: false,
    hint: 'errors.hint.check_key',
    status: 401,
    url: 'api.openai.com/v1/chat/completions',
    retry_after_s: null,
    upstream_request_id: 'req_8f2c41e0',
  }
  return (
    <section data-testid="design-translate-error">
      <TranslateHeader />
      <ErrorCard error={error} providerName="OpenAI" onOpenSettings={() => {}} />
    </section>
  )
}

// ---------------------------------------------------------------------------
// OCR

function OcrMedia({ recognizing }: { recognizing: boolean }) {
  const t = useT()
  const thumb = designFixtures().payload.ocr()
  return (
    <div className="dz-header" style={{ alignItems: 'flex-start', marginBottom: 10 }}>
      <div className="dz-ocr-media" style={{ margin: '2px 0 0 6px' }}>
        {thumb.kind === 'ocr' ? (
          <Thumbnail
            src={thumb.thumb_url}
            alt="capture"
            naturalSize={thumb.image_size}
            maxHeight={72}
            onOpen={() => {}}
          />
        ) : null}
        <span className="dz-ocr-meta">
          <span>1280 × 480</span>
          <span>{recognizing ? t('common.state.recognizing') : 'qwen3-vl-plus'}</span>
        </span>
      </div>
      <span className="dz-spacer" />
      <IconButton icon={X} label={t('common.action.close')} shortcut="Escape" />
    </div>
  )
}

function OcrRecognizing() {
  return (
    <section data-testid="design-ocr-recognizing">
      <OcrMedia recognizing />
      <Skeleton lines={3} />
    </section>
  )
}

/** OCR 完成态里译文区之上的内容高度（缩略图、OCR 文本、底栏、译文区头部） */
const OCR_UPPER = 372

function OcrDone({ maxHeight }: { maxHeight: number }) {
  const t = useT()
  const [text, setText] = useState(OCR_TEXT)
  return (
    <section data-testid="design-ocr-done">
      <OcrMedia recognizing={false} />
      <TextArea
        variant="bare"
        value={text}
        onChange={setText}
        minRows={2}
        maxRows={8}
        aria-label="OCR"
        className="dz-ocr-text"
      />
      <div className="dz-footer">
        <StatusLine model="qwen3-vl-plus" elapsedMs={2140} />
        <CopyButton text={text} shortcut="Mod+KeyC" />
        <Chip variant="subtle" icon={Languages} dropdown selected>
          {t('common.action.translate')}
        </Chip>
        <IconButton icon={RotateCw} label={t('common.action.regenerate')} shortcut="Mod+KeyR" />
        <IconButton icon={Pin} label={t('common.action.pin')} shortcut="Mod+KeyP" />
      </div>
      <div className="dz-section">
        <div className="dz-header" style={{ marginBottom: 6 }}>
          <LangBar source="zh-CN" target="en" onSwap={() => {}} onPickTarget={() => {}} />
          <span className="dz-spacer" />
          <StyleChip name={t('styles.technical')} />
          <CopyButton text={OCR_TRANSLATION} />
        </div>
        {/* 译文区的高度 = 卡片上限减去上方内容，超出部分在区内滚动 */}
        <StreamView
          format="plain"
          initialText={OCR_TRANSLATION}
          ended
          maxHeight={Math.max(96, maxHeight - OCR_UPPER)}
        />
      </div>
    </section>
  )
}

// ---------------------------------------------------------------------------
// 剪贴板

const KIND_ICONS = {
  text: Type,
  rich_text: Type,
  link: Link,
  code: Code,
  files: File,
  image: ImageIcon,
}

function ClipLeading({ hit }: { hit: ClipHit }) {
  const it = hit.item
  if (it.kind === 'image' && it.image?.thumb_url) {
    return (
      <Thumbnail
        src={it.image.thumb_url}
        alt=""
        naturalSize={it.image}
        maxHeight={44}
        maxWidth={56}
      />
    )
  }
  if (it.kind === 'color') {
    return (
      <span className="dz-kind">
        <span className="dz-kind-color" style={{ background: it.color ?? undefined }} />
      </span>
    )
  }
  const I = KIND_ICONS[it.kind as keyof typeof KIND_ICONS] ?? Type
  return (
    <span className="dz-kind">
      <I size={14} strokeWidth={1.5} absoluteStrokeWidth aria-hidden />
    </span>
  )
}

function ClipTitle({ hit }: { hit: ClipHit }) {
  const it = hit.item
  if (it.kind === 'image' && it.image) return <>{`${it.image.width} × ${it.image.height}`}</>
  const text = <Highlight text={it.preview} ranges={hit.highlights} />
  return it.kind === 'code' ? <span className="dz-clip-code">{text}</span> : text
}

/** 剪贴板头部与底部提示条的高度（列表高度 = 卡片上限 − 两者，超出部分在列表内滚动） */
const CLIP_CHROME = 48 + 36

function ClipboardDesign({
  query,
  empty = false,
  maxHeight,
}: {
  query: string
  empty?: boolean
  maxHeight: number
}) {
  const t = useT()
  const page = useMemo(() => {
    const f = designFixtures()
    return f.clipPage(empty ? [] : f.clipItems(), 'all', query || null)
  }, [empty, query])
  const pinned = page.pinned_count
  type Row = { type: 'group'; label: string } | { type: 'item'; hit: ClipHit; n: number }
  const rows: Row[] = []
  page.items.forEach((hit, i) => {
    if (i === 0 && pinned > 0) rows.push({ type: 'group', label: '置顶' })
    if (i === pinned && i < page.items.length) rows.push({ type: 'group', label: '最近' })
    rows.push({ type: 'item', hit, n: i + 1 })
  })
  const selected = rows.findIndex((r) => r.type === 'item' && r.n === (query ? 1 : 3))
  const [now] = useState(() => Date.now())
  return (
    <section
      className="dz-clip"
      data-testid={
        empty
          ? 'design-clipboard-empty'
          : query
            ? 'design-clipboard-search'
            : 'design-clipboard-list'
      }
    >
      <div className="dz-clip-head">
        <SearchField
          variant="bare"
          value={query}
          onChange={() => {}}
          placeholder="搜索剪贴板历史"
          aria-label="搜索剪贴板历史"
        />
        <Chip variant="subtle" dropdown>
          全部
        </Chip>
        <IconButton icon={Ellipsis} label={t('common.a11y.more_actions')} />
      </div>
      {empty ? (
        <EmptyState title="剪贴板还是空的" detail="复制的文字、图片和文件会出现在这里" />
      ) : (
        <VirtualList
          height={Math.min(
            maxHeight - CLIP_CHROME,
            rows.reduce(
              (h, r) => h + (r.type === 'group' ? 28 : r.hit.item.kind === 'image' ? 64 : 44),
              8,
            ),
          )}
          count={rows.length}
          selectedIndex={selected}
          stagger
          estimateSize={(i) => {
            const r = rows[i]
            return !r || r.type === 'group' ? 28 : r.hit.item.kind === 'image' ? 64 : 44
          }}
          renderItem={(i) => {
            const r = rows[i]
            if (!r) return null
            if (r.type === 'group') return <div className="dz-group">{r.label}</div>
            const it = r.hit.item
            return (
              <ListRow
                indicator
                selected={i === selected}
                size={it.kind === 'image' ? 'lg' : 'md'}
                leading={<ClipLeading hit={r.hit} />}
                title={<ClipTitle hit={r.hit} />}
                trailing={
                  <>
                    {it.source ? <AppIcon src={it.source.icon_url} name={it.source.name} /> : null}
                    <span>{relativeTime(it.last_used_at, now, 'short')}</span>
                  </>
                }
              />
            )
          }}
        />
      )}
      <div className="dz-hintbar">
        <span className="dz-hint">
          <Kbd combo="Enter" variant="cap" /> 粘贴
        </span>
        <span className="dz-hint">
          <Kbd combo="Shift+Enter" variant="cap" /> 纯文本
        </span>
        <span className="dz-hint">
          <Kbd combo="Mod+Enter" variant="cap" /> 仅复制
        </span>
        <span className="dz-hint">
          <Kbd combo="Space" variant="cap" /> 预览
        </span>
      </div>
    </section>
  )
}

// ---------------------------------------------------------------------------
// 输入

function InputDesign() {
  const t = useT()
  const [text, setText] = useState(INPUT_DRAFT)
  return (
    <section data-testid="design-input">
      <div className="dz-header">
        <span className="dz-title">{t('panel.view.input')}</span>
        <span className="dz-spacer" />
        <LangBar source={null} target="en" onPickTarget={() => {}} detecting={false} />
        <IconButton icon={X} label={t('common.action.close')} shortcut="Escape" />
      </div>
      <TextArea
        variant="bare"
        value={text}
        onChange={setText}
        minRows={3}
        maxRows={10}
        placeholder="输入或粘贴要翻译的文字"
        aria-label={t('panel.view.input')}
      />
      <div className="dz-footer" style={{ marginRight: 0 }}>
        <span className="dz-input-hint">
          <Kbd combo="Enter" variant="cap" /> 换行
        </span>
        <Button variant="primary" size="sm">
          {t('common.action.translate')}
          <Kbd combo="Mod+Enter" variant="inline" />
        </Button>
      </div>
    </section>
  )
}

// ---------------------------------------------------------------------------

export function renderDesignView(id: DesignViewId) {
  return function DesignRender({ maxHeight }: ViewSwitchRenderProps) {
    switch (id) {
      case 'translate-streaming':
        return <TranslateStreaming />
      case 'translate-done':
        return <TranslateDone />
      case 'translate-dict':
        return <TranslateDict />
      case 'translate-error':
        return <TranslateError />
      case 'ocr-recognizing':
        return <OcrRecognizing />
      case 'ocr-done':
        return <OcrDone maxHeight={maxHeight} />
      case 'clipboard-list':
        return <ClipboardDesign query="" maxHeight={maxHeight} />
      case 'clipboard-search':
        return <ClipboardDesign query="wisp" maxHeight={maxHeight} />
      case 'clipboard-empty':
        return <ClipboardDesign query="" empty maxHeight={maxHeight} />
      case 'input':
        return <InputDesign />
    }
  }
}

/** 按载荷类型渲染视觉稿（shell.view-switch 用：视图切换时能看到真实的内容差异） */
export function renderDesignByPayload(props: ViewSwitchRenderProps) {
  switch (props.payload.kind) {
    case 'translate':
      return <TranslateDone />
    case 'clipboard':
      return <ClipboardDesign query="" maxHeight={props.maxHeight} />
    case 'input':
      return <InputDesign />
    case 'ocr':
      return <OcrDone maxHeight={props.maxHeight} />
    case 'toast':
      return null
  }
}

/** 短译文（用于计算宽度的展示载荷） */
export const DESIGN_SOURCE_SHORT = TRANSLATION_ZH
