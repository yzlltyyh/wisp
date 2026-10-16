# 架构

本文面向贡献者，说明 Wisp 由哪些部分组成、各部分如何协作，以及几条贯穿全局的设计约束。构建、测试与发版见[开发指南](development.md)，模型服务的接入细节见[模型服务](providers.md)。

## 目录

- [总览](#总览)
- [仓库结构](#仓库结构)
- [Rust crate](#rust-crate)
- [桌面应用（src-tauri）](#桌面应用src-tauri)
- [前端](#前端)
- [IPC](#ipc)
- [一次划词翻译的路径](#一次划词翻译的路径)
- [窗口体系](#窗口体系)
- [平台层](#平台层)
- [多设备同步](#多设备同步)
- [离线模型与推理进程](#离线模型与推理进程)
- [数据与配置](#数据与配置)
- [安全与隐私](#安全与隐私)
- [性能设计](#性能设计)
- [Cargo feature](#cargo-feature)

## 总览

Wisp 是一个 [Tauri 2](https://tauri.app) 应用：一个常驻的 Rust 进程，加上系统自带的 WebView（macOS 的 WKWebView、Windows 的 WebView2、Linux 的 WebKitGTK）负责界面。

```text
Wisp 进程（Rust，tokio 2 个 worker）
  app            启动顺序、运行时、日志、外观、权限、诊断导出、单实例与命令行
  hotkeys        全局快捷键的注册与回退、冲突检测、录制、扩展触发
  orchestrator   翻译 / 输入 / OCR / 截图直接翻译四条流程的状态机，流式请求的登记与回放
  windows        WindowManager：面板显示管线、定位与尺寸、固定窗口、悬浮剪贴板、截图覆盖层、设置窗口、wisp:// 协议
  clipboard_svc  剪贴板监听、记录、检索、识别与变换、统一的粘贴管线、连续粘贴
  settings       settings.json 等配置文件、热加载、钥匙串、模型能力缓存、同步凭据
  sync_svc       多设备同步：改动捕获、触发与去抖、同步状态广播（同步关闭时不打开任何资源）
  models_svc     离线模型：库存、下载队列、推理进程的槽位与生命周期（没用到离线模型时不创建任何资源）
  tray · updater · ipc · bus
      │ 调用
      ▼
  wisp-llm · wisp-prompts · wisp-clipboard · wisp-capture · wisp-platform · wisp-sync · wisp-local · wisp-text · wisp-types

        │ models_svc 按需启动、闲置释放（没用到离线模型时不存在）
        ▼
推理进程：ORT 工作进程（wisp-desktop --wisp-worker ort）· llama-server（由守护进程或 Job Object 托管）

        ▲ │  Tauri IPC：命令（invoke）· 事件 · Channel 流 · wisp:// 资源
        │ ▼

WebView（React 19 + Motion + Tailwind CSS 4）
  panel（翻译 · OCR · 剪贴板 · 输入 · 提示）· settings / onboarding · pinned · clipfloat · overlay（原生 TS）
```

几条贯穿全局的约束：

- **Rust 是事实来源**。流程状态、配置、剪贴板历史、API Key 都在 Rust 进程里；WebView 只负责呈现，随时可以销毁重建。
- **平台差异收敛在少数位置**。`#[cfg(target_os)]` 只出现在 `wisp-platform`、`wisp-capture`、`apps/desktop/src-tauri/src/windows/native/` 以及 `lib.rs` 的插件注册里，其余代码通过 trait 调用平台能力。
- **非桌面 crate 不依赖 Tauri**。`crates/*` 都可以脱离 WebView 单独测试和做基准；`scripts/check-contract.mjs` 检查这条边界，以及 crate 之间的[依赖白名单](#rust-crate)。
- **空闲时不做事**。没有常驻的周期定时器，窗口按需创建，HTTP 客户端、SQLite 连接、钥匙串句柄都在第一次使用时才初始化；同步只由本机改动与用户活动触发，不轮询；离线模型只在用到时才读盘、起进程，闲置后释放（见[性能设计](#性能设计)）。

## 仓库结构

```text
wisp/
├── apps/desktop/                 Tauri 应用
│   ├── src/                      前端（React + TypeScript）
│   ├── src-tauri/                Rust 入口（crate 名 wisp-desktop）
│   └── panel.html / settings.html / pinned.html / clipfloat.html / overlay.html   五个页面入口
├── crates/
│   ├── wisp-types/               共享数据类型（IPC、配置、持久化）
│   ├── wisp-text/                确定性的纯文本规则：文字分类、片段与跨行拼接、列表项判定、空白整理（不依赖任何 crate）
│   ├── wisp-llm/                 四种协议的流式客户端
│   ├── wisp-prompts/             翻译与 OCR 的提示词、风格、语言检测、后处理
│   ├── wisp-clipboard/           剪贴板历史存储（SQLite + FTS5）
│   ├── wisp-capture/             屏幕捕获、裁剪、编码
│   ├── wisp-platform/            操作系统集成层（三端实现）
│   ├── wisp-sync/                多设备同步：端到端加密、WebDAV、本机副本与合并
│   └── wisp-local/               离线模型：目录、下载与校验、本机存储、推理进程的托管、本地与 PaddleOCR 云端的文字识别
├── vendor/                       通过 [patch.crates-io] 引入并打了补丁的上游 crate（wry、tao、tray-icon、notify、oar-ocr / oar-ocr-core）
├── scripts/                      检查、版本号、性能测量、截图、发布脚本
├── docs/                         文档
└── .github/                      CI 与发布工作流
```

Cargo workspace 的成员是 `crates/*` 与 `apps/desktop/src-tauri`；pnpm workspace 包含仓库根与 `apps/*`。依赖版本由 `Cargo.lock` 与 `pnpm-lock.yaml` 锁定，Tauri 相关的 crate 与前端 npm 包在清单里写精确版本，Rust 工具链由 `rust-toolchain.toml` 锁定。

## Rust crate

```text
wisp-types  ←── 除 wisp-text 外的 crate 都依赖它（纯数据，只依赖 serde）
wisp-text   ←── 需要共用文本规则的 crate 依赖它（确定性的纯文本规则，不依赖任何 crate）
wisp-llm       → wisp-types
wisp-prompts   → wisp-types、wisp-text（eval feature 另依赖 wisp-llm）
wisp-clipboard → wisp-types、wisp-text
wisp-capture   → wisp-types
wisp-platform  → wisp-types
wisp-sync      → wisp-types
wisp-local     → wisp-types、wisp-text（`ort` feature 另依赖 ort 与 oar-ocr）
wisp-desktop   → 以上全部 + tauri
```

crate 之间的横向依赖只允许指向 `wisp-types` 与 `wisp-text`（`wisp-prompts` 的 `eval` feature 例外），这两个 crate 不依赖任何工作区 crate；`wisp-text` 不依赖任何 crate，只放确定性的纯文本规则（同样的输入总是同样的输出，不做 I/O），版面与几何由调用方处理。`scripts/check-contract.mjs` 按这份白名单检查每个 crate 的直接依赖（含 dev / build 依赖）。

| crate | 职责 | 不负责 |
| --- | --- | --- |
| `wisp-types` | 所有跨 crate、跨进程（IPC）与持久化用到的数据类型及其默认值；事件名常量；宽松的配置解析 | 业务逻辑 |
| `wisp-text` | 只看文字本身、要在多处保持一致的确定性规则：字符按书写习惯分类（汉字、假名、全角形式不用空格分词，谚文与拉丁文一样用空格）；两段文字拼接时是否加空格、跨行续接（行尾断词还原、东亚标点直接相连、中英之间按全文惯例补空格）、列表项判定（OCR 的文本行拼装与剪贴板「合并为一行」共用）；识别结果的默认整理（统一换行符、去掉行尾空白、连续空行压成一个、去掉首尾空行，整段与流式两个版本结果相同）；去掉全部空行（剪贴板「粘贴为…」用） | 版面与几何（视觉行、分栏、缩进、段落边界由调用方判断）；I/O；任何依赖 |
| `wisp-llm` | OpenAI Chat Completions、OpenAI Responses、Anthropic Messages、Google Gemini 四种协议的请求构造与 SSE 流式解析；`<think>` 标签剥离；错误归一化；Base URL 规范化；模型列表与视觉能力判定；连接测试；参数降级与能力缓存；超时、重试、取消；16 ms 合并器；服务模板（含 PaddleOCR 的模板；`paddle_ocr` 协议只是服务商的标记，所有 LLM 路径对它直接返回「未配置」）；`chat_template_kwargs` 方式的关闭思考 | 提示词内容、Key 的存储；PaddleOCR 的请求（在 `wisp-local`） |
| `wisp-prompts` | 内置风格与用户覆盖的合并；模板渲染；翻译、词典式、润色、校对、提升译文、OCR 的提示词装配；语言检测与翻译方向；术语匹配；token 估算；流式安全的后处理（OCR 逐行与段落模式最后做默认整理）；缓存键；离线翻译（Index-Translate）的官方提示词格式（`PromptProfile::IndexTranslate`：单条 user 消息、中文语言名、术语与风格写成约束）；评测运行器（`eval` feature） | 发请求 |
| `wisp-clipboard` | SQLite（WAL）的打开与迁移；记录管线（隐私规则 → 规范化 → 去重 → 验证码的到期时间 → 写入 → 缓存）；类型推断；疑似密钥检测与应用排除名单；FTS5 trigram 检索与高亮（含收藏的名称）；收藏与命名；保留策略与清理；图片与缩略图文件；最近条目的内存缓存；识别历史；`smart/` 下的智能识别：识别器注册表（`recognize.rs`，电话、邮箱、链接、验证码、单号、金额、证件号等，每种一个 `Recognizer` 实现并登记）、变换注册表（`transform.rs`，「粘贴为…」的各项，每种一个 `Transform`）与验证码检测（`otp.rs`，记录时据此定到期时间） | 读取系统剪贴板（属于平台层）；识别结果不落库，按需计算 |
| `wisp-capture` | 并行捕获所有显示器（macOS 用 ScreenCaptureKit，Windows 用 xcap，X11 用 MIT-SHM / GetImage）；逻辑坐标到物理像素的换算与裁剪；按长边缩放；PNG / JPEG 编码与体积回退；窗口列表 | 覆盖层窗口（属于桌面应用） |
| `wisp-platform` | 取词、粘贴、焦点、权限、应用信息、显示器与外观、剪贴板读写与监听、外部点击、快捷键环境、输入钩子与浮标的 trait 及三端实现；系统信息（内存、性能核、磁盘空间）、内存压力监听、受托管的子进程（守护进程、Job Object、孤儿清扫），见[平台层](#平台层) | 窗口的创建与绘制 |
| `wisp-local` | 离线模型，见[离线模型与推理进程](#离线模型与推理进程)：编译进二进制的模型与运行时目录（文件、大小、SHA-256、下载地址）；`models/` 的存储布局与安装记录；断点续传、多源、校验与解包的下载器；设备适配；llama-server 的参数、启动与就绪判定；ORT 工作进程（入口与主进程侧客户端、帧协议）；PP-OCRv6 识别、版面检测、PaddleOCR-VL 文档流水线、文本行拼装（几何部分，文字规则在 `wisp-text`）、OTSL 表格；PaddleOCR 云端客户端；引擎结果的流式包装；假引擎（`fake` feature） | 决定何时加载与释放（属于桌面应用的 `models_svc`）；平台相关的进程托管（由桌面应用注入 `Launcher`，实现在 `wisp-platform`） |
| `wisp-sync` | 多设备同步，见[多设备同步](#多设备同步)：加密（PBKDF2 主密钥、HKDF 子密钥、ChaCha20-Poly1305 封装与分块流式封装）；混合逻辑时钟；记录、日志段与 head 的编解码；`Remote` trait、WebDAV 客户端（reqwest + quick-xml）与测试用的内存远端；本机副本 `sync.db`；settings / styles / glossary 的投影与反投影；连接、同步循环、压缩与清除服务器数据 | 读写本机的配置、剪贴板与钥匙串（由桌面应用实现 `LocalStore` 提供）；决定何时同步 |

提示词模板的正文在 `crates/wisp-prompts/templates/`，以 `include_str!` 编入二进制；内置风格的权威定义在代码里，用户的修改只保存与默认值不同的字段。

## 桌面应用（src-tauri）

`apps/desktop/src-tauri/src/lib.rs` 只做装配：运行时、插件、`wisp://` 协议、setup 与命令注册。`run()` 的第一步是判断子进程模式：`argv[1]` 为 `--wisp-worker` 或 `--wisp-guard` 时直接执行对应的入口并退出，不初始化 Tauri、日志与单实例（见[离线模型与推理进程](#离线模型与推理进程)）。服务之间不互相持有引用，统一从 `state.rs` 的 `Deps` 构造，运行时通过 `app.state::<AppState>()` 取得彼此，避免循环依赖。

| 模块 | 内容 |
| --- | --- |
| `app/` | 启动顺序（托盘就绪之后的工作全部异步）、tokio 运行时与 WebView 创建前的环境预检、路径、日志、外观、托盘与通知的文案（`i18n.rs`）、权限状态、开机自启、单实例与 `--trigger` 命令行、诊断导出、性能埋点、冒烟模式、设置变化的统一副作用（`reactor.rs`） |
| `bus.rs` | 应用内命令总线：托盘、快捷键、扩展触发、命令行、引导页把动作投递到这里，由 `orchestrator` 消费 |
| `orchestrator/` | 每次由快捷键等发起的流程是一个状态机（`fsm.rs` 的「状态 × 事件」转移表）：取词 → 未就绪检查 → 语言与方向 → 词典式判定 → 长文本确认 → 缓存 → 请求 → 后处理 → 合并 → 推送。`registry.rs` 保存每个流的事件日志，支持回放与所有权转移（面板固定成独立窗口时流不中断）；`cache.rs` 是翻译结果的 LRU 缓存；`captures.rs` 管理截图的冻结帧与裁剪图（离线引擎直接拿缩放后的像素，不经 PNG 编解码）；`env.rs` 是与外界的接缝，单元测试用内存实现驱动状态机。OCR 的目标分两种：LLM（视觉模型），或引擎（离线的 PP-OCRv6 / PaddleOCR-VL、PaddleOCR 云端）；引擎不能直接翻译，截图直接翻译时分识别、翻译两步。离线翻译经保留的合成服务商 `@offline` 走 wisp-llm，`TauriEnv::llm_stream` 是它唯一的路由点。模型的选择顺序：本次显式指定 > 本机的 `offline.*` > 风格绑定 > `defaults.*` |
| `windows/` | `WindowManager` 与面板显示管线（见[窗口体系](#窗口体系)）；`float.rs` 是悬浮剪贴板；`native/` 是窗口层的平台实现 |
| `hotkeys/` | 由设置算出每个动作的候选组合并按「首选 → 备选 → 留空」注册；分层冲突检测（静态表在 `data/`）；录制与测试；全局 Esc；扩展触发（浮标、双击修饰键、鼠标键） |
| `clipboard_svc/` | 监听回调 → 工作线程读取 → `wisp-clipboard` 记录 → 推送 `clipboard:changed`（悬浮剪贴板打开时同时推给它）；统一的粘贴管线（见下文）；连续粘贴（`queue.rs`）；条目检视的缓存与验证码到期（`smart.rs`，到期用一次性任务，读取时也过滤已过期的条目）；拖出数据（`drag.rs`）；来源应用图标；空闲清理；同步的接入点（本机改动的 sink、远端改动的批量应用） |
| `settings/` | `settings.json`、`styles.json`、`glossary.json`、`state.json` 的读写（原子写入、JSON Merge Patch、文件监听热加载）；钥匙串；模型能力缓存；同步的接入点（改动钩子、`apply_synced`、同步凭据） |
| `models_svc/` | 离线模型的桌面端：库存（第一次需要时才读）、下载队列（一次一个文件，暂停、取消、退出时转为暂停）、三个推理槽位与闲置定时器、内存压力监听、设置订阅（换模型、关 GPU、改闲置时长）、启动时的孤儿清扫、引擎接线（本地 PP-OCR / VL、云端 PaddleOCR、离线翻译）、`offline_models:changed` 广播、诊断与通知；`launcher.rs` 用 `wisp_platform::child` 实现 `wisp-local` 的 `Launcher` |
| `sync_svc/` | 多设备同步的桌面端：`LocalStore` 适配器（配置、Key、剪贴板、识别历史）、改动日志的写入线程、触发与去抖、退避、同步凭据的读取、`sync:status` 广播与托盘状态；同步关闭时不打开 `sync.db`、不建 HTTP 客户端与线程 |
| `ipc/` | 全部 Tauri 命令。只做参数校验与分发，不写业务逻辑 |
| `tray/` | 托盘菜单、状态行与角标；托盘图标按平台、任务栏明暗与状态选图，Windows 按通知区域的实际尺寸（随显示缩放）选用恰好等大的一张；Linux 上检测托盘宿主，没有时退化为设置窗口加系统通知 |
| `updater/` | 检查更新（启动 30 s 后一次，之后惰性检查，间隔至少 24 h）、下载、验签、安装；三种策略：只提示、自动安装、关闭 |
| `assets.rs` | `wisp://` 协议的资源注册表：截图帧、裁剪图、剪贴板图片与缩略图、应用图标 |

**剪贴板的粘贴管线**：所有粘贴都经 `clipboard_svc/paste.rs` 的 `PasteRequest`，由「来源 × 宿主」两部分组成。来源是整条条目、变换后的条目（「粘贴为…」）、一段文字（芯片的片段）或按顺序合并的多条；宿主由调用窗口与面板当前的视图推出，是面板、悬浮剪贴板或连续粘贴；条目只能在剪贴板视图里粘贴，片段（芯片）也可以在显示 OCR 结果的面板里粘贴。写入剪贴板一律抑制自身变更，片段、变换与合并的结果不成为新的历史条目。宿主只决定交还焦点的方式：面板先隐藏再还原焦点、发粘贴键；悬浮剪贴板把前台交还给目标应用（`native::float_yield`）后再发粘贴键；连续粘贴只写入剪贴板，按键由钩子重放用户自己按下的那一次。新的粘贴入口只需要构造一个 `PasteRequest`。

**连续粘贴**（`clipboard_svc/queue.rs`）：开始时立即粘贴第一条，之后才经 `HookSpec.paste_keys` 装上拦截粘贴键的钩子，并接管全局 Esc。用户在任何应用里按粘贴键时，钩子吞掉这次按键，工作线程写入下一条后再把按键重放给前台应用。钩子**只在队列进行中存在**：用完、Esc、10 分钟无操作、锁屏或用户取消时立即卸载，平时不拦截任何按键。

**条目检视**：`clipboard_inspect`（按 id）与 `text_inspect`（OCR 结果等纯文本）一次返回片段与适用的变换，芯片条、「提取」与「粘贴为…」共用；Engine 以内容 hash 为键缓存最近 8 条，识别设置变化时整体作废。`text_transform` 对一段纯文本执行其中一种变换并把结果交回前端（OCR 结果的排版等），不适用时原样返回；浏览器 mock 不实现变换，只查由同一套函数生成的样例表 `fixtures/consts/TransformCases.json`。

## 前端

```text
apps/desktop/src/
├── entries/        五个页面的入口：panel.tsx、settings.tsx（也承载引导）、pinned.tsx、clipfloat.tsx、overlay.ts
├── panel/          面板外壳：视图路由、进入 / 退出动效、高度上报、拖动
├── views/          translate · ocr · clipboard · input · toast 五个视图（面板与固定窗口共用；剪贴板视图也用于悬浮剪贴板）
├── settings/       设置窗口的各个分组（`offline/` 是离线模型页的模型卡片）
├── onboarding/     首次启动引导
├── pinned/         固定窗口
├── floating/       悬浮剪贴板的窗口外壳（首帧交接、拖动、关闭）
├── overlay/        截图框选（原生 TypeScript + DOM，不加载 React / Motion）
├── shared/
│   ├── ipc/        唯一允许引用 @tauri-apps/* 的目录：命令封装、事件、类型镜像、黄金样例、浏览器 mock
│   ├── motion/     动效令牌与预设（时长、曲线、弹簧参数）
│   ├── styles/     设计令牌（颜色、圆角、阴影等 CSS 变量）
│   ├── ui/         通用组件
│   ├── offline/    离线模型的前端目录（显示顺序、组合、依赖的推理组件）与快照的订阅（`offline_models:changed`）
│   ├── stream/     流式文本的追加写入
│   ├── markdown/   增量渲染的 Markdown 子集
│   ├── i18n/ · keys/ · hooks/ · perf/
├── locales/        zh-CN 与 en-US 的界面文案，按命名空间分文件
└── generated/      脚本生成的文件（第三方许可清单），不要手改
```

- **页面入口**：Vite（Rolldown）构建五个 HTML 入口，共享的 chunk 自动拆分。面板的五个视图全部静态导入，首次唤出任何视图都不需要再加载代码；设置与引导互相懒加载；固定窗口的入口不含剪贴板视图，悬浮剪贴板（`clipfloat`）有自己的入口并静态带上剪贴板视图；覆盖层入口不含 React / Motion 运行时。OCR 结构化结果的表格与公式预览（`views/ocr/structured/`）是唯一的非首屏 chunk：结构化模式开始识别时预取，加载完成前先显示源码。`scripts/check-size.mjs` 检查这些依赖关系与体积预算。
- **剪贴板的操作注册表**：剪贴板的每个操作（粘贴、收藏、提取、粘贴为、连续粘贴、悬浮……）在 `views/clipboard/actions/` 里声明一次：标签、图标、快捷键、作用于一条还是多条、出现在哪些界面、何时可用、怎么执行。右键菜单、⌘K、「⋯」菜单、头部按钮、提示条与键盘绑定都由它生成，测试保证 id 唯一、文案齐全、显示的快捷键与绑定一致、同一层没有冲突。面板与悬浮剪贴板的差异（高度、焦点、Esc、提示方式）集中在 `views/clipboard/host.ts`；片段与变换的图标、文案与顺序在 `shared/smart/specs.ts`（剪贴板与 OCR 共用，文案在 `smart` 命名空间；识别与变换本身都在 Rust）。
- **状态**：Zustand 保存视图状态；配置的快照来自 Rust（`settings_get` 与 `settings:changed`），前端不持有第二份权威数据。
- **流式渲染**：Rust 已按 16 ms 合并增量，前端在 `requestAnimationFrame` 里批量追加到文本节点，避免每个 token 触发一次 React 渲染。
- **动效**：时长和曲线只能引用 `shared/motion/` 的令牌或 `--dur-*` / `--ease-*` CSS 变量；颜色只能用设计令牌。「减少动效」打开后只保留简短的淡入淡出。
- **文案**：用户可见的字符串一律经 `t()` 取自 `locales/`；托盘、系统通知等由 Rust 显示的文案在 `src-tauri/src/app/i18n.rs`。
- **浏览器 mock**：`pnpm -C apps/desktop dev:mock` 在普通浏览器里运行前端，`shared/ipc/mock/` 模拟全部命令与事件，`scenarios/` 下按视图提供场景（URL 参数 `?label=panel&scenario=…`）。界面开发、截图与浏览器级测试都基于它。

## IPC

前端与 Rust 之间有四种通道：

| 通道 | 方向 | 用途 |
| --- | --- | --- |
| 命令（`invoke`） | 前端 → Rust | 所有操作，返回 `Result<T, IpcError>` |
| 事件 | Rust → 前端 | 低频广播：`panel:prepare`、`clipboard:changed`、`clipboard:queue`、`settings:changed`、`glossary:changed`、`appearance:changed`、`permissions:changed`、`hotkeys:changed`、`update:status`、`sync:status`、`offline_models:changed` 等 |
| Channel 流 | Rust → 前端 | 翻译与 OCR 的流式输出（`StreamEvent`） |
| `wisp://` 协议 | 前端读取 | 截图帧、裁剪图、剪贴板图片与缩略图、应用图标等二进制资源 |

约定：

- 命令名、事件名、字段名、枚举值统一用 snake_case；事件名形如 `域:动作`。前端的封装函数用 camelCase（`api.translateStart(...)`），但参数对象的键保持 snake_case。
- 命令按领域分在 `ipc/cmd_*.rs`：窗口与面板、流式请求与截图覆盖层、剪贴板、设置与风格、模型服务与 Key、离线模型（`cmd_models.rs`：`offline_models_get`、下载 / 暂停 / 取消 / 删除、删除推理组件、释放内存、打开文件夹、重新检测显卡）、快捷键、应用与权限、更新、同步。离线模型的选用走 `settings_patch`（`offline.*`）；下拉里的「下载并使用」给 `offline_model_download` 传 `select: true`，安装成功后才写入 `offline.*`，下载期间用户改过同类别的选择则不覆盖。命令清单只在 `ipc/mod.rs` 的 `commands!` 宏里写一处。
- **API Key 只进不出**：前端可以设置、删除 Key，没有读取 Key 的命令；所有请求由 Rust 发出。PaddleOCR 的访问令牌就是这个服务的 Key，走同一套 `secret_set` / `secret_delete`。WebDAV 应用密码与同步密码同样只进不出：只作为 `sync_probe` / `sync_connect` / `sync_reset_remote` 的参数传入，没有读回的命令。
- 字符串、浮点数与集合参数在命令函数体的第一步校验（长度、条数、有限性），校验失败返回 `invalid_argument`。
- 流式事件的序列是 `Start` →（`Meta` | `Phase` | `Notice` | `Delta`）* → 恰好一个 `Done` 或 `Error`（`Phase` 也用来报告离线模型加载中 `loading_model` 与云端排队 `queued`；`Notice{partial}` 表示 PaddleOCR-VL 的文档里有区域没能识别、已跳过）；取消时以 `Error{code: cancelled}` 结束。`Delta` 是经过后处理与合并之后的增量，前端只追加。`stream_attach` 先回放已有事件再推送实时事件，固定窗口接管面板里进行中的流就是靠它。

**类型同步**：Rust 类型在 `crates/wisp-types`，TypeScript 镜像在 `apps/desktop/src/shared/ipc/types.ts`。两边由黄金样例绑定——`cargo test -p wisp-types` 把每个类型的标准样例序列化后与 `shared/ipc/fixtures/*.json` 逐字比对，前端测试读取同一组文件。改动类型时两边一起改，再用 `UPDATE_GOLDEN=1 cargo test -p wisp-types` 重新生成样例。命令集合与每条命令的参数键由 `ipc::contract_tests` 与 `scripts/check-contract.mjs` 比对前后端。

## 一次划词翻译的路径

```text
快捷键回调（hotkeys）
  → bus：Command::Action { action: TranslateSelection, … }
  → orchestrator：platform.selection.get()
        先读系统提供的选区（macOS AX、Windows UI Automation、X11 PRIMARY 选区），
        读不到时模拟一次复制并把剪贴板原样还原
  → 未就绪检查（没有模型、没有权限……）→ 语言检测与方向 → 是否词典式 → 是否超长
  → 计算锚点与面板尺寸，窗口在隐藏状态下就位，emit panel:prepare
  → 与此同时发出 LLM 请求，不等窗口显示
前端收到 panel:prepare：同步渲染骨架并测量 → invoke panel_ready
  → Rust 在命令内显示窗口，返回后前端播放进入动效
wisp-llm 的流 → 后处理 → 16 ms 合并 → Channel → 前端追加到文本节点
```

设计目标是从按下快捷键到面板出现不超过 100 ms：LLM 请求与窗口显示并行，面板页面预先加载并复用。

## 窗口体系

所有 WebView 窗口都由 `WindowManager` 按 `tauri.conf.json` 里 `create: false` 的模板创建。

| label | 页面 | 用途与行为 |
| --- | --- | --- |
| `panel` | `panel.html` | 全部结果共用的浮动面板。启动后预先加载、隐藏复用；显示时不激活应用（翻译 / OCR 视图不抢原应用的焦点，剪贴板与输入视图需要打字，会成为 key 窗口）；不出现在任务栏与 Alt-Tab。macOS 上是非激活的 NSPanel |
| `pinned-<id>` | `pinned.html` | 把面板结果「固定」成的独立窗口，可拖动、可调整大小、默认置顶；关闭即销毁 |
| `clipfloat` | `clipfloat.html` | 悬浮剪贴板（单例）：可拖动、可调整大小、默认置顶（可切换），记住位置、大小与置顶状态（`state.json`）；点击条目粘贴进当前应用而不抢焦点；截图时隐藏；关闭即销毁 |
| `settings` | `settings.html` | 设置窗口，按需创建，记住位置与尺寸；macOS 上存在期间应用切换为普通应用（出现在 Dock 与 ⌘Tab） |
| `onboarding` | `settings.html` | 首次启动引导，与设置共用页面入口，按窗口 label 区分 |
| `overlay-<n>` | `overlay.html` | 截图框选的全屏覆盖层，每个显示器一个；确认或取消后销毁（Windows 上主显示器的覆盖层预先建好、隐藏复用） |
| 浮标 | 原生窗口 | 扩展触发的浮标，不用 WebView，原生绘制 |

**面板显示时序**：Rust 先在隐藏状态下确定位置与宽度，emit `panel:prepare`；前端以透明状态同步渲染并测量内容高度，调用 `panel_ready`；Rust 在这个命令里显示窗口，命令返回后前端播放进入动效。这样不会闪出上一次的内容，也不会出现空白窗口。

**两种帧模式**（面板高度随流式内容增长时的做法）：

- **Native（macOS）**：窗口就是卡片本身。前端只上报目标高度，Rust 用与前端相同参数的弹簧逐帧调整窗口，帧时钟只在动画期间存在。材质用原生的 `NSVisualEffectView`。
- **Css（Windows / Linux）**：窗口是透明的，卡片在窗口内用 CSS 画出阴影与圆角；窗口按阶梯放大，卡片在窗口内做动画。卡片之外的透明区域不拦截点击：Windows 在面板可见期间用低级鼠标钩子按光标位置切换窗口的 `WS_EX_TRANSPARENT`；X11 在面板可见期间以 30 Hz 读取光标位置，光标落在卡片外时整窗忽略鼠标。面板隐藏后两者都停止。Wayland 或关闭了合成时窗口不透明，卡片铺满窗口。

**关闭面板**：翻译 / OCR 视图不是 key 窗口，由全局 Esc 与外部点击监听关闭（在原应用里拖选文字不算点击）；剪贴板与输入视图失去焦点时关闭。

**悬浮剪贴板**：从面板交接（⌘P 或头部按钮）时，悬浮窗先订阅事件、取初始状态（继承面板的查询、筛选与光标），首帧渲染完才请求显示，Rust 在同一次主线程回调里显示悬浮窗、隐藏面板，没有空帧。它要做到「点一下就粘贴进别的应用」，窗口本身不能拿走前台，平台差异集中在 `native::float_*`（`float_attach`、`float_set_on_top`、`float_set_focusable`、`float_yield`，三端同名同签名）：

- **macOS**：非激活、可以成为 key 的 NSPanel（浮动层级，可出现在所有桌面与全屏应用旁）；粘贴前激活目标应用，等悬浮窗不再是 key 后再发粘贴键，超时则只复制并在窗口里提示。
- **Windows**：常驻 `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW`，点击不夺前台；搜索框需要输入时临时可激活，粘贴前把前台交还给目标窗口。
- **X11**：平时不可聚焦，搜索时打开，粘贴前用 `_NET_ACTIVE_WINDOW` 交还；Wayland 只复制并提示。

## 平台层

`wisp-platform` 以 trait 描述操作系统能力，上层只通过 `Platform` 门面调用：

| trait | 能力 |
| --- | --- |
| `SelectedText` | 读取当前选中的文字（含选区矩形） |
| `Paste` | 写剪贴板、向目标应用模拟粘贴键 |
| `FocusTracker` | 前台应用、激活、是否以管理员身份运行 |
| `Permissions` | 辅助功能、屏幕录制、输入监控的状态与授权入口 |
| `AppInfo` | 应用图标、显示名、运行中的应用 |
| `CursorScreen` | 显示器、光标位置、系统强调色、减少动效 / 透明度、深色模式及其变化；Windows 任务栏明暗与通知区域图标尺寸 |
| `ClipboardAccess` | 读取、全类型快照与恢复、变更监听、抑制自身写入 |
| `OutsideClick` | 外部点击监听（关闭面板用） |
| `HotkeyEnv` / `KeyGrab` | 系统保留的快捷键、键盘布局；X11 上的事件驱动快捷键注册 |
| `InputHooks` / `BobberWindow` | 扩展触发的输入钩子与浮标窗口；连续粘贴期间拦截粘贴键的钩子（`HookSpec.paste_keys`）（`mouse-hook` feature） |

约束：可能阻塞的调用（无障碍接口、UI Automation、X11 selection、剪贴板）都有硬超时；钩子回调只做常数时间的工作，然后交给工作线程；各实现惰性初始化，`Platform::create` 不阻塞。

| 能力 | macOS | Windows | Linux（X11） |
| --- | --- | --- | --- |
| 取词 | AX，回退模拟 ⌘C | UI Automation，回退模拟 Ctrl+C | PRIMARY 选区，回退模拟复制（终端不发 Ctrl+C） |
| 剪贴板监听 | `changeCount` 轮询（用户活跃时 4 Hz，安静后与锁屏时降频） | `AddClipboardFormatListener`，事件驱动 | XFixes，事件驱动 |
| 模拟按键 | CGEvent | SendInput | XTest |
| 截图 | ScreenCaptureKit | xcap（GDI；可选 Windows.Graphics.Capture） | MIT-SHM / GetImage |
| 隐私标记 | `org.nspasteboard.ConcealedType` 等 | `ExcludeClipboardContentFromMonitorProcessing` 等 | `x-kde-passwordManagerHint` |

模拟复制的回退在读取之前对剪贴板做全类型快照，读完立即恢复，期间产生的变更不会进入剪贴板历史。

连续粘贴的按键拦截只在队列进行中安装：macOS 用一个 CGEventTap、Windows 用低级键盘钩子，吞掉没有 Wisp 标记的粘贴键（包括按键的自动重复与抬起），交给工作线程写入下一条后再带标记重放；X11 把粘贴键加进被动抓取，抬起时触发，重放前先注销抓取再用 XTest 发送；Wayland 不提供（界面上置灰并说明）。锁屏（macOS 的锁屏通知、Windows 的桌面切换）作为信号上报，队列随之结束。

Wayland 会话下取词、全局快捷键、截图与剪贴板监听都不可用，能力的可用性经 `Platform::info()` 报告给上层与界面。`wayland` feature 提供基于 wl-clipboard 与 XDG 门户的降级实现，发布版没有编入。

离线模型用到的系统能力是 `wisp-platform` 里的三个模块（不是 `Platform` 门面上的 trait），只在离线模型用到时调用：

| 模块 | macOS | Windows | Linux |
| --- | --- | --- | --- |
| `system`：内存总量与可用量、性能核数、磁盘可用空间 | `hw.memsize` 与 `host_statistics64`；`hw.perflevel0.physicalcpu`；`statfs` | `GlobalMemoryStatusEx`；`GetLogicalProcessorInformationEx`（只数最高性能等级的核）；`GetDiskFreeSpaceExW` | `/proc/meminfo` 与 cgroup v2 上限取较小值；sysfs 的核拓扑去重；`statvfs` |
| `pressure`：内存压力事件，`watch()` 返回的句柄丢弃即卸载 | `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE` | 低内存 / 内存充足两个通知对象交替登记一次性等待 | PSI 触发器（`/proc/pressure/memory`）；内核不支持时不安装 |
| `child`：受托管的子进程、pid 文件与孤儿清扫 | `--wisp-guard` 守护进程 + 进程组 | Job Object（`KILL_ON_JOB_CLOSE`）、`CREATE_NO_WINDOW`、低优先级 | 同 macOS |

窗口层自己的平台代码在 `apps/desktop/src-tauri/src/windows/native/{macos,windows,linux}.rs`：NSPanel（借助 `tauri-nspanel`）、原生材质与弹簧动画、WebView2 的可见性与内存目标、X11 上透明区域的点击穿透等。

## 多设备同步

`crates/wisp-sync` 实现同步本身（不依赖 Tauri，可以单独测试），桌面端的 `sync_svc/` 负责接线。用户可见的行为见[使用说明 · 多设备同步](user-guide.md#多设备同步)。

**按最弱的 WebDAV 服务器设计**。只用 `PROPFIND`（`Depth: 1`）、`GET`、`PUT`、`DELETE`、`MKCOL`；不少服务器忽略 `If-Match` 等条件请求，所以**每台设备只写自己的文件**，从不改写别人的文件。远端是用户地址下固定的 `wisp/` 文件夹：

```text
wisp/
  vault.json                          明文：格式版本、随机 vault id、密钥派生参数、同步密码的校验值
  <vault>.<device>.head               每台设备一个（密文）：设备信息与本设备的日志段清单
  <vault>/log/<device>/<w>.seg        日志段（密文，不可变；写入编号不复用）
  <vault>/blobs/<device>/<name>.blob  图片等大对象（密文，不可变；只有上传者写和删）
```

**记录与合并**。每个可同步的值是一个寄存器 `(域, key) → 值`，带混合逻辑时间戳（HLC：`物理毫秒 << 16 | 计数`，在捕获改动的那一刻分配）。合并是逐 key 的 last-writer-wins，按 `(时间戳, 设备 id)` 比较，满足交换律、结合律与幂等——任意顺序、重复接收都收敛，所以没有冲突对话框。删除写墓碑；剪贴板与识别历史的清空写带时间戳的 `@clear` 记录。本版本不认识的域、key 或值照常进入副本、标为「未应用」，升级后再应用，因此不同版本的 Wisp 可以混用。

**域与投影**（`projection`）：

| 域 | 内容 |
| --- | --- |
| `cfg` | `settings.json`：每个分组的每个字段是一个 key（如 `/translate/primary_lang`），更深的值整体作为一个寄存器；模型服务、排除的应用、应用内粘贴键按 id 拆成元素；只属于本机的字段（开机启动、开发者模式、`advanced`、`sync`、`offline`、剪贴板记录开关、疑似密钥检测、识别历史开关）不投影；按系统区分的字段（快捷键、双击修饰键、复制等待、面板材质）的 key 带 `@<系统>` 前缀，只在同系统的设备之间生效；模型条目里本机学到的能力缓存不同步 |
| `sty` | `styles.json`：顺序、每个内置风格的覆盖项、每个自定义风格；风格快捷键按系统区分 |
| `glo` | 术语表：每条术语按原文 |
| `key` | 模型服务的 Key，带设置它时服务的 `scheme://host:port`；与当前服务的地址不一致时不写入 |
| `clip` | 剪贴板条目（按内容 hash；验证码带到期时间，收到时已过期的不导入；到期只删除本机的副本、不写墓碑，避免盖掉其他设备上的收藏）、收藏状态与名称、清空；图片原图作为 blob。「同步验证码」关闭时未收藏的验证码不发布也不导入 |
| `ocr` | 识别记录、清空；缩略图与原图作为 blob |

反投影从本机当前文档出发，只改动要应用的 key；本机文件里不认识的字段原样保留。`unproject(project(x), x) == x` 由测试保证。

**一轮同步**（同一时刻只跑一轮，运行中再有触发就在结束后补一轮）：

1. `PROPFIND wisp/` 复核 vault，只 GET ETag 变了的 head，读新的日志段，合并进本机副本；
2. 按 配置 → 风格 / 术语 → Key → 剪贴板 → 识别历史 的顺序应用胜出记录，经桌面端确认写入后才标记已应用；
3. 分批推送：配置与 Key → 剪贴板文本 → 图片与识别记录，每批一个新段加一次 head 更新，中断后已完成的批次对其他设备可见；推送先把「进行中的写入」落盘，失败重试时换新的写入编号，所以是幂等的；
4. 视情况压缩：每台设备只重写自己的日志，丢掉被取代、被删除或清空、超出保留设置的记录，服务器上的历史因此有界；不再被引用的 blob 由上传者删除。

没有变化时一轮只有 1 个请求。加入、创建与恢复由 `sync_connect` 按服务器与本机状态决定，不带模式参数：服务器没有数据时创建；已有数据、本机没有副本时加入（配置以服务器为准，本机独有的元素与剪贴板、识别历史按各自的时间发布，**加入永不删除本机数据**：早于已有删除或清空的本机条目留在本机、不发布，因此也不会在其他设备上复活；这台设备以前就在同步这份数据时例外，重新连接后照常应用它离开期间的删除与清空）；本机副本属于服务器上的这份数据时只更新凭据并解锁。另有克隆检测：复制了数据目录的另一台机器会换用新的设备 id。

**加密**（`crypto`，全部基于 `ring`）：

- 主密钥 = PBKDF2-HMAC-SHA256（NFC 规范化的同步密码、随机盐、60 万次迭代），只在连接时计算一次（在阻塞线程里）；HKDF-SHA256 以 vault id 为盐派生数据密钥与命名密钥。
- head 与日志段：`WSY1` 文件头 + 随机 nonce + ChaCha20-Poly1305 密文；AAD 包含文件头、vault id 与文件的逻辑名，调换文件或拿别的 vault 的文件都会解密失败。
- blob：64 KiB 分块的流式封装，每块的 nonce 含块序号与「最后一块」标记，截断可以发现；加密、解密与上传、下载都按流处理（经数据目录 `sync/` 下的临时文件），不整块读入内存。blob 的文件名是内容身份的 HMAC，不泄露内容。
- 桌面端在钥匙串里保存派生出的主密钥与 WebDAV 应用密码（一项，见[数据与配置](#数据与配置)），不保存同步密码本身。

**桌面端接线**：`SettingsService` 的改动钩子、Key 钩子与剪贴板 / 识别历史的 sink 在锁内只做常数时间的工作（分配时间戳、送进日志写入线程的通道），写入线程在短事务里把改动记进 `sync.db`。远端改动经单独的入口应用（`apply_synced`、`secret_apply_synced`、剪贴板的 `apply_remote_*`），这些入口不触发钩子，所以不会回声；设置文件正被热加载或无法解析时推迟应用。设置页的模型服务与术语表逐条写入（`provider_save`、`glossary_upsert` 等），不整组写回旧值。

## 离线模型与推理进程

`crates/wisp-local` 实现离线模型本身（不依赖 Tauri，可以单独测试），桌面端的 `models_svc/` 决定何时加载、何时释放。用户可见的行为见[使用说明 · 离线模型](user-guide.md#离线模型)。

**目录**（`catalog`）：五个模型与两个推理运行时编译进二进制，不拉远程清单。每个模型文件记录大小、SHA-256，以及 ModelScope 与 Hugging Face 上钉到具体 commit 的下载地址（两边内容一致）；运行时只从官方 GitHub Release 获取，记录发布包的 SHA-256 与要解出的文件。换文件或换版本时增加 `CATALOG_REVISION`：已安装的模型提示有新版本，不自动重新下载。目标平台在运行时按 `std::env::consts` 判断，这个 crate 不写 `#[cfg(target_os)]`。

| 模型 | 运行时 | 文件 |
| --- | --- | --- |
| PP-OCRv6 Tiny / Small | ONNX Runtime 1.28.3（官方 CPU 构建的动态库） | 检测与识别的 ONNX 模型，识别模型的 `inference.yml`（内嵌字典） |
| PaddleOCR-VL-1.6 | llama.cpp b11669 + ONNX Runtime | 官方 BF16 的 GGUF 与 mmproj；整页文档用的版面检测模型 PP-DocLayoutV3（ONNX） |
| Index-Translate 2B / 9B | llama.cpp b11669（官方预编译的 `llama-server`） | 官方 Q4_K_M 的 GGUF |

Windows / Linux 使用 llama.cpp 的 Vulkan 构建（包含 CPU 后端，没有可用的 Vulkan 驱动时退回 CPU），macOS 使用 Metal 构建。Windows 的 ONNX Runtime 发布包大部分是调试符号，下载器按 HTTP Range 只取 zip 里的 `onnxruntime.dll` 一个条目（压缩后约 6 MB）并校验它解压后的哈希，服务器不支持区间时退回整包。

**进程结构**：

```text
Wisp 主进程（models_svc）
  ├─ ort 槽位         wisp-desktop --wisp-worker ort         PP-OCRv6、版面检测（ONNX Runtime 只加载在这个进程里）
  ├─ vl 槽位          [守护进程] ─ llama-server             PaddleOCR-VL-1.6
  └─ translate 槽位   [守护进程] ─ llama-server             Index-Translate 2B 或 9B

守护进程：macOS / Linux 上是 wisp-desktop --wisp-guard -- llama-server …；Windows 上直接启动并加入 Job Object
```

- **ORT 工作进程**：ONNX Runtime 的动态库加载后无法卸载，丢弃会话后仍有残留内存，所以放进子进程，闲置时结束进程即彻底释放；崩溃也不会连累主进程。工作进程就是 Wisp 自己的二进制（不另带可执行文件），经 stdin / stdout 上带两个长度前缀的帧通信（JSON 头 + 像素负载），请求串行，主进程侧用专用线程读写管道，不占 tokio 线程。环境变量给出动态库的绝对路径、线程数与模型目录，并关闭 ONNX Runtime 的遥测；每个请求有 60 秒时限，超时即结束进程。会话按需创建、进程内复用；PP-OCRv6 的检测与识别用 oar-ocr，参数全部显式设置为官方产线的值；版面检测直接调用 ort。stdin 关闭（Wisp 退出或崩溃）时工作进程自行退出。
- **llama-server**：OpenAI 兼容接口，只绑 `127.0.0.1`，端口由 Wisp 先占一个临时端口取号后传入。每次启动生成 32 字节的随机 API Key，经环境变量 `LLAMA_API_KEY` 传入（不进命令行），离线翻译与 VL 识别用它访问。工作目录设为 llama-server 所在目录（不会从别处加载被放进去的后端模块），环境只保留必要的变量；参数包括 `--offline`、单并发、8192 上下文、不缓存提示、关闭 WebUI，翻译槽位关闭思考，关闭 GPU 加速时 `-ngl 0`；`--sleep-idle-seconds` 设为闲置时长 + 60 秒，万一进程残留也会卸载权重。就绪靠轮询 `/health`（不经系统代理），加载超时 120 秒。stdout / stderr 只匹配几种固定格式（监听端口、致命错误、内存不足、后端初始化失败、缺少动态库）用来归类错误，只在内存里保留最后 4 KB，从不写进日志。
- **守护进程**（macOS / Linux）：Wisp 二进制的 `--wisp-guard` 模式。它自成进程组，按需把优先级降到 nice 5，启动目标后先报告目标的 pid，再原样转发目标的输出；目标的环境只含调用方显式列出的变量（值经守护进程自己的环境传递，不进命令行）。守护进程读自己的 stdin，读到 EOF（Wisp 退出或崩溃）就向目标发 SIGTERM，最多等 1 秒再 SIGKILL，然后退出；强制结束时向整个进程组发 SIGKILL。
- **Windows**：子进程以挂起状态创建，加入带 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的 Job Object 后再恢复，并带 `CREATE_NO_WINDOW` 与 `BELOW_NORMAL_PRIORITY_CLASS`；Wisp 持有的 Job 句柄关闭（包括崩溃）时，由内核结束整棵进程树。
- **pid 文件**：每个推理进程写 `models/run/<槽位>.pid`（pid、启动时间与可执行文件路径）。启动链里（面板预热之后，且只有 `models/` 存在、有 pid 文件时）与第一次启动槽位之前各清扫一次：三者都匹配才结束，避免误杀复用了 pid 的其他进程。
- **槽位与生命周期**（`models_svc`）：三个槽位互不阻塞，`stopped → starting → ready`；加载单飞，并发的请求等同一次启动；同一模型因同一原因 30 秒内连续失败时不再重启、直接报错，避免抖动。每次请求开始与结束都重排这个槽位的一次性闲置定时器（`offline.idle_release_min`，默认 5 分钟），没有槽位加载时不存在任何定时器。至少一个槽位加载时才安装内存压力监听：警告时停止空闲的槽位，严重时忙的槽位也在当前请求完成后停止；全部停止后卸载监听。加载前做内存预算：Windows / Linux 按可用内存比较（留 512 MB 余量）；macOS 的「可用内存」不含可回收的文件缓存与可压缩的内存，所以改为与内存总量比较（需要量超过总量的 70%，或系统内存压力已是严重时才拒绝），其他空闲槽位只在加起来放不下时才先停；仍不够返回「可用内存不足」，不硬启动。同一槽位换进程时，新进程等旧进程完全退出后才启动（停止在取下旧进程的同一把锁里登记），内存预算也在旧进程退出之后再算，两个模型不会短暂共存。换模型、关闭 GPU 加速、「立即释放内存」时，正在启动或正在处理的槽位先完成手上的请求再停（只有删除与退出会立即中断）；两个 PP-OCR 档位之间切换不停 ORT 工作进程（一个进程服务两档）。用显卡推理的模型安装完成后会预热一次：槽位空着、内存够、不需要停别的槽位时后台启动一次，就绪后按普通闲置处理，把 Metal / Vulkan 着色器的首次编译提前做完；只有集成显卡时不预热。退出（以及 Windows 安装更新之前）关闭所有槽位的 stdin，最多等 1 秒再强制结束，并删除 pid 文件。
- **取消**：编排器的取消令牌一直传到槽位：llama-server 在连接断开时停止生成；PP-OCR 的一次识别很短，取消后丢弃结果；VL 文档流水线在区域之间检查取消。
- **线程与优先级**：llama-server 的线程数取性能核数的一半（2–8），ORT 工作进程取一半（1–4）；推理进程的优先级略低于正常，不和前台应用抢 CPU。
- **设备适配**（`fitness`）：按内存总量、是否 Apple 芯片、显卡显存与「使用 GPU 加速」，给每个模型「适合 / 较慢 / 内存不足 / 待检测」。Windows / Linux 的显卡在 llama.cpp 运行时安装之后用 `llama-server --list-devices` 探测一次，结果缓存在 `models/runtimes/llama.cpp/devices.json`（带运行时版本），用户可以重新检测；macOS 直接按 Metal 与统一内存判断。集成显卡（按设备名，或显存占到系统内存一半左右）报告的「显存」是共享的系统内存，适配按 CPU 档评估（推理仍交给 llama.cpp 的自动卸载）。下载一个适配为「待检测」的模型时分两段：先装推理组件、探测显卡，再重新评估；变成「较慢」或「内存不足」时任务停在已暂停，等用户在界面上确认后才下载模型本身。

**下载**（`download`）：独立的 reqwest 客户端，用户开始下载时创建、任务结束即丢弃；只允许 https，不带鉴权头与 `Referer`，读系统代理，最多跟随 5 次重定向。续传用 `.part` 文件加 `Range` / `If-Range`，服务器不支持区间时从头下载，CDN 签名过期时重新解析地址再续。`auto` 源同时探测 ModelScope 与 Hugging Face，按先成功响应的选；出错或持续 20 秒低于 50 KB/s 时换源，在同一个部分文件上续传。写盘与 SHA-256 在专用线程里进行（有界通道背压，GB 级文件不占 tokio 线程）；单个文件校验通过后才改名，全部完成后整体移进模型目录、最后写 `installed.json`；哈希不符时丢弃并换源重试一次。同一时刻只下载一个文件；开始前检查磁盘空间。运行时解包只取需要的文件（llama-server 与它加载的动态库、ggml 后端模块），拒绝绝对路径与 `..`，解到临时目录、写好记录后整体改名。

**识别**（`ocr/`）：

- PP-OCRv6 的结果是带四点框与置信度的文本行，由 `assemble` 拼成「纯文本」（按视觉行，保留相对缩进）或「段落」（按行距、左边界与行宽合并，还原英文的断词），置信度低于 0.5 的行丢弃；PaddleOCR 云端的 PP-OCRv6 结果用同一套拼装。PP-OCRv6 不做结构化，编排器把结构化模式退回段落。
- PaddleOCR-VL 的文档流水线（`vl`）照官方 PaddleX 产线移植：版面检测（PP-DocLayoutV3，在 ORT 工作进程里）→ 阅读顺序 → 逐区域按类别发提示词（文字、表格、公式；图表与印章默认不识别，页眉页脚等忽略）→ 拼成 Markdown（表格由 OTSL 转成 Markdown 表格）。每完成一个区域发一段增量；单个区域失败时跳过（全部失败才报错），所有区域都返回空结果时报告显卡加速出错。较小的截图或只有一个文字区域时整图识别一次。
- PaddleOCR 云端客户端（`cloud`）见[模型服务 · PaddleOCR](providers.md#paddleocr文字识别服务)：按地址判别任务接口与部署的服务，客户端在第一次识别时创建、用完丢弃。

**离线翻译**：编排器把 `@offline` 合成服务商（OpenAI Chat 协议，地址与随机 Key 在槽位就绪后填入，首字时限 120 秒）交给 wisp-llm，提示词用 Index-Translate 的官方格式（单条 user 消息，无 system，温度 0）。润色、校对返回 `OfflineTaskUnsupported`，「提升译文」与词典式退化为普通翻译，绝不改用云端。

**默认零成本**：`ModelsService::new` 只建结构，不读盘、不起线程、不建 HTTP 客户端；第一次打开设置页的离线模型一页或第一次用到离线引擎时才读一次库存（`installed.json` 与文件大小，不重算哈希），启动链里只在 `models/` 已经存在时才读（让面板的模型菜单知道装了什么）。安装包里只有 Rust 代码：ONNX Runtime 以 `load-dynamic` 方式在工作进程里加载，llama.cpp 是独立的可执行文件，都按需下载。

**oar-ocr 的补丁**：oar-ocr / oar-ocr-core 0.10.0 以 `vendor/` 副本经 `[patch.crates-io]` 引入，把 `image` 与 `imageproc` 的默认特性和带 C 代码的 `tokenizers` 收进可选特性，避免特性合并把全部图片解码器链进安装包；详情见 [vendor/README.md](../vendor/README.md)。

**macOS 的 entitlement**：发布构建开启 hardened runtime，默认只允许加载同一 Team ID 签名的动态库，ad-hoc 签名的应用因此拒绝一切非系统库。ORT 工作进程与主程序是同一个二进制，要加载下载来的 ONNX Runtime 动态库，所以主程序带 `com.apple.security.cs.disable-library-validation`（`apps/desktop/src-tauri/Entitlements.plist`，由 `tauri.conf.json` 的 `bundle.macOS.entitlements` 引用）。它只放开库校验：库本身仍须签名，未签名的仍被拒绝；下载的库按钉死的 SHA-256 校验后才会被加载。llama-server 是独立的进程，不受这一项影响。

## 数据与配置

| 内容 | macOS | Windows | Linux |
| --- | --- | --- | --- |
| 配置：`settings.json`、`styles.json`、`glossary.json`、`state.json` | `~/Library/Application Support/wisp/` | `%APPDATA%\wisp\` | `$XDG_CONFIG_HOME/wisp/` |
| 数据：`clipboard.db`（SQLite，WAL）与 `blobs/`；开启同步时还有 `sync.db`（SQLite，WAL）与 `sync/`（上传与下载用的临时文件） | `~/Library/Application Support/wisp/` | `%LOCALAPPDATA%\wisp\` | `$XDG_DATA_HOME/wisp/` |
| 日志：`wisp.log`，2 MB × 5 滚动 | `~/Library/Logs/wisp/` | `%LOCALAPPDATA%\wisp\logs\` | `$XDG_STATE_HOME/wisp/logs/` |
| 离线模型：数据目录下的 `models/`（运行时、模型、断点续传的部分文件、pid 文件） | `~/Library/Application Support/wisp/models/` | `%LOCALAPPDATA%\wisp\models\`（不漫游） | `$XDG_DATA_HOME/wisp/models/` |
| API Key（含 PaddleOCR 的访问令牌） | 钥匙串，服务 `wisp`、账户为服务 ID | 凭据管理器，`<服务 ID>.wisp` | Secret Service，服务 `wisp`、账户为服务 ID |
| 同步凭据（一项 JSON：地址、账号、应用密码、vault id、主密钥） | 钥匙串，服务 `wisp-sync`、账户 `account` | 凭据管理器，`account.wisp-sync` | Secret Service，服务 `wisp-sync`、账户 `account` |

- 目录名固定为 `wisp`，不使用带应用标识的目录。
- 所有 JSON 文件原子写入（先写临时文件再改名）。`settings.json` 允许手工编辑：文件变化后自动重新加载，只做宽松解析、不回写；解析失败时保留上一次有效的设置并在设置窗口里提示。
- `settings.json` 带 `schema_version`，启动时迁移；内置风格以代码为准，`styles.json` 只保存覆盖项与自定义风格；`state.json` 保存运行状态（引导是否完成、窗口位置、上次检查更新的时间等）。
- 模型能力缓存（是否支持图片、哪些参数被拒绝过）写在 `settings.json` 的模型条目上。
- `settings.json` 的 `offline` 分组（选用的离线 OCR / 翻译模型、闲置释放时长、GPU 加速、下载源）只属于本机：同步的投影（`LOCAL_GROUPS`）与桌面端应用远端改动时都排除它；`defaults` 与风格的模型引用不得指向保留的 `@offline`，所以同步不能把一台设备从离线改成云端。离线模型被选用时优先于风格绑定与默认模型。
- `models/` 的布局：`runtimes/<运行时>/<版本>/`、`<模型 id>/`（各带一份 `installed.json`：目录版本、文件名、大小与哈希、安装时间）、`.downloads/`（断点续传的 `.part` 与 `.meta`）、`run/`（pid 文件）；删除时先改名为 `.trash-*` 再在阻塞线程里删，失败的下次启动重试。安装状态只看 `installed.json` 与文件大小，不在加载时重算 GB 级文件的哈希。测试与真机验证可以用 `WISP_MODELS_DIR` 改到别处，只在 debug 构建或冒烟模式下生效。
- Linux 没有 Secret Service 时，Key 只保存在内存里（仅本次运行），绝不明文落盘。
- 剪贴板数据库带 schema 版本；从新版本降级时不会删除或改写新版本的数据库，只停用剪贴板历史并提示。
- 同步的远端数据在用户填写的 WebDAV 地址下的 `wisp/` 文件夹里（布局见[多设备同步](#多设备同步)）。
- `sync.db` 是同步的本机副本：每个 key 的胜出记录与应用状态、各远端设备的读取游标、本机日志账本、进行中的写入、本机上传过的 blob。剪贴板与识别历史的正文只在等待应用期间暂存，长期副本只在 `clipboard.db`；Key 不以明文保存。同步关闭时不打开，断开同步时删除。
- 同步凭据只有在 `settings.json` 里 `sync.server_url` / `sync.username` 与条目一致时才使用，地址或账号改过就按「缺少凭据」处理，旧密码不会发往另一台服务器。不写 `stored_keys` 索引；Linux 没有 Secret Service 时同样只在内存里。
- `state.json` 另存本机的同步设备 id（16 位随机十六进制，断开后重连沿用）。`settings.json` 的 `sync` 分组只属于本机，其中 `enabled`、`server_url`、`username` 只由同步命令写入，`settings_patch` 拒绝改动它们。

## 安全与隐私

- **Key 不进 WebView**：前端只知道服务 ID；请求、连接测试、模型列表都在 Rust 里完成。
- **最小权限**：`capabilities/default.json` 只给 WebView 开放事件监听与窗口拖动 / 关闭；插件的 JS API 都不使用，窗口能力一律走应用自己的命令。
- **严格 CSP**：没有远程资源，脚本只能来自应用自身；`freezePrototype` 开启。
- **日志不含正文**：只记录耗时、状态码、模型名、取词方式等元数据；原文、译文、剪贴板内容、截图、Key、请求头的值与 URL 查询参数都不写入日志。诊断包导出时再次脱敏。
- **截图只在内存里**：冻结帧随截图会话结束释放；只有用户打开「识别历史」时才写盘。
- **剪贴板隐私**：疑似密码、令牌、私钥的内容不记录；密码管理器默认排除；遵守应用的「不要记录」标记。被隐私规则跳过的复制不留下任何痕迹（不取图标、不写日志正文）。智能识别只在本机进行，识别出的片段、变换结果与收藏的名称都不写日志（最多记数量）；证件号与银行卡号的识别默认关闭，显示时遮住中间几位。
- **网络请求只有这几类**：用户配置的模型服务（包括 PaddleOCR 服务，截图上传到用户填写的地址，任务接口的结果从服务给出的链接下载）；检查更新（可关闭）；用户自己配置并开启的同步服务器（WebDAV，默认关闭）；用户点击触发的离线模型下载（模型来自 ModelScope / Hugging Face，推理运行时来自 GitHub 的官方发布页）。离线推理只访问 `127.0.0.1` 上的 llama-server 与本机管道，llama-server 以 `--offline` 运行，不联网。
- **离线模型**：下载的文件全部按钉死在二进制里的大小与 SHA-256 校验后才使用；下载只走 https，不带鉴权头。llama-server 只绑回环地址、带每次随机生成的 API Key，工作目录固定，环境变量只保留必要的几项；ONNX Runtime 的遥测关闭。离线引擎失败时绝不改用云端；离线翻译不支持的任务明确提示，不悄悄换服务。离线模型的选择不被同步改写。日志只记录模型 id、字节数、耗时、状态码与错误种类（下载记录主机与路径的第一段，不记签名参数），不记录推理进程的输出。
- **PaddleOCR**：访问令牌与 API Key 同等对待（只在 Rust 与系统凭据存储里）；结果链接的下载不带令牌；日志只记录主机、接口类别、状态码与耗时，不记录任务 id、结果链接与令牌；错误信息只用固定词汇，不回显服务返回的原文；公网地址必须 https。
- **诊断包里的离线模型信息**：已下载的模型 id、状态与大小，运行时版本，槽位状态，最近一次失败的种类，内存总量与可用量，是否 Apple 芯片，显卡名称与显存；不含路径、端口与 Key。
- **同步端到端加密**：服务器只见密文，以及文件的数量、大小与时间；明文的 `vault.json` 只有格式版本、随机 id、密钥派生参数与校验值。WebDAV 应用密码与同步密钥只在 Rust 与系统凭据存储里，同步密码本身不保存。
- **同步的网络边界**：`http://` 只允许回环、私有与链路本地地址、`.local` 与单标签主机名，其余必须是 https（Basic 认证不能明文过公网）；只有 GET 跟随重定向（最多 3 次，跨主机只允许 https → https），离开服务器自身的源之后不再带 `Authorization`，从不发送 `Referer`；其他方法遇到重定向直接报错，请求体不会被重发到别处。
- **同步的日志与诊断**：只记录阶段、请求数、字节数、耗时、HTTP 状态码与错误种类；服务器地址与主机、账号、密码、口令、密钥、远端路径与文件名、设备名、记录内容都不写入日志与错误信息（界面上的错误详情只由固定词汇拼成）。诊断包把 `sync.server_url` 换成 `scheme://***`、`sync.username` 换成 `***`，不打包 `sync.db`；`RuntimeInfo.sync` 不含地址、账号与设备名。

报告安全问题的方式见 [SECURITY.md](../SECURITY.md)。

## 性能设计

预算：

| 指标 | 预算 |
| --- | --- |
| 冷启动到托盘就绪 | ≤ 800 ms |
| 按下快捷键到面板出现 | ≤ 100 ms |
| 空闲 CPU | 接近 0（没有周期性唤醒） |
| 前端资源 / 覆盖层入口 | ≤ 1.5 MB / ≤ 15 KB（压缩前） |
| 安装包 | dmg ≤ 12 MB、NSIS ≤ 10 MB、deb / rpm ≤ 15 MB、AppImage ≤ 90 MB |

做法：

- **常驻的只有必要的东西**：tokio 只开 2 个 worker；托盘、快捷键与剪贴板监听都由系统事件驱动（macOS 剪贴板是唯一的轮询：用户活跃时 4 Hz，安静 30 秒 / 5 分钟后降到 1 Hz / 0.2 Hz，锁屏或显示器睡眠时 30 秒一次）；扩展触发全部关闭时不安装任何钩子。
- **按需创建**：设置、引导、固定窗口、悬浮剪贴板与覆盖层都在需要时创建、关闭即销毁；HTTP 客户端、SQLite 连接、钥匙串在第一次使用时初始化。悬浮剪贴板打开期间，列表里的相对时间按「下一次会变化的时刻」排一次性定时器，页面不可见时暂停。
- **剪贴板的新能力不常驻**：智能识别与变换只在需要时运行（检视结果缓存最近 8 条）；验证码到期用一次性任务排程，读取时也过滤已过期的条目，不需要周期清理；连续粘贴的按键钩子只在队列进行中存在。
- **同步关闭时几乎零成本**：不建 HTTP 客户端、不开 `sync.db`、不起线程、没有计时器、不发请求、不读钥匙串；只有两个常驻的异步任务分别等待设置变化与同步状态变化，由事件唤醒，平时不运行。开启后只由事件驱动：本机改动在有待上传的内容时安排一次性去抖（2 s，连续改动最长 10 s），这是同步唯一的计时器；取回远端改动只在用户活动时进行（面板显示、托盘打开、设置窗口获焦、再次启动），按间隔节流；暂时性失败只记下退避截止时间（1 min 起翻倍，最长 30 min），不设计时器，等下一次活动再判断。启动时只在托盘就绪后做本机工作，不联网、不读钥匙串；退出时只把改动日志落盘。没有变化的一轮只有 1 个请求；同步用自己的 HTTP 客户端（同一套 reqwest + rustls），在一轮同步的第一个请求时创建，排队的几轮跑完即随连接池一起释放；图片等 blob 按流加密、上传与下载，不整块读入内存。
- **离线模型不常驻**：没用到时不读盘、不起进程与线程、不建 HTTP 客户端、没有定时器；推理进程按需启动、连续使用复用，闲置（一次性定时器，每次使用重排）或内存紧张时结束进程释放内存与显存，模型文件保留。内存压力监听只在有模型加载时安装。下载进度按 ≥ 250 ms 或 ≥ 1% 节流广播。
- **面板预热与复用**：托盘就绪后异步加载面板页面，之后一直复用。隐藏时前端停掉全部 `requestAnimationFrame`、定时器与 Observer，CSS 动画暂停；Windows 上把 WebView2 设为不可见并降低内存目标。
- **面板回收**：macOS 上面板隐藏约 45 秒后释放 WebView 的页面内存，下次唤出时重新加载（仍在 100 ms 预算内）；「空闲时卸载面板」是可选的更激进策略。
- **流式合并**：增量按 16 ms 合并后再跨 IPC，前端按帧批量写入。
- **热点包单独优化**：发布构建整体按体积优化（`opt-level = "s"`），截图编码与缩放、哈希、SQLite 等热点包单独用 `opt-level = 3`。

度量工具在 `scripts/perf/`：冒烟模式（`WISP_SMOKE=1`）记录启动各阶段耗时、内存与 100 次连续唤出的延迟分布，CI 用 `smoke-gate.mjs` 与基线比较；`measure-macos.sh` 等脚本测量长时间空闲的 CPU、唤醒次数与内存。用法见[开发指南](development.md#性能测量)。

## Cargo feature

| feature | 所在 crate | 默认 | 作用 |
| --- | --- | --- | --- |
| `mouse-hook` | wisp-desktop、wisp-platform | 开 | 编入扩展触发（浮标、双击修饰键、鼠标键，运行时默认关闭）与连续粘贴的按键拦截（只在连续粘贴期间安装） |
| `fake-llm` | wisp-desktop（→ wisp-llm `fake`、wisp-local `fake`） | 关 | 内置假模型，设置 `WISP_FAKE_LLM=1` 时启用（离线翻译、离线识别与 PaddleOCR 也改用假引擎，离线模型视为已下载）；`pnpm dev`、`pnpm build:debug`、冒烟与性能测试使用，发布版不编入 |
| `ort` | wisp-local | 关（wisp-desktop 依赖它时打开） | ONNX Runtime（`ort`，`load-dynamic`）与 oar-ocr：PP-OCRv6 与版面检测的推理，只在 ORT 工作进程里执行。不开时 crate 的其余部分（目录、下载、存储、拼装、云端客户端……）照常编译与测试 |
| `fake` | wisp-local | 关 | 假引擎（固定输出、可配置的加载延迟与失败），供编排器测试与 `WISP_FAKE_LLM=1` 的开发运行 |
| `wayland` | wisp-desktop、wisp-platform、wisp-capture | 关 | Wayland 会话的降级实现（wl-clipboard、XDG 门户截图） |
| `wgc` | wisp-desktop、wisp-capture | 关 | Windows 截图改用 Windows.Graphics.Capture（GDI 截图异常时的备选） |
| `eval` | wisp-prompts | 关 | 提示词评测运行器（开发用） |
