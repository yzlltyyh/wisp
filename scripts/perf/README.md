# scripts/perf —— 性能采集、冒烟与门禁

三端空闲开销的采集脚本、`WISP_SMOKE` 冒烟、性能记录的格式（`wisp.perf/1`）与门禁。CI（`.github/workflows/ci.yml`）的冒烟、会话后测量与面板回收对照都调用这里的脚本，门禁读 `baselines/` 下的基线。

| 文件 | 作用 |
| --- | --- |
| `measure-macos.sh` / `measure-linux.sh` / `measure-windows.ps1` | 各平台空闲采样（主进程与 WebView 进程各自的 RSS、CPU、唤醒），交给 `record.mjs` 汇总；都支持 `--dry-run`（`-DryRun`），CI 在对应 runner 上跑；都支持 `--exercise` / `--settle`（`-Exercise` / `-Settle`）测「用过之后」的空闲，并把应用日志交给 exercise（`WISP_LOG`）与 record.mjs 核对（`idle.session`）。`--app` 时带 `--autostart` 启动；macOS / Linux 默认隔离 HOME（Linux 连同 XDG 目录）并在其中写入测量设置（`--settings preset`：更新检查关闭、日志级别 debug，见 `exercise.mjs`）；**Windows 无法隔离**（Wisp 用已知文件夹），`-App` 必须加 `-UseRealProfile` 确认。macOS 另有 `--window-end`（采样终点钉在 exercise 结束后的固定秒数） |
| `exercise.mjs` | 会话后测量的「用一次 Wisp」：往剪贴板灌一批有代表性的条目（文本 / 代码 / 链接 / 颜色值 / 长文 / PNG），依次唤出、关闭输入与剪贴板面板，**每一步由应用的 debug 日志核对**（失焦先隐藏、再次 `--trigger` 把面板重新打开时再关一次），结束时保证没有面板在屏幕上；`--write-settings <settings.json>` 写入测量设置 |
| `macos-thread-wakeups.py` / `linux-thread-wakeups.py` / `windows-thread-wakeups.ps1` | 空闲唤醒的**按线程归因**：计数窗口首尾各读一次每个线程的唤醒 / 上下文切换与 CPU，列出哪些线程在醒、各自的次数与等待点。Windows 版不需要管理员权限：线程名（GetThreadDescription）、起始模块 + 偏移、等待原因，`-Stacks <文件>` 另用 cdb 非侵入附加转储等待栈，`-SelfTest` 自检 |
| `windows-tray-probe.ps1` | Windows 托盘图标「离开检测」定时器的复测（`vendor/tray-icon` 的补丁）：找到 Wisp 的托盘窗口与图标，`-Mode Input`（`SendInput`，经资源管理器转发，最接近真实悬停）或 `-Mode Post`（直接投递托盘回调消息，合成）走「停在图标上」「停 100 ms 后一下跳走」两个场景，只数托盘窗口所在 UI 线程的上下文切换；需要交互桌面，会移动光标（结束时放回）；退出码 0 通过、1 定时器没停、2 资源管理器没有转发（结论不成立） |
| `macos-webkit.py` | macOS 上按「负责进程」找出属于 Wisp 的 WebKit XPC 进程并取 phys_footprint（measure-macos.sh、run-smoke.mjs 调用） |
| `compare-records.mjs` | 几份性能记录的空闲段并排成表（CI 的会话后测量、panel-recycle 作业的摘要用） |
| `macos-webview-probe.swift` | macOS dry-run 用的最小 WKWebView 应用（验证 WebView 归属与 WebView 的 CPU / 唤醒采样） |
| `record.mjs` | 原始样本 → 性能记录（`wisp.perf/1`），按 schema 校验后写 `target/perf/<date>-<os>.json`（`--out` 可改） |
| `latency-report.mjs` | 从 Wisp 日志或冒烟报告计算 prepare→shown 的 p50 / p95；另按 `page=` 分组（冷唤出单列）、拆分 ready→窗口上屏 / 成为 key、统计每次启动后的第一次唤出与 Overlay 上屏 |
| `run-smoke.mjs` | 以 `WISP_SMOKE=1 WISP_FAKE_LLM=1` 启动构建产物，等它写出冒烟报告并退出；`--webview`（默认关闭）时在 macOS / Linux 上每秒采样 WebView 进程内存，写进报告的 `webview` 段（只记录，见下）|
| `smoke-gate.mjs` | 冒烟报告 / 性能记录与基线比较的门禁（`--ci` 为 CI 口径；`--require-webview` / `--expect-state` / `--expect-views` / `--webview-gate` 见「门禁规则」） |
| `perf-record.schema.json` / `smoke-report.schema.json` | 两种 JSON 的正式格式（JSON Schema 2020-12） |
| `baselines/` | 门禁的基线（见「基线」）：`macos.json`（本机 macOS 26）、`macos-15.json`（GitHub 的 macos-15 runner） |
| `examples/` | 格式样例与自检用的真实记录（`smoke-gate.mjs --self-test` 会校验它们）：`smoke-report.failed-run.json` 是一次没有通过的真实冒烟输出（TranslateText 没有被处理，按预期不通过；`log_file` 换成了相对路径）；`perf-post-{hot,recycled}.macos-15.json` 是生成 runner 基线的两份会话后记录 |

## 常用命令

