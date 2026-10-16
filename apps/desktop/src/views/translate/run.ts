/**
 * 翻译请求的状态机：一个视图实例一个 `TranslateRun`，管理「接上已有请求 / 发起新请求 / 取消 / 重试」与
 * StreamEvent 的消费。不依赖 React：视图经 `useSyncExternalStore` 订阅快照，正文经 `bindSink` 直接写进 StreamView。
 *
 * 设计要点：
 * - **流式文本不进状态**：Delta 直接交给 StreamView 的句柄（Writer 每个 rAF 合并一次写 DOM）；快照只在
 *   状态迁移时变化（开始、第一个 Delta、Meta、结束……），整段流只触发个位数次 React 渲染；
 * - **代次**（`gen`）：每次发起 / 接上请求加一，旧请求迟到的事件一律丢弃（连切 3 次风格只有最后一个
 *   在流式；取消之后不再有字节进界面）。编排器不会替同一会话取消旧请求，所以**本状态机发起的**请求只要
 *   过期（被新请求换掉、被停止、视图已关闭），不论它的 request_id 是早就知道、还是之后才随 StartAck / Start 事件
 *   到达，都会补发一次 `cancel`，不留孤儿请求在 Rust 里流完；
 * - **正文代次**（`textGen`）：新请求的第一个 Delta 到达时才换掉旧正文（StreamView 以它为 key 重新挂载），在那之前
 *   旧正文保持显示——风格切换时由视图把它变暗、模糊，宽限期内就到达的结果（命中缓存）不变暗，直接交叉淡入；
 * - **回放**（`attach`）：回放期间到达的 Delta 先缓冲，回放结束后一次性无动画写入，Pinned 首帧据此同步测量。
 *   `stream_attach` 的 invoke 返回之后再等一个宏任务（大于约 8 KB 的 Channel 消息由 Tauri 异步投递，先后不保证）；
 *   一条事件都还没收到时等到第一个 Delta / 终止事件，最多 `REPLAY_SETTLE_MS`；
 * - 思考耗尽：`Done.truncated == max_tokens` 且这次流没有收到任何非空白 Delta → 「模型思考耗尽了输出额度」
 *   （前端判定）；
 * - 词典式：显示用的文字经 `DictFormat` 整理（dict.ts），复制 / 固定用模型的原文（`rawText()`）。
 */
import { api, WispIpcError } from '../../shared/ipc'
import type {
  Lang,
  LlmError,
  ModelRef,
  NoticeKind,
  PinStatus,
  StreamEvent,
  StreamPhase,
  TaskKind,
  TranslateReq,
  Usage,
} from '../../shared/ipc/types'
import { duration, ms } from '../../shared/motion/tokens'
import type { StreamFormat, StreamHandle } from '../../shared/stream/StreamView'
import { DictFormat, formatDict } from './dict'
import { isLocalRequest, subscribeBuffered } from './hub'

export type RunStatus = 'idle' | 'waiting' | 'streaming' | 'done' | 'cancelled' | 'error'

/** 发起请求所需的参数（会话内的临时选择，不写入设置） */
export interface RunParams {
  readonly text: string
  readonly task: TaskKind
  readonly styleId: string
  /** 手动指定的目标语言；null = 自动方向 */
  readonly target: Lang | null
  /** 本会话临时指定的模型；null = 默认模型 */
  readonly model: ModelRef | null
}

export interface RunModel {
  readonly providerId: string
  readonly modelId: string
  readonly label: string
}

