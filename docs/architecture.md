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
  windows        WindowManager：面板显示管线、定位与尺寸、固定窗口、截图覆盖层、设置窗口、wisp:// 协议
  clipboard_svc  剪贴板监听、记录、检索、粘贴回原应用
  settings       settings.json 等配置文件、热加载、钥匙串、模型能力缓存
  tray · updater · ipc · bus
      │ 调用
      ▼
  wisp-llm · wisp-prompts · wisp-clipboard · wisp-capture · wisp-platform · wisp-types

        ▲ │  Tauri IPC：命令（invoke）· 事件 · Channel 流 · wisp:// 资源
        │ ▼

WebView（React 19 + Motion + Tailwind CSS 4）
  panel（翻译 · OCR · 剪贴板 · 输入 · 提示）· settings / onboarding · pinned · overlay（原生 TS）
```

几条贯穿全局的约束：

- **Rust 是事实来源**。流程状态、配置、剪贴板历史、API Key 都在 Rust 进程里；WebView 只负责呈现，随时可以销毁重建。
- **平台差异收敛在少数位置**。`#[cfg(target_os)]` 只出现在 `wisp-platform`、`wisp-capture`、`apps/desktop/src-tauri/src/windows/native/` 以及 `lib.rs` 的插件注册里，其余代码通过 trait 调用平台能力。
- **非桌面 crate 不依赖 Tauri**。`crates/*` 都可以脱离 WebView 单独测试和做基准；`scripts/check-contract.mjs` 检查这条边界。
- **空闲时不做事**。没有常驻的周期定时器，窗口按需创建，HTTP 客户端、SQLite 连接、钥匙串句柄都在第一次使用时才初始化（见[性能设计](#性能设计)）。

## 仓库结构

```text
wisp/
├── apps/desktop/                 Tauri 应用
│   ├── src/                      前端（React + TypeScript）
│   ├── src-tauri/                Rust 入口（crate 名 wisp-desktop）
│   └── panel.html / settings.html / pinned.html / overlay.html   四个页面入口
├── crates/
│   ├── wisp-types/               共享数据类型（IPC、配置、持久化）
│   ├── wisp-llm/                 四种协议的流式客户端
│   ├── wisp-prompts/             翻译与 OCR 的提示词、风格、语言检测、后处理
│   ├── wisp-clipboard/           剪贴板历史存储（SQLite + FTS5）
│   ├── wisp-capture/             屏幕捕获、裁剪、编码
│   └── wisp-platform/            操作系统集成层（三端实现）
├── vendor/                       通过 [patch.crates-io] 引入并打了补丁的上游 crate（wry、tao、tray-icon、notify）
├── scripts/                      检查、版本号、性能测量、截图、发布脚本
├── docs/                         文档
└── .github/                      CI 与发布工作流
```

Cargo workspace 的成员是 `crates/*` 与 `apps/desktop/src-tauri`；pnpm workspace 包含仓库根与 `apps/*`。依赖版本由 `Cargo.lock` 与 `pnpm-lock.yaml` 锁定，Tauri 相关的 crate 与前端 npm 包在清单里写精确版本，Rust 工具链由 `rust-toolchain.toml` 锁定。

## Rust crate

```text
wisp-types  ←── 所有 crate 都依赖它（纯数据，只依赖 serde）
wisp-llm       → wisp-types
wisp-prompts   → wisp-types（eval feature 另依赖 wisp-llm）
wisp-clipboard → wisp-types
wisp-capture   → wisp-types
wisp-platform  → wisp-types
wisp-desktop   → 以上全部 + tauri
```

crate 之间的横向依赖只允许指向 `wisp-types`。

| crate | 职责 | 不负责 |
| --- | --- | --- |
| `wisp-types` | 所有跨 crate、跨进程（IPC）与持久化用到的数据类型及其默认值；事件名常量；宽松的配置解析 | 业务逻辑 |
| `wisp-llm` | OpenAI Chat Completions、OpenAI Responses、Anthropic Messages、Google Gemini 四种协议的请求构造与 SSE 流式解析；`<think>` 标签剥离；错误归一化；Base URL 规范化；模型列表与视觉能力判定；连接测试；参数降级与能力缓存；超时、重试、取消；16 ms 合并器；服务模板 | 提示词内容、Key 的存储 |
| `wisp-prompts` | 内置风格与用户覆盖的合并；模板渲染；翻译、词典式、润色、校对、提升译文、OCR 的提示词装配；语言检测与翻译方向；术语匹配；token 估算；流式安全的后处理；缓存键；评测运行器（`eval` feature） | 发请求 |
| `wisp-clipboard` | SQLite（WAL）的打开与迁移；记录管线（隐私规则 → 规范化 → 去重 → 写入 → 缓存）；类型推断；疑似密钥检测与应用排除名单；FTS5 trigram 检索与高亮；保留策略与清理；图片与缩略图文件；最近条目的内存缓存；识别历史 | 读取系统剪贴板（属于平台层） |
| `wisp-capture` | 并行捕获所有显示器（macOS 用 ScreenCaptureKit，Windows 用 xcap，X11 用 MIT-SHM / GetImage）；逻辑坐标到物理像素的换算与裁剪；按长边缩放；PNG / JPEG 编码与体积回退；窗口列表 | 覆盖层窗口（属于桌面应用） |
| `wisp-platform` | 取词、粘贴、焦点、权限、应用信息、显示器与外观、剪贴板读写与监听、外部点击、快捷键环境、输入钩子与浮标的 trait 及三端实现，见[平台层](#平台层) | 窗口的创建与绘制 |

提示词模板的正文在 `crates/wisp-prompts/templates/`，以 `include_str!` 编入二进制；内置风格的权威定义在代码里，用户的修改只保存与默认值不同的字段。

## 桌面应用（src-tauri）

`apps/desktop/src-tauri/src/lib.rs` 只做装配：运行时、插件、`wisp://` 协议、setup 与命令注册。服务之间不互相持有引用，统一从 `state.rs` 的 `Deps` 构造，运行时通过 `app.state::<AppState>()` 取得彼此，避免循环依赖。

| 模块 | 内容 |
| --- | --- |
| `app/` | 启动顺序（托盘就绪之后的工作全部异步）、tokio 运行时与 WebView 创建前的环境预检、路径、日志、外观、托盘与通知的文案（`i18n.rs`）、权限状态、开机自启、单实例与 `--trigger` 命令行、诊断导出、性能埋点、冒烟模式、设置变化的统一副作用（`reactor.rs`） |
| `bus.rs` | 应用内命令总线：托盘、快捷键、扩展触发、命令行、引导页把动作投递到这里，由 `orchestrator` 消费 |
| `orchestrator/` | 每次由快捷键等发起的流程是一个状态机（`fsm.rs` 的「状态 × 事件」转移表）：取词 → 未就绪检查 → 语言与方向 → 词典式判定 → 长文本确认 → 缓存 → 请求 → 后处理 → 合并 → 推送。`registry.rs` 保存每个流的事件日志，支持回放与所有权转移（面板固定成独立窗口时流不中断）；`cache.rs` 是翻译结果的 LRU 缓存；`captures.rs` 管理截图的冻结帧与裁剪图；`env.rs` 是与外界的接缝，单元测试用内存实现驱动状态机 |
| `windows/` | `WindowManager` 与面板显示管线（见[窗口体系](#窗口体系)）；`native/` 是窗口层的平台实现 |
| `hotkeys/` | 由设置算出每个动作的候选组合并按「首选 → 备选 → 留空」注册；分层冲突检测（静态表在 `data/`）；录制与测试；全局 Esc；扩展触发（浮标、双击修饰键、鼠标键） |
| `clipboard_svc/` | 监听回调 → 工作线程读取 → `wisp-clipboard` 记录 → 推送 `clipboard:changed`；粘贴流程（写入剪贴板、抑制自身变更、隐藏面板、还原焦点、模拟粘贴键）；来源应用图标；空闲清理 |
| `settings/` | `settings.json`、`styles.json`、`glossary.json`、`state.json` 的读写（原子写入、JSON Merge Patch、文件监听热加载）；钥匙串；模型能力缓存 |
| `ipc/` | 全部 Tauri 命令。只做参数校验与分发，不写业务逻辑 |
| `tray/` | 托盘菜单、状态行与角标；Linux 上检测托盘宿主，没有时退化为设置窗口加系统通知 |
| `updater/` | 检查更新（启动 30 s 后一次，之后惰性检查，间隔至少 24 h）、下载、验签、安装；三种策略：只提示、自动安装、关闭 |
| `assets.rs` | `wisp://` 协议的资源注册表：截图帧、裁剪图、剪贴板图片与缩略图、应用图标 |

## 前端

```text
apps/desktop/src/
├── entries/        四个页面的入口：panel.tsx、settings.tsx（也承载引导）、pinned.tsx、overlay.ts
├── panel/          面板外壳：视图路由、进入 / 退出动效、高度上报、拖动
├── views/          translate · ocr · clipboard · input · toast 五个视图（面板与固定窗口共用）
├── settings/       设置窗口的各个分组
├── onboarding/     首次启动引导
├── pinned/         固定窗口
├── overlay/        截图框选（原生 TypeScript + DOM，不加载 React / Motion）
├── shared/
│   ├── ipc/        唯一允许引用 @tauri-apps/* 的目录：命令封装、事件、类型镜像、黄金样例、浏览器 mock
│   ├── motion/     动效令牌与预设（时长、曲线、弹簧参数）
│   ├── styles/     设计令牌（颜色、圆角、阴影等 CSS 变量）
│   ├── ui/         通用组件
│   ├── stream/     流式文本的追加写入
│   ├── markdown/   增量渲染的 Markdown 子集
│   ├── i18n/ · keys/ · hooks/ · perf/
├── locales/        zh-CN 与 en-US 的界面文案，按命名空间分文件
└── generated/      脚本生成的文件（第三方许可清单），不要手改
```

- **页面入口**：Vite（Rolldown）构建四个 HTML 入口，共享的 chunk 自动拆分。面板的五个视图全部静态导入，首次唤出任何视图都不需要再加载代码；设置与引导互相懒加载；覆盖层入口不含 React / Motion 运行时。`scripts/check-size.mjs` 检查这些依赖关系与体积预算。
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
| 事件 | Rust → 前端 | 低频广播：`panel:prepare`、`clipboard:changed`、`settings:changed`、`appearance:changed`、`permissions:changed`、`hotkeys:changed`、`update:status` 等 |
| Channel 流 | Rust → 前端 | 翻译与 OCR 的流式输出（`StreamEvent`） |
| `wisp://` 协议 | 前端读取 | 截图帧、裁剪图、剪贴板图片与缩略图、应用图标等二进制资源 |

约定：

- 命令名、事件名、字段名、枚举值统一用 snake_case；事件名形如 `域:动作`。前端的封装函数用 camelCase（`api.translateStart(...)`），但参数对象的键保持 snake_case。
- 命令按领域分在 `ipc/cmd_*.rs`：窗口与面板、流式请求与截图覆盖层、剪贴板、设置与风格、模型服务与 Key、快捷键、应用与权限、更新。命令清单只在 `ipc/mod.rs` 的 `commands!` 宏里写一处。
- **API Key 只进不出**：前端可以设置、删除 Key，没有读取 Key 的命令；所有请求由 Rust 发出。
- 字符串、浮点数与集合参数在命令函数体的第一步校验（长度、条数、有限性），校验失败返回 `invalid_argument`。
- 流式事件的序列是 `Start` →（`Meta` | `Phase` | `Notice` | `Delta`）* → 恰好一个 `Done` 或 `Error`；取消时以 `Error{code: cancelled}` 结束。`Delta` 是经过后处理与合并之后的增量，前端只追加。`stream_attach` 先回放已有事件再推送实时事件，固定窗口接管面板里进行中的流就是靠它。

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
| `settings` | `settings.html` | 设置窗口，按需创建，记住位置与尺寸；macOS 上存在期间应用切换为普通应用（出现在 Dock 与 ⌘Tab） |
| `onboarding` | `settings.html` | 首次启动引导，与设置共用页面入口，按窗口 label 区分 |
| `overlay-<n>` | `overlay.html` | 截图框选的全屏覆盖层，每个显示器一个；确认或取消后销毁（Windows 上主显示器的覆盖层预先建好、隐藏复用） |
| 浮标 | 原生窗口 | 扩展触发的浮标，不用 WebView，原生绘制 |

**面板显示时序**：Rust 先在隐藏状态下确定位置与宽度，emit `panel:prepare`；前端以透明状态同步渲染并测量内容高度，调用 `panel_ready`；Rust 在这个命令里显示窗口，命令返回后前端播放进入动效。这样不会闪出上一次的内容，也不会出现空白窗口。

**两种帧模式**（面板高度随流式内容增长时的做法）：

- **Native（macOS）**：窗口就是卡片本身。前端只上报目标高度，Rust 用与前端相同参数的弹簧逐帧调整窗口，帧时钟只在动画期间存在。材质用原生的 `NSVisualEffectView`。
- **Css（Windows / Linux）**：窗口是透明的，卡片在窗口内用 CSS 画出阴影与圆角；窗口按阶梯放大，卡片在窗口内做动画。卡片之外的透明区域不拦截点击：Windows 在面板可见期间用低级鼠标钩子按光标位置切换窗口的 `WS_EX_TRANSPARENT`；X11 在面板可见期间以 30 Hz 读取光标位置，光标落在卡片外时整窗忽略鼠标。面板隐藏后两者都停止。Wayland 或关闭了合成时窗口不透明，卡片铺满窗口。

**关闭面板**：翻译 / OCR 视图不是 key 窗口，由全局 Esc 与外部点击监听关闭（在原应用里拖选文字不算点击）；剪贴板与输入视图失去焦点时关闭。

## 平台层

`wisp-platform` 以 trait 描述操作系统能力，上层只通过 `Platform` 门面调用：

| trait | 能力 |
| --- | --- |
| `SelectedText` | 读取当前选中的文字（含选区矩形） |
| `Paste` | 写剪贴板、向目标应用模拟粘贴键 |
| `FocusTracker` | 前台应用、激活、是否以管理员身份运行 |
| `Permissions` | 辅助功能、屏幕录制、输入监控的状态与授权入口 |
| `AppInfo` | 应用图标、显示名、运行中的应用 |
| `CursorScreen` | 显示器、光标位置、系统强调色、减少动效 / 透明度、深色模式及其变化 |
| `ClipboardAccess` | 读取、全类型快照与恢复、变更监听、抑制自身写入 |
| `OutsideClick` | 外部点击监听（关闭面板用） |
| `HotkeyEnv` / `KeyGrab` | 系统保留的快捷键、键盘布局；X11 上的事件驱动快捷键注册 |
| `InputHooks` / `BobberWindow` | 扩展触发的输入钩子与浮标窗口（`mouse-hook` feature） |

约束：可能阻塞的调用（无障碍接口、UI Automation、X11 selection、剪贴板）都有硬超时；钩子回调只做常数时间的工作，然后交给工作线程；各实现惰性初始化，`Platform::create` 不阻塞。

| 能力 | macOS | Windows | Linux（X11） |
| --- | --- | --- | --- |
| 取词 | AX，回退模拟 ⌘C | UI Automation，回退模拟 Ctrl+C | PRIMARY 选区，回退模拟复制（终端不发 Ctrl+C） |
| 剪贴板监听 | `changeCount` 轮询（用户活跃时 4 Hz，安静后与锁屏时降频） | `AddClipboardFormatListener`，事件驱动 | XFixes，事件驱动 |
| 模拟按键 | CGEvent | SendInput | XTest |
| 截图 | ScreenCaptureKit | xcap（GDI；可选 Windows.Graphics.Capture） | MIT-SHM / GetImage |
| 隐私标记 | `org.nspasteboard.ConcealedType` 等 | `ExcludeClipboardContentFromMonitorProcessing` 等 | `x-kde-passwordManagerHint` |

模拟复制的回退在读取之前对剪贴板做全类型快照，读完立即恢复，期间产生的变更不会进入剪贴板历史。

Wayland 会话下取词、全局快捷键、截图与剪贴板监听都不可用，能力的可用性经 `Platform::info()` 报告给上层与界面。`wayland` feature 提供基于 wl-clipboard 与 XDG 门户的降级实现，发布版没有编入。

窗口层自己的平台代码在 `apps/desktop/src-tauri/src/windows/native/{macos,windows,linux}.rs`：NSPanel（借助 `tauri-nspanel`）、原生材质与弹簧动画、WebView2 的可见性与内存目标、X11 上透明区域的点击穿透等。

## 数据与配置

| 内容 | macOS | Windows | Linux |
| --- | --- | --- | --- |
| 配置：`settings.json`、`styles.json`、`glossary.json`、`state.json` | `~/Library/Application Support/wisp/` | `%APPDATA%\wisp\` | `$XDG_CONFIG_HOME/wisp/` |
| 数据：`clipboard.db`（SQLite，WAL）与 `blobs/` | `~/Library/Application Support/wisp/` | `%LOCALAPPDATA%\wisp\` | `$XDG_DATA_HOME/wisp/` |
| 日志：`wisp.log`，2 MB × 5 滚动 | `~/Library/Logs/wisp/` | `%LOCALAPPDATA%\wisp\logs\` | `$XDG_STATE_HOME/wisp/logs/` |
| API Key | 钥匙串，服务 `wisp`、账户为服务 ID | 凭据管理器，`<服务 ID>.wisp` | Secret Service，服务 `wisp`、账户为服务 ID |

- 目录名固定为 `wisp`，不使用带应用标识的目录。
- 所有 JSON 文件原子写入（先写临时文件再改名）。`settings.json` 允许手工编辑：文件变化后自动重新加载，只做宽松解析、不回写；解析失败时保留上一次有效的设置并在设置窗口里提示。
- `settings.json` 带 `schema_version`，启动时迁移；内置风格以代码为准，`styles.json` 只保存覆盖项与自定义风格；`state.json` 保存运行状态（引导是否完成、窗口位置、上次检查更新的时间等）。
- 模型能力缓存（是否支持图片、哪些参数被拒绝过）写在 `settings.json` 的模型条目上。
- Linux 没有 Secret Service 时，Key 只保存在内存里（仅本次运行），绝不明文落盘。
- 剪贴板数据库带 schema 版本；从新版本降级时不会删除或改写新版本的数据库，只停用剪贴板历史并提示。

## 安全与隐私

- **Key 不进 WebView**：前端只知道服务 ID；请求、连接测试、模型列表都在 Rust 里完成。
- **最小权限**：`capabilities/default.json` 只给 WebView 开放事件监听与窗口拖动 / 关闭；插件的 JS API 都不使用，窗口能力一律走应用自己的命令。
- **严格 CSP**：没有远程资源，脚本只能来自应用自身；`freezePrototype` 开启。
- **日志不含正文**：只记录耗时、状态码、模型名、取词方式等元数据；原文、译文、剪贴板内容、截图、Key、请求头的值与 URL 查询参数都不写入日志。诊断包导出时再次脱敏。
- **截图只在内存里**：冻结帧随截图会话结束释放；只有用户打开「识别历史」时才写盘。
- **剪贴板隐私**：疑似密码、令牌、私钥的内容不记录；密码管理器默认排除；遵守应用的「不要记录」标记。被隐私规则跳过的复制不留下任何痕迹（不取图标、不写日志正文）。
- **网络请求只有两类**：用户配置的模型服务，以及检查更新（可关闭）。

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
- **按需创建**：设置、引导、固定窗口与覆盖层都在需要时创建、关闭即销毁；HTTP 客户端、SQLite 连接、钥匙串在第一次使用时初始化。
- **面板预热与复用**：托盘就绪后异步加载面板页面，之后一直复用。隐藏时前端停掉全部 `requestAnimationFrame`、定时器与 Observer，CSS 动画暂停；Windows 上把 WebView2 设为不可见并降低内存目标。
- **面板回收**：macOS 上面板隐藏约 45 秒后释放 WebView 的页面内存，下次唤出时重新加载（仍在 100 ms 预算内）；「空闲时卸载面板」是可选的更激进策略。
- **流式合并**：增量按 16 ms 合并后再跨 IPC，前端按帧批量写入。
- **热点包单独优化**：发布构建整体按体积优化（`opt-level = "s"`），截图编码与缩放、哈希、SQLite 等热点包单独用 `opt-level = 3`。

度量工具在 `scripts/perf/`：冒烟模式（`WISP_SMOKE=1`）记录启动各阶段耗时、内存与 100 次连续唤出的延迟分布，CI 用 `smoke-gate.mjs` 与基线比较；`measure-macos.sh` 等脚本测量长时间空闲的 CPU、唤醒次数与内存。用法见[开发指南](development.md#性能测量)。

## Cargo feature

| feature | 所在 crate | 默认 | 作用 |
| --- | --- | --- | --- |
| `mouse-hook` | wisp-desktop、wisp-platform | 开 | 编入扩展触发（浮标、双击修饰键、鼠标键）；运行时默认关闭 |
| `fake-llm` | wisp-desktop（→ wisp-llm `fake`） | 关 | 内置假模型，设置 `WISP_FAKE_LLM=1` 时启用；`pnpm dev`、`pnpm build:debug`、冒烟与性能测试使用，发布版不编入 |
| `wayland` | wisp-desktop、wisp-platform、wisp-capture | 关 | Wayland 会话的降级实现（wl-clipboard、XDG 门户截图） |
| `wgc` | wisp-desktop、wisp-capture | 关 | Windows 截图改用 Windows.Graphics.Capture（GDI 截图异常时的备选） |
| `eval` | wisp-prompts | 关 | 提示词评测运行器（开发用） |