```bash
# 本机 600 s 空闲测量：经 LaunchServices 启动 release 包（WebView 精确归属）→ 预热 30 s → 测量 → 退出。
# 默认以隔离的 HOME（临时目录）与 --autostart 启动：不读写真实配置 / 剪贴板历史、不弹引导（--home / --app-args 可改）；
# 本机已有 Wisp 在运行时用另一个 identifier 构建，否则新实例会经单实例锁转发给已有的 Wisp
pnpm tauri build --config src-tauri/tauri.no-updater.conf.json --config '{"identifier":"io.github.yzlltyyh.wisp.perf"}'
bash scripts/perf/measure-macos.sh --app target/release/bundle/macos/Wisp.app --idle 600 --settle 60
#   测量已在运行的实例：--pid <pid>（从访达 / open 启动的实例才能精确归属 WebView）
#   可加 --latency-log ~/Library/Logs/wisp/wisp.log 与 --smoke target/smoke-report.json（可重复）

# 冒烟：fake-llm 构建 → 运行 → 门禁
pnpm tauri build --debug --no-bundle --features fake-llm --config src-tauri/tauri.no-updater.conf.json
node scripts/perf/run-smoke.mjs --profile debug
node scripts/perf/smoke-gate.mjs target/smoke-report.json scripts/perf/baselines/macos.json        # 本机门禁
node scripts/perf/smoke-gate.mjs target/perf/<date>-macos.json scripts/perf/baselines/macos.json   # 空闲记录与基线比较

# 会话后的 WebView 内存（macOS 预算 ≤ 55 MiB）：exercise.mjs 灌剪贴板（30 条文本 + 3 张 PNG）、用一次输入面板与
# 剪贴板面板，然后测两个窗口（在仓库根目录运行；会改写系统剪贴板，本机运行时灌完写回原来的文本）——
#   回收前：静置 5 s，采样终点钉在 exercise 结束后 32 s（--window-end；起点随脚本开销浮动，约 7–10 s），离回收（最后一次
#           隐藏 + 45 s ≈ exercise 后 43 s）留约 11 s；
#   回收后：静置 60 s、采样 60 s（约 63–125 s）。CI 的 macOS 冒烟作业跑的就是这两段。
# --expect-state 由应用日志核对：采样窗口里面板已隐藏（debug 日志）、窗口的实际状态是预期的那一种（回收时长
# windows::manager::panel_recycle_after 变了会直接失败）；--expect-views 核对两个视图都用到了；--require-webview 要求 WebView
# 精确归属且 3 个进程都测到；--webview-gate hard 才是硬门禁：回收后按 55 MiB，回收前按 100 MiB 防退化上限。给了基线时
# 主进程按基线的 gate.session_rss_main 判退化、核对 launch_env=true（MallocSpaceEfficient 生效；CI 在 runner 上用
# baselines/macos-15.json）。隔离 HOME 里的测量设置关闭了更新检查；预热 35 s
EX='node scripts/perf/exercise.mjs --views input,clipboard --texts 30 --images 3'
bash scripts/perf/measure-macos.sh --app target/release/bundle/macos/Wisp.app --warmup 35 --exercise "$EX" --settle 5 --idle 25 --window-end 32 --out target/perf-post-hot.json
node scripts/perf/smoke-gate.mjs target/perf-post-hot.json scripts/perf/baselines/macos.json --ci --require-webview --expect-state hot --expect-views input,clipboard --webview-gate hard
bash scripts/perf/measure-macos.sh --app target/release/bundle/macos/Wisp.app --warmup 35 --exercise "$EX" --settle 60 --idle 60 --out target/perf-post-recycled.json
node scripts/perf/smoke-gate.mjs target/perf-post-recycled.json scripts/perf/baselines/macos.json --ci --require-webview --expect-state recycled --expect-views input,clipboard --webview-gate hard
node scripts/perf/compare-records.mjs 回收前=target/perf-post-hot.json 回收后=target/perf-post-recycled.json

# 给已有的基线补 / 刷新冒烟段（空闲段原样保留）
node scripts/perf/record.mjs --update scripts/perf/baselines/macos.json --smoke target/smoke-release.json --smoke target/smoke-debug.json

# 自检（不需要应用）
bash scripts/perf/measure-macos.sh --dry-run      # 编译 WKWebView 探针，验证 WebView 归属与采样
node scripts/perf/record.mjs --self-test
node scripts/perf/smoke-gate.mjs --self-test
node scripts/perf/latency-report.mjs --self-test
node scripts/perf/run-smoke.mjs --self-test
node scripts/perf/compare-records.mjs --self-test
node scripts/perf/exercise.mjs --self-test
```

### WebView 进程的归属与测量

WebView 的内存、CPU、唤醒与主进程**分别**采集：「空闲零 CPU」有一半在前端（隐藏后必须停掉的 rAF、定时器、Observer、CSS 动画），这类泄漏只会体现在 WebView 进程上。

| 平台 | 归属方式 | `webview_attribution` |
| --- | --- | --- |
| macOS | `com.apple.WebKit.*` 中「负责进程」（`responsibility_get_pid_responsible_for_pid`）是 Wisp 的那些。只有经 LaunchServices 启动（访达、`open`、登录项、`--app <Wisp.app>`）时 Wisp 才是自己的负责进程，匹配是精确的；从终端 / IDE 直接执行可执行文件时（`./target/release/wisp-desktop`、`pnpm dev`）负责进程是终端，只能按「同一负责进程且启动不早于 Wisp」近似匹配，可能混入同一终端里随后启动的其他 WebKit 应用 | `responsible` / `responsible-approx`（门禁告警） |
| Linux | 主进程后代中名字以 `WebKit` 开头的进程（`WebKitWebProcess`、`WebKitNetworkProcess`） | `descendants` |
| Windows | 主进程后代中的 `msedgewebview2.exe` | `descendants` |

一个 WebView 进程都没找到时，WebView 的各项指标记为 **null**（不是 0），`notes` 里写明原因；门禁把它判为「WebView 预算未验证」（本机门禁失败、CI 告警），不会当成 0 MiB 放行。

## 性能记录 `wisp.perf/1`