export interface RunSnapshot {
  readonly status: RunStatus
  readonly params: RunParams
  readonly requestId: string | null
  readonly gen: number
  /** 正在显示的正文属于哪一代请求（StreamView 的 key）；0 = 还没有正文 */
  readonly textGen: number
  /** 正在显示的正文的任务、排版与是否词典式（第一个 Delta 到达时定格） */
  readonly textTask: TaskKind
  readonly textFormat: StreamFormat
  readonly textDict: boolean
  /** 正在显示的正文是一次性写入的静态内容（Pinned 里已结束的结果、提升译文前后的切换），不是流式写出来的 */
  readonly textStatic: boolean
  /** 正在显示的正文含非空白字符 */
  readonly hasText: boolean
  readonly phase: StreamPhase | null
  readonly model: RunModel | null
  readonly detected: Lang | null
  readonly target: Lang
  /** Meta 告知的是否词典式；null = 还不知道 */
  readonly dict: boolean | null
  readonly lowConfidence: boolean
  readonly elapsedMs: number | null
  readonly usage: Usage | null
  readonly truncated: boolean
  readonly cached: boolean
  readonly notices: readonly NoticeKind[]
  readonly error: LlmError | null
  /**
   * 输出额度被思考耗尽（截断且这次流没有任何非空白文字，显示在状态行）。提升译文时原译文
   * 保留（hasText 仍为真），其余情况正文为空
   */
  readonly thinkingExhausted: boolean
  /** 提升译文：反思返回 OK（没有改动、也没有截断），保留原译文 */
  readonly noImprovement: boolean
  /** 提升译文已完成：正在显示提升后的译文 / 原译文（可以来回切换）；null = 不是提升译文的结果 */
  readonly refined: 'improved' | 'original' | null
  /** 重新请求中、旧正文变暗（宽限期过后才为真；宽限期内到达的结果不变暗） */
  readonly dimmed: boolean
}

export interface RunHost {
  /** 面板会话 id；Pinned 宿主为 0 */
  readonly sessionId: number
  /** 正文排版：按任务、风格与是否词典式决定 */
  formatOf(task: TaskKind, styleId: string, dict: boolean): StreamFormat
  /** Meta 到达之前推断是否词典式 */
  guessDict(params: RunParams): boolean
  /** 面板宿主：`StartAck.width` 非空（新请求要求改宽度） */
  onWidth?(width: number, run: TranslateRun): void
}

export interface StartOptions {
  bypassCache?: boolean
  /** refine：当前译文 */
  draft?: string | null
}

/** 换新请求后多久还没有新内容才把旧正文变暗（命中缓存的结果在这之前就到了，直接交叉淡入） */
const DIM_GRACE_MS = ms(duration.instant)

/**
 * `stream_attach` 返回时一条事件都还没收到：最多再等这么久第一个 Delta / 终止事件（请求还在等首字时就是空日志，
 * 不能一直等；Pinned 的首帧交接另有 1.5 s 兜底）。不是动效时长
 */
const REPLAY_SETTLE_MS = 120

/** 已补发过取消的 request_id 最多记这么多（只用于去重） */
const MAX_CANCELLED = 16

const NON_SPACE = /\S/

/** 命令失败（没能开始）时的错误：优先用 IpcError 携带的 LlmError */
export function toLlmError(error: unknown): LlmError {
  const base: LlmError = {
    code: 'internal',
    subcode: null,
    message: error instanceof Error ? error.message : String(error),
    retryable: true,
    hint: null,
    status: null,
    url: null,
    retry_after_s: null,
    upstream_request_id: null,
  }
  if (!(error instanceof WispIpcError)) return base
  if (error.llm) return error.llm
  if (error.code === 'not_ready') return { ...base, code: 'not_configured', retryable: false }
  if (error.code === 'keychain') return { ...base, code: 'keychain' }
  return base
}

type Listener = () => void

/** 提升译文前 / 后的一份结果：来回切换时整段静态写入，统计随之切换 */
interface SavedResult {
  readonly raw: string
  readonly task: TaskKind
  readonly format: StreamFormat
  readonly dict: boolean
  readonly model: RunModel | null
  readonly elapsedMs: number | null
  readonly usage: Usage | null
  readonly cached: boolean
  readonly notices: readonly NoticeKind[]
}