`record.mjs` 写出的记录（默认 `target/perf/<date>-<os>.json`）与 `baselines/<os>.json` 都是这个格式。正式定义见 `perf-record.schema.json`，样例见 `examples/perf-record.example.json`。

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `schema` | `"wisp.perf/1"` | 格式版本 |
| `ts` | string | 生成时间（ISO 8601，UTC） |
| `os` | `macos` / `windows` / `linux` | |
| `os_version`、`arch`、`machine` | string \| null | 系统版本、CPU 架构、机型（便于解释基线差异） |
| `version` | string | 应用版本（默认取 `apps/desktop/package.json`） |
| `build` | `release` / `debug` / null | 空闲测量所用的构建类型 |
| `idle` | object \| null | 空闲测量，见下 |
| `latency` | object \| null | 延迟统计，见下 |
| `smoke` | object \| null | 冒烟摘要，**按构建类型分组**：`{ release?: {…}, debug?: {…} }`，见下 |
| `gate` | object（可选，仅基线） | 门禁参数：`max_ready_timeouts`（默认 0）、`regression`（退化阈值，默认 0.1）、`floor`（各指标的噪声下限，逐项覆盖默认值，见「门禁规则」）、`session_rss_main`（会话后窗口——记录带 `idle.session`——主进程内存的参考值，MiB：与空闲段同一环境实测、回收前 / 回收后两个窗口的较大者，按 `regression` 与 `floor.rss` 判退化） |
| `notes` | string[]（可选） | 备注 |

`idle`：

| 字段 | 单位 | 说明 |
| --- | --- | --- |
| `duration_s`、`samples` | s、个 | 采样时长与样本数 |
| `memory_metric` | string | `rss_*` 的口径：macOS `phys_footprint`（`proc_pid_rusage`，与活动监视器的「内存」一致，不含共享的系统框架页；没有系统 python3 时退回 `rss`）、Linux `rss`、Windows `working_set`；旧记录没有这个字段，按 `rss`。门禁只在口径相同时比较内存 |
| `rss_main`、`rss_main_max` | MiB | 主进程内存的平均值与峰值（口径见 `memory_metric`；macOS phys_footprint、Linux `VmRSS`、Windows `WorkingSet64`） |
| `rss_webview`、`rss_webview_max` | MiB \| null | WebView 进程内存之和的平均值与峰值（同一口径） |
| `webview_processes` | 个 \| null | 计入的 WebView 进程数（峰值） |
| `webview_attribution` | string \| null | WebView 的归属方式，见上表 |
| `cpu_avg`、`cpu_max` | % | 主进程 CPU（单核百分比）：采样窗口内累计 CPU 时间之差 ÷ 墙钟时间；`cpu_max` 为相邻两次采样之间的最大值 |
| `cpu_webview_avg`、`cpu_webview_max` | % \| null | WebView 进程合计的 CPU，按相邻两个样本里都存在的进程计算（进程增减不会让总和跳变） |
| `cpu_total_avg` | % \| null | 主进程 + WebView 合计，空闲 CPU 预算（< 0.1%）按它判定 |
| `wakeups`、`wakeups_webview` | 次/s \| null | 主进程、WebView 进程合计的唤醒频率；拿不到时为 null，原因写进 `notes` |
| `wakeups_method` | string \| null | `top-idlew`（macOS 无 sudo）、`powermetrics-idlew`（macOS sudo，只取 `idle_wakeups`，与 IDLEW 同口径——`intr_wakeups` 已包含它，不能相加）、`nr_wakeups`（Linux schedstats）、`voluntary_ctxt_switches`（Linux 近似）、`context-switches`（Windows 近似：线程上下文切换数，按线程求差——窗口里退出的线程不计，否则它此前的累计数会被一起减掉） |
| `notes` | string[] | 测量过程的说明 |
| `launch_env` | boolean \| null（可选） | macOS：本次启动的应用日志里 `preflight … launch_env=` 的值——经 LaunchServices / LaunchAgent 启动时 Info.plist 的 `LSEnvironment`（`MallocSpaceEfficient=1`）是否生效（record.mjs 从 `--app-log` 读出；读不到日志、旧版本没有这一项或其他平台为 null）。从终端直接执行时是 false（分配器设置不生效，主进程在 macOS 26 上约多 8 MiB） |
| `session` | object \| null（可选） | 会话后空闲（`--exercise`）的核对信息，record.mjs 由样本文件的 exercise 时刻（`meta exercise_start_ms` / `exercise_end_ms` / `settle_s`）与应用日志（`--app-log`，measure-*.sh 启动应用时自动给出）得出；时刻都是 exercise 结束后的秒数：`settle_s`、`window_s`（采样窗口 `[起, 止]`）、`traces` / `shown`（exercise 之后日志里的 perf 追踪数与其中面板显示过的次数——exercise 是否真的唤出了面板；读不到日志时为 null）、`recycles_s` / `unloads_s`（面板回收 / 空闲卸载的时刻）、`state`（`hot` 回收前：采样结束前没有回收 / 卸载；`recycled` / `unloaded` 回收后 / 卸载后：采样开始前已发生、采样期间没有；`mixed` 采样期间发生了；`unknown` 读不到日志）、`log`（日志文件名）。以下来自 debug 级别的面板事件（`advanced.log_level = debug`，测量设置会打开；没有 debug 行时为 null / unknown）：`panel`（`hidden` 采样窗口开始前 exercise 唤出的面板都已开始隐藏、窗口里没有再上屏；`visible` 测的是可见的面板；`unknown`）、`views`（上屏过的视图，按顺序）、`hides`（隐藏原因计数，`unlogged` 为 Rust 内部直接隐藏的）、`last_hide_s`（最后一次开始隐藏的时刻）、`clipboard`（应用记录到的剪贴板变化：新条目按类型计数——exercise 的使用画像；复制的内容与历史里已有的一条相同时应用只把它置顶、不新增，记为 `touched`——历史没有清空，例如 Windows 上用真实用户目录连续测量）；`log_lines`（采样窗口里写出的日志行数）。另外 `idle.notes` 记一句本次运行的更新检查（次数、启动后的时刻、是否落在采样窗口里） |
| `activity` | object（可选） | 采样窗口里的用户活动（record.mjs 从 debug 日志得出）：macOS 剪贴板轮询各档的时长（`tiers_s`）、复制次数（`copies`）、不能用 exercise 解释的活跃档时长（`user_active_s`）。有人在用时 smoke-gate 把 CPU / 唤醒判定降为告警 |

`latency`：`source`（`log` / `smoke`）、`metric`（固定为 `prepare_to_shown_ms`）、`count`（面板显示过的次数）、`p50`、`p95`、`max`、`mean`（ms，冒烟来源没有 `mean`）、`ready_timeouts`、`first_token_p50`、`first_token_p95`（只有日志来源才有）。分位数用最近秩法（排序后取第 ⌈p·n⌉ 个样本，不插值）。日志来源另有：

| 字段 | 说明 |
| --- | --- |
| `overlay_count`、`overlay_p50` / `p95` / `max` | 截图流程的 `overlay_ms`（触发 → 第一块屏的 Overlay 上屏，≤ 150 ms） |
| `pages.{warm,recycled,cold}` | 按 perf 行 `page=` 分组的 `count`、prepare→shown `p50` / `p95` / `max`、`ready_timeouts`：热页面、回收出来的新页面、空闲卸载之后重建（冷唤出，≤ 150 ms）。**冷唤出不计入主指标**（主指标的 100 ms 预算针对热 / 回收页面）；没有 `page=` 的旧日志全部计入主指标。`pages.cold.window` 是冷唤出的卡片上屏（prepare→window_ms，原生外壳），冷唤出的硬门禁按它判 |
| `split.{ready_to_window,ready_to_key,ready_to_shown}` | ready 之后的 Rust 侧拆分：`window_ms − ready_ms`（原生窗口上屏）、`key_ms − ready_ms`（面板成为 key，输入 / 剪贴板视图）、`shown_ms − ready_ms`；各有 `count`、`p50`、`p95`、`max` |
| `first_show` | 每次启动后的第一次面板唤出：`count`（启动次数）、`ready_timeouts`、`not_shown`、`max`（prepare→shown 最长）。每次启动取有 prepare 埋点、trace id **最小**的那条（行是追踪结束时才写的，真正的第一次可能晚于之后的追踪写出）。启动以 `startup stage=tray_ready` 行分界；整个文件一行 stage 都没有时（日志被截取过）才按 trace id 兜底——同一个 id 再次出现才算换了一次启动 |

`smoke.<build>`（由冒烟报告换算）：`ok`、`cold_start_ms`（= `startup.tray_ready_ms`，从内核创建进程算起）、`panel_prewarm_ms`、`rss_main`（= `rss_kb.end`，MiB）、`rss_main_peak`（`rss_kb` 三个值中的最大值）、`prepare_to_shown_p95`、`trigger_to_shown_p95`、`windows_end`、`memory_metric`（冒烟报告的同名字段）、`os_version`（录制环境的系统版本：本次报告与它的系统大版本不同时 RSS 退化比较只告警）、`rss_webview` / `rss_webview_peak` / `webview_attribution`（run-smoke.mjs `--webview` 采的 WebView 合计，MiB；只记录不比较）。

**会话后的空闲**：用过面板之后 Wisp 必须回到空闲（WebKit 进程同样要回到零唤醒）。`measure-macos.sh --exercise "<命令>" --settle <秒>` 在预热之后先执行一次命令（`bash -e -c`：任何一条命令失败都算失败；环境变量 `WISP_PID` / `WISP_BIN` / `WISP_LOG` 是被测进程、它的可执行文件与应用日志），静置 `--settle` 秒（默认 30）后再做空闲采样，记录的 `idle.notes` 注明「会话后空闲」、`idle.session` 给出核对信息。命令用 `node scripts/perf/exercise.mjs`：不要用「同一个 `--trigger` 再按一次就关闭」的裸命令——`--trigger` 的第二实例启动时可能让面板失焦先隐藏，下一次触发反而把面板打开，exercise 结束时面板还在屏幕上；exercise.mjs 每一步看日志、必要时再关一次，`smoke-gate --expect-state` 再核对一遍 `session.panel`。exercise.mjs 的使用画像是「灌了几十条剪贴板、打开过输入面板与剪贴板面板」，**没有翻译**（流式渲染与 markdown-lite 的内存不在其中，CLI 没有带文本的触发方式）——读数是这种用法下的值，翻译之后的状态要另测。macOS 的面板在最后一次隐藏 45 s 后回收（`windows::manager::panel_recycle_after`），同一个 exercise 之后的空闲因此有两种状态：**回收前**（刚用过，采样在回收之前结束）与**回收后**（回收出来的新页面，用过之后的常驻状态）；静置之后脚本还要约 3–4 s 读 WebKit 归属与唤醒计数的起始值，采样的实际窗口以 `session.window_s` 为准，「回收前」用 `--window-end` 把终点钉在回收之前。冒烟报告的 `post_session_idle`（窗口全部隐藏 / 关闭后静置 10 s 的主进程 CPU 与唤醒）由 smoke-gate 按空闲预算判定。

## 基线

`baselines/<os>.json` 与普通记录格式相同。CI 的「选基线」步骤在 macOS 上先找 `baselines/macos-<系统大版本>.json`（`sw_vers` 的大版本），没有时用 `baselines/<平台>.json`；文件不存在时门禁只检查不依赖基线的项（CI 的 macOS 冒烟会告警「RSS 退化门禁没有生效」）。目前只有 macOS 有基线：

| 文件 | 环境 | 内容 |
| --- | --- | --- |
| `macos.json` | 本机 macOS 26.4（Apple Silicon） | 空闲段是 600 s「从未显示」（fake-llm release .app 经 LaunchServices 启动、`launch_env=true`、屏幕解锁、无人操作）；冒烟段 `smoke.release` / `smoke.debug`；延迟段含冷唤出；`gate.session_rss_main = 15.4` |
| `macos-15.json` | GitHub macos-15 runner（macOS 15 / `VirtualMac2,1`，Apple 虚拟机：WebKit 没有 GPU 进程、软件合成） | 冒烟段 `smoke.release` 取自 runner 上的 release 冒烟；空闲段是会话后 ② 回收后窗口（runner 上没有「从未显示」的窗口），**带 `idle.session`（`state = recycled`）**——smoke-gate 据此知道它不是从未显示，从未显示的记录对它只比较主进程内存；`gate.session_rss_main = 13.3`（两个会话后窗口主进程的较大者）；延迟段置空（虚拟机的唤出延迟不代表真机）；`gate.max_ready_timeouts = 2`（虚拟机上 100 次连续触发里偶尔有一次撞上 40 ms 的 ready 兜底） |