export class TranslateRun {
  private snap: RunSnapshot
  private readonly listeners = new Set<Listener>()
  private sink: StreamHandle | null = null
  /** 当前正文（textGen）已收到的全部文字（显示用，词典式经过整理）；StreamView 重新挂载时据此整段补写 */
  private text = ''
  /** 当前正文的模型原文（复制、固定用） */
  private raw = ''
  /** 词典式的增量整理（非词典式为 null） */
  private fmt: DictFormat | null = null
  /** 已经交给当前句柄的字符数（其余还在缓冲：新 StreamView 挂载之前、回放期间） */
  private delivered = 0
  /** 当前句柄已经写过东西：再绑定新句柄时整段补写（无动画） */
  private sinkUsed = false
  /** 缓冲的文字来自回放：写入时不做动画（一次性 reset） */
  private pendingReplay = false
  /** 正文已结束（done / cancelled / error）：绑定时补一次 end() */
  private textEnded = false
  private replaying = false
  /** 回放期间已经收到过 Delta / 终止事件 */
  private replayHeard = false
  /** 回放在等第一个 Delta / 终止事件 */
  private replayWaiter: (() => void) | null = null
  private unsubscribe: (() => void) | null = null
  private dimTimer: ReturnType<typeof setTimeout> | null = null
  /** 已转给 Pinned / 新视图实例的请求：之后关闭面板、过期都不再取消它 */
  private releasedId: string | null = null
  /** 面板正在关闭：之后才拿到 request_id 的请求立即取消 */
  private closing = false
  private readonly cancelled = new Set<string>()
  /** 提升译文：反思前的结果、提升后的结果 */
  private beforeRefine: SavedResult | null = null
  private afterRefine: SavedResult | null = null

  constructor(
    private host: RunHost,
    init: { params: RunParams; detected: Lang | null; target: Lang; dict: boolean | null },
  ) {
    this.snap = {
      status: 'idle',
      params: init.params,
      requestId: null,
      gen: 0,
      textGen: 0,
      textTask: init.params.task,
      textFormat: host.formatOf(init.params.task, init.params.styleId, init.dict ?? false),
      textDict: init.dict ?? false,
      textStatic: false,
      hasText: false,
      phase: null,
      model: null,
      detected: init.detected,
      target: init.target,
      dict: init.dict,
      lowConfidence: false,
      elapsedMs: null,
      usage: null,
      truncated: false,
      cached: false,
      notices: [],
      error: null,
      thinkingExhausted: false,
      noImprovement: false,
      refined: null,
      dimmed: false,
    }
  }

  /** 换宿主回调（视图每次提交后传入读到最新风格表 / props 的回调） */
  setHost(host: RunHost): void {
    this.host = host
  }

  // ----- 订阅（useSyncExternalStore）

  readonly subscribe = (listener: Listener): (() => void) => {
    this.listeners.add(listener)
    return () => this.listeners.delete(listener)
  }

  readonly getSnapshot = (): RunSnapshot => this.snap

  private update(patch: Partial<RunSnapshot>): void {
    this.snap = { ...this.snap, ...patch }
    for (const l of this.listeners) l()
  }

  /** 通知一次订阅者（不改状态）：需要一次提交来触发视图的布局 effect 时用 */
  touch(): void {
    this.update({})
  }

  /** 请求是否还在进行（等待首字或流式中） */
  get active(): boolean {
    return this.snap.status === 'waiting' || this.snap.status === 'streaming'
  }

  /** 当前正文的模型原文（复制、固定；词典式不含显示用的整理标记） */
  rawText(): string {
    return this.raw
  }

  // ----- 正文