两份基线各自的测量条件写在文件的 `notes` 里。冒烟口径随构建类型差别很大（debug 构建没有优化，RSS 与耗时都比 release 大得多），所以冒烟摘要按构建类型分开保存，门禁只和**同构建类型**的那一段比较：

- `smoke.release`：**CI 的 macOS 冒烟是 release 构建**，冒烟 RSS 的硬门禁和它比较，必须来自 release 冒烟；
- `smoke.debug`：本机 debug 构建的冒烟和它比较。

录制本机基线：

```bash
# 两种冒烟各跑一次（release 先构建：pnpm tauri build --no-bundle --features fake-llm --config src-tauri/tauri.no-updater.conf.json）
node scripts/perf/run-smoke.mjs --profile release --out target/smoke-release.json
node scripts/perf/run-smoke.mjs --profile debug --out target/smoke-debug.json
# 空闲测量 + 两份冒烟 → 基线（fake-llm 的 release .app：pnpm tauri build --bundles app --features fake-llm --config src-tauri/tauri.no-updater.conf.json）
bash scripts/perf/measure-macos.sh --app target/release/bundle/macos/Wisp.app --idle 600 \
  --smoke target/smoke-release.json --smoke target/smoke-debug.json --out scripts/perf/baselines/macos.json
# 录完用 prettier 格式化（scripts/ 下的 JSON 由 pnpm format:check 检查）
pnpm exec prettier --write scripts/perf/baselines/
```

`record.mjs` 只接受 `ok: true` 的冒烟报告，同一构建类型只能给一份。已有基线只缺冒烟段（或冒烟口径变了）时用 `record.mjs --update <基线> --smoke …` 只替换对应构建类型的冒烟段。基线存在但没有冒烟段（`smoke: null`）或缺少同构建类型的冒烟段时，本机门禁与 CI 的 macOS 冒烟**失败**。基线冒烟段的 `os_version` 与本次报告的系统大版本不同（本机 macOS 26 录的基线、CI 的 macos-15 runner）时，RSS 退化比较只告警（跨系统版本的 phys_footprint 不可比），宽松绝对上限照常硬判——所以 runner 需要自己的基线。

录制 / 更新 runner 基线（runner 换镜像，例如 macos-16 时重做一遍）：

1. 手动运行 CI（勾选 macOS 与冒烟），从运行的产物 `smoke-macos` 下载 `smoke-report.json`、`perf-post-hot.json`、`perf-post-recycled.json`（会话后记录的应用日志里应有 `launch_env=true`）；
2. 以下载的冒烟报告为冒烟段、按 runner 的系统大版本另存一份：
   `node scripts/perf/record.mjs --update scripts/perf/baselines/macos.json --smoke <smoke-report.json> --out scripts/perf/baselines/macos-<大版本>.json`
   （`--update` 只替换同构建类型——release——的冒烟段；`os_version` 随报告写入）；
3. 用 `perf-post-recycled.json` 的 `idle` 段换掉空闲段，**连同它的 `idle.session` 一起保留**，并改写顶层的 `os_version` / `machine`；`gate.session_rss_main` 取两份会话后记录主进程的较大者；延迟段置空；在 `notes` 里写明来源与条件；
4. 提交新基线后再手动运行 2 次，确认 runner 上的冒烟 RSS 与会话后主进程的波动在 10 % 以内（否则在基线的 `gate.regression` / `gate.floor` 里放宽并注明）。

要恢复从未显示的记录对 runner 基线的 WebView / CPU / 唤醒比较，在 runner 上录一段从未显示的空闲换掉空闲段并去掉 `idle.session`。

## 冒烟报告（`app::smoke` 的格式，`schema: 1`）

`WISP_SMOKE=1` 时应用执行冒烟流程（启动 → 首次翻译 → 100 次连续触发 → 开关设置窗口与 Overlay → 会话后空闲），把报告写到环境变量 `WISP_SMOKE_REPORT` 指定的路径（`run-smoke.mjs` 总会设置它，为绝对路径）；未设置时写到可执行文件所在 `target/` 目录下的 `smoke-report.json`。冒烟日志复制到报告旁边的同名 `.log`。报告写成功就以 0 退出（流程失败时 `ok: false`，原因在 `failures`），写不出报告才以非 0 退出。字段以 `apps/desktop/src-tauri/src/app/smoke.rs` 为准，正式定义见 `smoke-report.schema.json`（未列出的新字段允许存在），样例见 `examples/smoke-report.example.json`。

| 字段 | 说明 |
| --- | --- |
| `schema` | 数字 `1` |
| `ok`、`failures` | 整个流程是否通过；失败原因（英文） |
| `ts` | 写报告的时间（Unix 毫秒） |
| `os`、`os_version`、`arch`、`version` | 平台、系统版本、CPU 架构、应用版本 |
| `debug_build` | 是否为 debug 构建（门禁据此区分 release / debug 口径） |
| `fake_llm` | 是否设置了 `WISP_FAKE_LLM=1` |
| `duration_ms`、`log_file` | 冒烟总耗时；冒烟日志路径 |
| `startup.tray_ready_ms` | 冷启动到托盘就绪（ms，从内核创建进程算起，含 exec / dyld / 首次运行的 Gatekeeper 扫描）；`startup.pre_main_ms` 为其中到 `run()` 第一条语句的部分（取不到内核时刻时为 null，计时退回从 `run()` 起算）；`startup.stages` 为按时间排序的 `[{ name, ms }]` |
| `panel_prewarm_ms` | Panel 预热完成（ms，自进程启动；失败为 null） |
| `first_translate` | 首次 `TranslateText`：`shown_ms`、`first_token_ms`（自触发）、`finished`（fake 流结束）、`elapsed_ms`；没有追踪时整体为 null |
| `triggers` | 连续触发：`count`（100）、`shown`（面板显示的次数）、`ready_timeouts`（40 ms 的 ready 兜底）、`hangs`（3 s 内既没显示也没 ready 超时）、`prepare_to_shown_ms` / `trigger_to_shown_ms` 为 `{ p50, p95, max }`（最近秩法；没有样本时为 null） |
| `settings_window` | `opened`、`open_ms`、`closed`、`close_ms` |
| `windows` | `peak`（全程峰值）、`end`（结束时的窗口数）、`labels_end`（结束时的窗口 label） |
| `post_session_idle` | 会话后的空闲：窗口全部隐藏 / 关闭后静置 `seconds`（10 s）的主进程 `cpu_pct`（单核百分比）与 `wakeups_per_s`（只有 macOS，其他平台 null） |
| `memory_metric` | `rss_kb` 的口径：`phys_footprint`（macOS）、`working_set`（Windows）、`rss`（Linux）；旧报告没有这个字段，按 `rss` |
| `rss_kb` | 主进程内存（KiB，口径见 `memory_metric`；字段名沿用 schema 1）：`after_startup`、`after_triggers`、`end`；取不到时为 null |
| `webview` | **由 run-smoke.mjs `--webview` 写入**（默认关闭：采样本身会扰动同一次冒烟里被判定的耗时）：冒烟期间每秒采样的 WebView 进程内存合计（KiB，口径同 `memory_metric`）。`hot_kb` 为会话后空闲那 10 s（跳过前 2 s）的中位数，`peak_kb` 为全程峰值，`processes`、`attribution`（macOS `responsible` / `responsible-approx`，Linux `descendants`）、`samples` / `hot_samples`、`series`（逐秒 `[距写报告的秒数, KiB, 进程数]`）、`notes`。Windows 不采样（没有这一段）。**只记录、不判定**：冒烟在 20 s 内连续触发 100 次、开关设置窗口与 Overlay（与面板共用一个 WebContent 进程），会话后几秒里 JS 堆还没回收，读数随 GC 时机在 140–370 MiB 之间摆动，与会话后测量的「回收前 / 回收后」都不是同一口径 |

## 延迟日志格式（latency-report.mjs 的输入）

应用的 `PerfMarks::finish` 每完成一次追踪写一行 info 日志（macOS 为 `~/Library/Logs/wisp/wisp.log`），没有到达的阶段写 `-`，毫秒均从触发起算：

```
2026-10-10T08:00:00.000Z INFO  wisp_desktop_lib::app::perf: perf trace=3 origin=hotkey session=7 method=ax selection_ms=4 prepare_ms=6 ready_ms=12 shown_ms=27 first_token_ms=310 ready_timeout=false
```

也接受 `perf latency {LatencyMarks 的 JSON}` 行。`perf paste …`、`startup stage=…` 等其他行忽略；面板没有显示（`shown_ms=-`）的追踪不计入分位数，但计入 ready 超时次数。

## 门禁规则（smoke-gate.mjs）

**退化** = 超过基线 ×1.1（`gate.regression` 可调）**且**超过「基线 + 噪声下限」。很多空闲指标接近 0（基线 0.01% 的 CPU 多 0.002 个百分点就是 +20%，基线 0 次/s 的唤醒任何变化都是无穷大），只按比例判定会被测量噪声淹没，所以另加下限：

| 指标 | 噪声下限（`gate.floor` 的键） | 依据 |
| --- | --- | --- |
| RSS（主进程 / WebView / 冒烟） | 0.5 MiB（`rss`） | 分配器的页对齐与缓存抖动 |
| 空闲 CPU（主进程 / WebView） | 0.01 个百分点（`cpu`） | 600 s 内 60 ms 的 CPU 时间；ps 的计时单位是 10 ms |
| 空闲唤醒（主进程 / WebView） | 0.2 次/s（`wakeups`） | 10 分钟内 120 次偶发唤醒 |
| prepare→shown p95 | 2 ms（`latency`） | 计时与调度抖动 |
| 冷启动 | 20 ms（`cold_start`） | 进程创建与磁盘缓存抖动 |

下限按测量分辨率取值，并按同机重复测量的实际抖动确认；需要时在基线的 `gate.floor` 里逐项覆盖（例如 `"gate": { "floor": { "cpu": 0.005 } }`）。

**预算**（不依赖基线）：主进程 RSS ≤ 40 / 40 / 50 MiB（macOS / Windows / Linux），WebView ≤ 55 / 80 / 60 MiB（macOS 判「回收后」——用过之后的常驻状态；「回收前」的 45 s 按 100 MiB 防退化上限判；长时间空闲可由空闲卸载面板 WebView 降到接近 0，默认关闭），空闲 CPU（主进程 + WebView）< 0.1%，**空闲主进程唤醒 ≤ 5 / 0.2 / 0.2 次/s**（macOS 唯一的周期唤醒是 4 Hz 的剪贴板 changeCount 轮询，另留 1 次/s 给系统事件；Windows / X11 上 Wisp 自己没有周期唤醒源，留 0.2 次/s 容忍偶发唤醒——新增任何 ≥ 0.2 Hz 的定时器都会超出；Windows 上启动后 1–5 分钟 WebView2 转来的零星唤醒计入其中），冷启动 ≤ 800 ms，prepare→shown p95 ≤ 100 ms（不含冷唤出），冷唤出（`page=cold`）p95 ≤ 150 ms，ready→窗口上屏 / 成为 key 的 p95 ≤ 100 ms，启动后第一次唤出 prepare→shown ≤ 100 ms 且不允许 ready 超时，Overlay 上屏 p95 ≤ 150 ms；冒烟结束时主进程 RSS 另有一条宽松上限（release 80 / 80 / 100 MiB，debug 256 / 256 / 320 MiB）。