  /**
   * StreamView（key = textGen）挂载后绑定句柄：把缓冲的文字写进去（回放来的一次性写入、不做动画），已结束的补 end()。
   * 句柄为 null 表示卸载
   */
  bindSink(textGen: number, handle: StreamHandle | null): void {
    if (textGen !== this.snap.textGen) return
    if (!handle) {
      this.sink = null
      return
    }
    if (handle !== this.sink) {
      // 新句柄（第一次挂载，或 StreamView 重新挂载后内容为空）：整段写入
      const rebind = this.sinkUsed
      this.sink = handle
      this.sinkUsed = false
      if (this.replaying) {
        // 回放还没结束：先不写，回放结束时整段同步写入（flush）
        this.delivered = 0
        this.pendingReplay = true
        return
      }
      if (this.text) {
        if (rebind || this.pendingReplay) handle.reset(this.text)
        else handle.append(this.text)
        this.sinkUsed = true
      }
      this.delivered = this.text.length
      this.pendingReplay = false
      if (this.textEnded) handle.end()
      return
    }
    this.flush()
  }

  /** 把缓冲的文字交给当前句柄（回放结束时一次性、无动画） */
  private flush(): void {
    const sink = this.sink
    if (!sink || this.delivered >= this.text.length || this.replaying) return
    if (this.pendingReplay && this.delivered === 0) sink.reset(this.text)
    else sink.append(this.text.slice(this.delivered))
    this.sinkUsed = true
    this.delivered = this.text.length
    this.pendingReplay = false
    if (this.textEnded) sink.end()
  }

  private write(chunk: string): void {
    this.raw += chunk
    const text = this.fmt ? this.fmt.push(chunk) : chunk
    if (!text) return
    this.text += text
    // 回放期间一律缓冲：回放结束时一次性写入（无动画，Pinned 首帧可以同步测量）
    if (this.sink && !this.replaying) {
      this.sink.append(this.text.slice(this.delivered))
      this.sinkUsed = true
      this.delivered = this.text.length
    }
  }

  private endText(): void {
    if (this.textEnded) return
    const tail = this.fmt?.end() ?? ''
    if (tail) {
      this.text += tail
      if (this.sink && !this.replaying) {
        this.sink.append(this.text.slice(this.delivered))
        this.sinkUsed = true
        this.delivered = this.text.length
      }
    }
    this.textEnded = true
    if (this.sink && this.delivered >= this.text.length) this.sink.end()
  }

  /** 新请求的第一个 Delta（或结束时仍没有任何 Delta）：换掉旧正文 */
  private beginText(gen: number, task: TaskKind = this.snap.params.task): Partial<RunSnapshot> {
    const s = this.snap
    const dict = s.dict ?? this.host.guessDict(s.params)
    const textDict = dict && task !== 'polish' && task !== 'proofread'
    this.sink = null
    this.text = ''
    this.raw = ''
    this.fmt = textDict ? new DictFormat() : null
    this.delivered = 0
    this.sinkUsed = false
    this.pendingReplay = this.replaying
    this.textEnded = false
    this.clearDim()
    return {
      textGen: gen,
      textTask: task,
      textFormat: this.host.formatOf(task, s.params.styleId, dict),
      textDict,
      textStatic: false,
      hasText: false,
      dimmed: false,
    }
  }

  /** 一次性写入一整段静态正文（新的正文代次） */
  private setStaticText(raw: string, begin: Partial<RunSnapshot>): Partial<RunSnapshot> {
    this.raw = raw
    this.text = begin.textDict ? formatDict(raw) : raw
    this.fmt = null
    this.pendingReplay = true
    this.textEnded = true
    return { ...begin, textStatic: true, hasText: NON_SPACE.test(raw) }
  }

  // ----- 发起 / 接上

  /**
   * 事件监听者。`owned`：请求是本状态机发起的（translate_start）——过期之后它的 Start 才到，说明 ack 之前就被换掉 /
   * 停止了，补发取消。接上的请求（attach）过期时不取消：可能是 StrictMode 的重新挂载、或新的视图实例接着显示它
   */
  private listen(gen: number, owned: boolean): (event: StreamEvent) => void {
    return (event) => {
      if (gen !== this.snap.gen) {
        if (owned && event.type === 'start') this.cancelOrphan(event.request_id)
        return
      }
      if (
        this.replaying &&
        (event.type === 'delta' || event.type === 'done' || event.type === 'error')
      ) {
        this.replayHeard = true
        this.replayWaiter?.()
      }
      this.onEvent(gen, event)
    }
  }