| 检查 | 本机门禁（默认） | `--ci`（GitHub runner） |
| --- | --- | --- |
| 报告格式、`ok`、`failures` 为空、fake LLM、启动与预热、首次翻译完成、100 次触发全部显示、0 卡死、ready 超时 ≤ 上限、设置窗口打开并关闭后没有残留窗口 | 失败 | 失败 |
| 启动后第一次唤出撞上 ready 超时（冒烟的首次翻译 / 连续触发第 1 次；性能记录的 `first_show`），不受 `gate.max_ready_timeouts` 放宽 | 失败 | 失败 |
| 性能记录的 `first_show` 里有没显示的（真实日志里可能是 NotReady 等正当原因） | 告警 | 告警 |
| 冒烟主进程 RSS 超宽松上限 | release 失败，debug 告警 | 同左 |
| 冒烟 RSS 退化（与同构建类型的基线比较；基线缺少该构建类型的冒烟段） | 失败 | macOS 失败，其他告警；基线冒烟段的系统大版本不同时两种模式都只告警 |
| 冒烟冷启动退化 | 告警 | 告警（runner 的时间抖动大） |
| prepare→shown p95 超预算 / 退化 | release 构建失败，debug 告警 | 告警 |
| 冷启动超预算 | release 构建失败，debug 告警 | 告警 |
| 空闲 RSS / CPU（主进程 + WebView 合计）/ 主进程唤醒超预算，或 RSS / CPU / 唤醒 / 延迟 p95 退化（性能记录） | 失败 | 告警；macOS release 记录且 WebView 精确归属（`responsible`，即经 LaunchServices 启动）时主进程内存的预算与退化比较失败 |
| 会话后窗口（记录带 `idle.session`）与基线比较：不比较 WebView / CPU / 唤醒（口径不同：基线的空闲段是「从未显示」，或是另一种状态的会话后窗口）；主进程按基线的 `gate.session_rss_main` 判退化（基线没有这一项时告警），基线（顶层 `os_version`）与记录的系统大版本不同时只告警 | 失败 | 同左（经 LaunchServices 启动的 macOS release 记录；其他告警） |
| 从未显示的记录（没有 `idle.session`）与基线比较，而基线的空闲段本身是会话后窗口（带 `idle.session`，例如 runner 基线）：只比较主进程内存（同一环境里从未显示的主进程不高于用过之后的，是偏宽的上限），WebView / CPU / 唤醒口径不同、不比较，摘要里有一条说明 | 主进程退化失败 | 同左（经 LaunchServices 启动的 macOS release 记录；其他告警） |
| 经 LaunchServices 启动的 macOS release 记录（`responsible`）启动环境没有生效（`idle.launch_env = false`：打出来的 .app 的 Info.plist 丢了 `LSEnvironment` 的 `MallocSpaceEfficient=1`，主进程会从约 13 MiB 回到约 21 MiB）；记录里没有 `launch_env` | 失败；没有这一项时 `--require-webview` 才失败、否则告警 | 同左 |
| 空闲 WebView 内存超预算（macOS 55 MiB 判回收后；回收前窗口按 100 MiB 防退化上限） | 失败 | 告警；`--webview-gate hard` 时失败（CI 的两个会话后窗口都传 hard） |
| 采样窗口里有人在用（`idle.activity`：复制，或 exercise 解释不了的剪贴板轮询活跃档） | 空闲 CPU / 唤醒的预算与退化比较改为告警 | 同左 |
| `--require-webview`（`--webview-gate hard` 隐含）：WebView 没测到、归属不可靠（macOS 不是 `responsible`）、进程数低于下限（macOS 3 / Linux 2 / Windows 3）、macOS 不是 release | 失败 | 失败 |
| `--expect-state <hot\|recycled\|unloaded>`：记录没有会话信息、读不到应用日志、exercise 没有唤出面板、采样窗口里面板还在屏幕上或无法核对（`session.panel` 不是 `hidden`：没有 debug 日志也算）、采样窗口的实际状态不是预期的那一种 | 失败 | 失败 |
| `--expect-views <视图,…>`：exercise 之后某个视图一次都没上屏（`session.views`；不规定次数——面板被重新打开时 exercise.mjs 会多关一次），或记录里没有视图信息 | 失败 | 失败 |
| 性能记录的延迟：冷唤出 p95、ready→窗口上屏 / 成为 key p95、第一次唤出耗时、Overlay 上屏 p95 超预算 | 失败 | 告警 |
| 虚拟机的记录：Apple 虚拟机（`machine` 以 `VirtualMac` 开头，即 macos-15 runner）与 Hyper-V 虚拟机（Windows / Linux 且 `machine` = `Virtual Machine`，即 GitHub 的 Windows / Linux runner）的 ready 超时次数、启动后第一次唤出的 ready 超时与延迟预算——共享 vCPU、没有 GPU、软件合成，唤出延迟不代表真机，以真机测量为准。内存、CPU、唤醒与测量有效性照常判定；Apple 虚拟机的 WebView 进程下限是 2（没有 GPU 进程） | 告警 | 告警 |
| 冒烟的 WebView 合计（run-smoke.mjs 采样） | 只记录 | 只记录 |
| WebView 进程没有测到（指标为 null；没加 `--require-webview` 时） | 失败 | 告警 |
| WebView 归属是近似的（`responsible-approx`） | 告警 | 告警 |

唤醒测量方法不同时跳过唤醒比较；主进程与 WebView 的 CPU、唤醒分别与基线比较。

## 面板回收对照