  private reset(gen: number, patch: Partial<RunSnapshot>): void {
    this.unsubscribe?.()
    this.unsubscribe = null
    this.replayWaiter = null
    this.clearDim()
    this.update({
      gen,
      requestId: null,
      status: 'waiting',
      phase: null,
      elapsedMs: null,
      usage: null,
      truncated: false,
      cached: false,
      notices: [],
      error: null,
      thinkingExhausted: false,
      noImprovement: false,
      refined: null,
      dimmed: false,
      ...patch,
    })
  }

  /**
   * 接上一个已有的请求（面板载荷里 Rust 已经发出的请求、输入视图交接过来的请求、Pinned 转移来的请求）：
   * 回放全部事件后继续接收。resolve 为是否成功（失败时状态不变，由调用方决定退路）；成功时 resolve 在回放写入之后
   */
  attach(requestId: string): Promise<boolean> {
    const gen = this.snap.gen + 1
    this.reset(gen, { requestId })
    const listener = this.listen(gen, false)
    this.replaying = true
    this.replayHeard = false
    const buffered = subscribeBuffered(requestId, listener)
    if (buffered) {
      // 本窗口里的缓冲：同步回放完毕，没有异步投递的问题
      this.unsubscribe = buffered
      this.endReplay(gen)
      return Promise.resolve(true)
    }
    return api.streamAttach(requestId, listener).then(
      () => this.settleReplay(gen).then(() => true),
      () => {
        if (gen === this.snap.gen) this.replaying = false
        return false
      },
    )
  }

  /** invoke 返回之后：等异步投递的回放事件落定再结束回放（见模块注释） */
  private settleReplay(gen: number): Promise<void> {
    return new Promise((resolve) => {
      let timer: ReturnType<typeof setTimeout> | null = null
      const finish = () => {
        if (timer !== null) clearTimeout(timer)
        timer = null
        if (gen === this.snap.gen) this.replayWaiter = null
        this.endReplay(gen)
        resolve()
      }
      const nextTask = () => {
        if (timer !== null) clearTimeout(timer)
        timer = setTimeout(finish, 0)
      }
      if (this.replayHeard || gen !== this.snap.gen) {
        nextTask()
        return
      }
      this.replayWaiter = () => {
        this.replayWaiter = null
        nextTask()
      }
      timer = setTimeout(finish, REPLAY_SETTLE_MS)
    })
  }

  /** 回放结束：缓冲的文字已经可以一次性写入 */
  private endReplay(gen: number): void {
    if (gen !== this.snap.gen || !this.replaying) return
    this.replaying = false
    this.flush()
    // 通知一次：视图在这次提交里（Pinned 用 flushSync）绑定新的 StreamView 并写入
    this.update({})
  }

  /**
   * 只改请求参数、不发请求（长文本确认期间选风格 / 任务 / 模型 / 方向：确认后按新参数发起）
   */
  configure(patch: Partial<RunParams>): void {
    if (this.active) return
    const params: RunParams = { ...this.snap.params, ...patch }
    this.update({ params, target: params.target ?? this.snap.target })
  }

  /**
   * 发起新请求：先取消还在进行的旧请求，旧正文保留到新内容到达（宽限期后变暗）。
   * `patch` 是对请求参数的修改（换风格、换目标语言、换模型、换任务）
   */
  start(patch: Partial<RunParams> = {}, options: StartOptions = {}): void {
    this.cancelActive()
    const gen = this.snap.gen + 1
    const params: RunParams = { ...this.snap.params, ...patch }
    const keepOld = this.snap.textGen > 0 && this.snap.hasText
    this.beforeRefine = params.task === 'refine' ? this.save() : null
    this.afterRefine = null
    this.replaying = false
    this.reset(gen, {
      params,
      target: params.target ?? this.snap.target,
      // 换风格 / 换任务后是否词典式要等新的 Meta；目标语言不变时方向检测结果仍然有效
      dict: null,
      lowConfidence: false,
      model: null,
    })
    // 宽限期过后还没有新内容：旧正文变暗（提升译文的反思阶段同样）
    if (keepOld) {
      this.dimTimer = setTimeout(() => {
        this.dimTimer = null
        if (gen === this.snap.gen && this.snap.textGen !== gen && this.active)
          this.update({ dimmed: true })
      }, DIM_GRACE_MS)
    }
    const req: TranslateReq = {
      session_id: this.host.sessionId,
      text: params.text,
      task: params.task,
      style_id: params.styleId,
      target_lang: params.target,
      model_ref: params.model,
      dict_mode: 'auto',
      bypass_cache: options.bypassCache ?? false,
      draft: options.draft ?? null,
      // 翻译视图里的原文要么不长、要么用户已经确认过（长文本卡片的「继续」、确认之后的换风格 / ⌘R / 改目标），
      // 不再让 Rust 拦一次
      confirm_long: true,
    }
    api.translateStart(req, this.listen(gen, true)).then(
      (ack) => {
        // 已经过期（换了请求、停止了、视图已卸载）或面板正在关闭：没有人再要这个请求
        if (gen !== this.snap.gen || this.closing) {
          this.cancelOrphan(ack.request_id)
          return
        }
        if (this.snap.requestId === null) this.update({ requestId: ack.request_id })
        if (ack.width !== null) this.host.onWidth?.(ack.width, this)
      },
      (error: unknown) => {
        if (gen !== this.snap.gen) return
        this.fail(toLlmError(error))
      },
    )
  }

  /**
   * 显示静态结果（Pinned 里已结束的请求：PinState.result_text，模型原文）。`status` 是面板交接来的结构化状态行
   * （PinState.status）：模型、耗时、用量、缓存、取消、截断逐项还原，底栏与面板里同一个
   * StatusLine；`model` 是这次结果实际用的模型（PinState.model_ref），之后重新生成沿用它
   */
  showStatic(text: string, status: PinStatus | null = null, model: ModelRef | null = null): void {
    const gen = this.snap.gen + 1
    this.reset(gen, { status: status?.cancelled ? 'cancelled' : 'done' })
    const begin = this.setStaticText(text, this.beginText(gen))
    const truncated = status?.truncated === 'max_tokens'
    this.update({
      ...begin,
      ...(status
        ? {
            model: status.model_label
              ? {
                  providerId: model?.provider_id ?? '',
                  modelId: model?.model_id ?? '',
                  label: status.model_label,
                }
              : null,
            elapsedMs: status.elapsed_ms,
            usage: status.usage,
            cached: status.cached,
            truncated,
            thinkingExhausted: truncated && begin.hasText !== true,
          }
        : null),
    })
  }

  /** 提升译文完成后：在原译文与提升后的译文之间切换（可以回到原译文对比 / 撤销） */
  toggleRefined(): void {
    const s = this.snap
    if (s.refined === null || this.active) return
    const target = s.refined === 'improved' ? this.beforeRefine : this.afterRefine
    if (!target) return
    const gen = s.gen + 1
    this.reset(gen, { status: 'done', params: s.params, target: s.target })
    const begin = this.beginText(gen, target.task)
    this.update({
      ...this.setStaticText(target.raw, {
        ...begin,
        textFormat: target.format,
        textDict: target.dict,
      }),
      model: target.model,
      elapsedMs: target.elapsedMs,
      usage: target.usage,
      cached: target.cached,
      notices: target.notices,
      refined: s.refined === 'improved' ? 'original' : 'improved',
    })
  }