macOS 上用过的面板在最后一次隐藏 45 s 后回收（换成干净的页面）；Windows / Linux 默认不回收（`windows::manager::panel_recycle_after`）。手动运行 CI 时勾选 `panel_recycle`（默认关闭，需要同时保留 `linux_windows`），作业 `panel-recycle`，Linux（ubuntu-22.04）与 Windows（windows-latest）各跑一次 `scripts/ci/panel-recycle-linux.sh` / `panel-recycle-windows.ps1`，为这项取舍提供数据：

- release + fake-llm 构建，测量设置（更新检查关闭、日志级别 debug）；先热身启动一次（20 s，不采样：二进制、共享库、WebView2 用户数据目录与磁盘缓存先热起来，三段的起点一致），再依次测「从未显示」「用过之后（当前默认，不回收）」「用过并回收（`WISP_PANEL_RECYCLE_SECS=45`）」三段 120 s 空闲（显式预热：Linux 60 s、Windows 40 s）；
- 每段都过 `smoke-gate --require-webview`，后两段再加 `--expect-state` / `--expect-views`；**一段无效不中断**，三段都测完、各段判定写进运行摘要后才以非零退出；对照表（`compare-records.mjs`）写进摘要，记录与应用日志作为产物上传（Linux 约 25 分钟、Windows 约 50 分钟）；
- Linux 在 `dbus-run-session` + Xvfb 里跑，`scripts/ci/sni-watcher.py` 在会话总线上提供最小的 StatusNotifierWatcher——否则 Wisp 走「托盘不可用 → 打开设置窗口」的退化路径，设置窗口的 WebView 会混进测量（日志里出现这条退化时那一段判为无效）；exercise 用 xclip 灌剪贴板；
- Windows 用 runner 的真实用户目录（见下「Windows 的数据目录」），测量设置写进 `%APPDATA%\wisp\settings.json`；exercise 用 powershell.exe（STA）灌剪贴板；两端的灌入都是 `--seed-optional`（失败只告警，记录的 `session.clipboard` 写明实际条数）；
- runner 是共享 vCPU、没有 GPU 的虚拟机：内存与唤醒能说明「用过之后有没有回到从未显示的水平」，绝对值不能当真机基线。

判定方法：「用过之后」的 WebView CPU / 唤醒（`cpu_webview_avg`、`wakeups_webview`）回到与「从未显示」相同的水平（≈ 0）、WebView 内存没有比从未显示时多出一截，就不需要回收；否则在对应平台打开 `panel_recycle_after`（「用过并回收」那一段就是打开之后的样子），并复测回收后的首次唤出。

真机上的测量命令（与作业相同的三段，可按需加长 `--idle`；在仓库根目录运行）：

```bash
# Linux（X11 桌面会话里；release 构建：pnpm tauri build --no-bundle --features fake-llm --config src-tauri/tauri.no-updater.conf.json；灌剪贴板要 xclip）
EX='node scripts/perf/exercise.mjs --views input,clipboard --texts 30 --images 3'
bash scripts/perf/measure-linux.sh --app target/release/wisp-desktop --idle 300 --out target/panel-recycle-never.json
bash scripts/perf/measure-linux.sh --app target/release/wisp-desktop --exercise "$EX" --settle 30 --idle 300 --out target/panel-recycle-used.json
WISP_PANEL_RECYCLE_SECS=45 bash scripts/perf/measure-linux.sh --app target/release/wisp-desktop --exercise "$EX" --settle 60 --idle 300 --out target/panel-recycle-recycled.json
node scripts/perf/compare-records.mjs 从未显示=target/panel-recycle-never.json 用过之后=target/panel-recycle-used.json 用过并回收=target/panel-recycle-recycled.json
```

隔离 HOME（默认）时 XDG_CONFIG / DATA / CACHE / STATE 一并指到临时目录、并写入测量设置，X11 授权沿用真实的 `~/.Xauthority`（`XAUTHORITY` 未设置时）；GTK / fontconfig / Mesa 的用户缓存是冷的，所以预热默认 60 s（`--warmup` 可改），让首次构建缓存的 CPU 与内存在采样之前结束。`--exercise` 与应用用同一套环境。

```powershell
# Windows（PowerShell 7；会读写当前用户真实的配置与数据，见下）。exercise.mjs 读 debug 级别的面板事件：先写入测量设置
# （改写真实的 settings.json：更新检查关闭、日志级别 debug），测完在设置里改回
node scripts/perf/exercise.mjs --write-settings "$env:APPDATA\wisp\settings.json"
$ex = 'node scripts/perf/exercise.mjs --views input,clipboard --texts 30 --images 3'
./scripts/perf/measure-windows.ps1 -App target/release/wisp-desktop.exe -UseRealProfile -Warmup 40 -Idle 300 -Out target/panel-recycle-never.json
./scripts/perf/measure-windows.ps1 -App target/release/wisp-desktop.exe -UseRealProfile -Warmup 40 -Exercise $ex -Settle 30 -Idle 300 -Out target/panel-recycle-used.json
$env:WISP_PANEL_RECYCLE_SECS = '45'; ./scripts/perf/measure-windows.ps1 -App target/release/wisp-desktop.exe -UseRealProfile -Warmup 40 -Exercise $ex -Settle 60 -Idle 300 -Out target/panel-recycle-recycled.json; Remove-Item Env:WISP_PANEL_RECYCLE_SECS
```

### Windows 的数据目录

Windows 上**没有隔离的数据目录**：Wisp 用 `dirs::config_dir()` / `dirs::data_local_dir()`（dirs-sys 在 Windows 上调 `SHGetKnownFolderPath`）找配置、数据与日志目录，WebView2 的用户数据目录同样走已知文件夹，都不读 `%APPDATA%` / `%LOCALAPPDATA%` 环境变量。所以 `measure-windows.ps1 -App` 启动的 Wisp 读写的就是当前用户真实的 `settings.json`、`clipboard.db`、WebView2 配置与日志，测量结果也受这些设置影响；脚本要求显式加 `-UseRealProfile` 确认，只在专用的测量账户、虚拟机或 CI runner 上用。