  /** 接不上 / 发不出请求：以错误卡片结束 */
  fail(error: LlmError): void {
    this.replaying = false
    this.flush()
    this.clearDim()
    this.endText()
    this.update({ status: 'error', error, dimmed: false, phase: null })
  }

  /** 用户点「停止」/ ⌘.：只停止不关闭，保留已收到的部分，显示「已取消」，不再等 usage */
  stop(): void {
    if (!this.active) return
    // request_id 已知时立即取消；还不知道（StartAck / Start 都没到）时，由过期代次的 ack / Start 补发
    this.cancelActive()
    // 回放还没结束就停止：已收到的部分照样写出来
    this.replaying = false
    this.replayWaiter = null
    this.flush()
    // 代次加一：之后到达的任何事件（包括 Error{cancelled} 与 usage）都丢弃
    const gen = this.snap.gen + 1
    this.unsubscribe?.()
    this.unsubscribe = null
    this.clearDim()
    this.endText()
    this.update({ gen, status: 'cancelled', phase: null, dimmed: false })
  }

  /**
   * 面板关闭（Esc / ✕ / ⌘W）：一并取消，并**冻结界面**——视图正在淡出（Native 帧模式下内容要保留到窗口隐藏），
   * 之后到达的事件（编排器几毫秒内回的 Error{cancelled}、usage……）一律丢弃，底栏不会在淡出中从 [停止] 换成完成态的
   * 操作、状态行也不会跳成「已取消」。之后才拿到 request_id 的请求（ack / Start 晚到）同样取消
   */
  cancelForClose(): void {
    this.closing = true
    if (this.active) this.cancelActive()
    this.freeze()
  }

  /**
   * 当前请求的所有权已转给 Pinned 或新的视图实例：之后关闭面板、过期都不再取消它。固定失败时传 false 收回
   */
  release(released = true): void {
    this.releasedId = released ? this.snap.requestId : null
    // 固定失败、面板已经在关闭：没有人再要这个请求了
    if (!released && this.closing) this.cancelActive()
  }

  /**
   * 视图卸载（含 StrictMode 的「卸载 → 立即重新挂载」）：丢弃之后到达的事件。**不取消已接上的请求**——卸载不一定是
   * 关闭（StrictMode 的模拟卸载、Pinned 接管）；真正的关闭走 `cancelForClose`，会话结束时编排器也会收尾。
   * 本状态机发起、还没拿到 request_id 的请求过期后由 ack / Start 补发取消。不清理订阅者：同一实例重新
   * 挂载后照常工作
   */
  suspend(): void {
    this.freeze()
  }

  /**
   * 丢弃之后到达的全部事件、快照保持不变：代次加一但**不通知订阅者**（界面停在当前这一帧）。本状态机发起的过期请求
   * 仍由 ack / 过期代次的 Start 补发取消（listen 的 owned 分支）
   */
  private freeze(): void {
    this.clearDim()
    this.unsubscribe?.()
    this.unsubscribe = null
    this.replaying = false
    this.replayWaiter = null
    this.snap = { ...this.snap, gen: this.snap.gen + 1 }
  }

  private cancelActive(): void {
    const id = this.snap.requestId
    if (this.active && id) this.cancelOrphan(id)
  }

  /** 取消一个不再需要的请求（本地 id、已转出去的、已经取消过的除外） */
  private cancelOrphan(id: string): void {
    if (isLocalRequest(id) || id === this.releasedId || this.cancelled.has(id)) return
    this.cancelled.add(id)
    if (this.cancelled.size > MAX_CANCELLED) {
      const oldest = this.cancelled.values().next().value
      if (oldest !== undefined) this.cancelled.delete(oldest)
    }
    void api.cancel(id).catch(() => undefined)
  }

  private clearDim(): void {
    if (this.dimTimer !== null) clearTimeout(this.dimTimer)
    this.dimTimer = null
  }

  private save(): SavedResult {
    const s = this.snap
    return {
      raw: this.raw,
      task: s.textTask,
      format: s.textFormat,
      dict: s.textDict,
      model: s.model,
      elapsedMs: s.elapsedMs,
      usage: s.usage,
      cached: s.cached,
      notices: s.notices,
    }
  }

  // ----- 事件

  private onEvent(gen: number, e: StreamEvent): void {
    const s = this.snap
    switch (e.type) {
      case 'start':
        this.update({
          requestId: s.requestId ?? e.request_id,
          model: { providerId: e.provider_id, modelId: e.model_id, label: e.model_label },
        })
        // 面板已经在关闭：ack 之前就开始的请求在这里第一次知道 id
        if (this.closing && s.requestId === null) this.cancelOrphan(e.request_id)
        return
      case 'meta':
        this.update({
          detected: e.detected_source_lang ?? s.detected,
          target: e.target_lang ?? s.target,
          dict: e.dict_mode ?? s.dict,
          lowConfidence: e.low_confidence ?? false,
        })
        return
      case 'phase':
        this.update({ phase: e.phase })
        return
      case 'notice':
        if (!s.notices.includes(e.kind)) this.update({ notices: [...s.notices, e.kind] })
        return
      case 'delta': {
        let patch: Partial<RunSnapshot> | null = null
        if (s.textGen !== gen) patch = { ...this.beginText(gen), status: 'streaming' }
        else if (s.status === 'waiting') patch = { status: 'streaming' }
        // 离线模型加载完、开始出字：「正在加载离线模型…」到此为止（提升译文的两个阶段照常保留）
        if (s.phase === 'loading_model' || s.phase === 'queued') patch = { ...patch, phase: null }
        const hasText = patch?.hasText ?? s.hasText
        if (!hasText && NON_SPACE.test(e.text)) patch = { ...patch, hasText: true }
        // 先换正文代次（新 StreamView 挂载前的文字进缓冲），再写
        if (patch) this.update(patch)
        this.write(e.text)
        return
      }
      case 'done': {
        const truncated = e.truncated === 'max_tokens'
        const hadText = s.textGen === gen && s.hasText
        // 提升译文没有任何 Delta：保留原译文——反思返回 OK 是「无需改进」；截断是思考耗尽（只在状态行
        // 提示），两者不会同时成立
        const keepOld = s.params.task === 'refine' && s.textGen !== gen
        const noImprovement = keepOld && !truncated
        // 其余没有任何 Delta 的结束：清空旧正文，不让上一个风格的译文冒充这次的结果
        const begin = s.textGen !== gen && !keepOld ? this.beginText(gen) : null
        this.unsubscribe?.()
        this.unsubscribe = null
        this.clearDim()
        this.endText()
        const improved = s.params.task === 'refine' && hadText && this.beforeRefine !== null
        this.update({
          ...begin,
          status: 'done',
          phase: null,
          dimmed: false,
          elapsedMs: e.elapsed_ms,
          usage: e.usage,
          truncated,
          cached: e.cached,
          thinkingExhausted: truncated && !hadText,
          noImprovement,
          refined: improved ? 'improved' : null,
        })
        if (improved) this.afterRefine = this.save()
        return
      }
      case 'error': {
        this.unsubscribe?.()
        this.unsubscribe = null
        this.clearDim()
        this.endText()
        if (e.code === 'cancelled') {
          this.update({ status: 'cancelled', phase: null, dimmed: false })
          return
        }
        const error: LlmError = {
          code: e.code,
          subcode: e.subcode,
          message: e.message,
          retryable: e.retryable,
          hint: e.hint,
          status: e.status,
          url: e.url,
          retry_after_s: e.retry_after_s,
          upstream_request_id: e.upstream_request_id,
        }
        this.update({ status: 'error', error, phase: null, dimmed: false })
        return
      }
    }
  }
}
