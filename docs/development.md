# 开发指南

本文说明如何在本机构建、运行、测试与调试 Wisp，以及发版流程。参与贡献的约定（issue、PR、代码风格、提交信息）见 [CONTRIBUTING](../CONTRIBUTING.md)，代码结构见[架构](architecture.md)。

## 目录

- [环境要求](#环境要求)
- [获取代码](#获取代码)
- [运行与构建](#运行与构建)
- [macOS 开发签名](#macos-开发签名)
- [测试与检查](#测试与检查)
- [调试](#调试)
- [目录约定](#目录约定)
- [CI](#ci)
- [性能测量](#性能测量)
- [README 截图](#readme-截图)
- [发布流程](#发布流程)
- [更新器密钥与代码签名](#更新器密钥与代码签名)

## 环境要求

| 工具 | 版本 | 说明 |
| --- | --- | --- |
| Rust | 1.96.1 | 由 `rust-toolchain.toml` 锁定，rustup 会自动安装。`Cargo.toml` 里的 `rust-version = "1.95"` 是声明的最低版本（clippy 按它判断可用的 API），CI 只用锁定的 1.96.1 构建 |
| Node | ≥ 24.15，或 22.x 中 ≥ 22.22.2 | 见 `package.json` 的 `engines`（`^22.22.2 \|\| >=24.15`），CI 用 Node 24 |
| pnpm | ≥ 12.3.4 | 见 `package.json` 的 `packageManager`（12.3.4）；`corepack enable` 后自动使用这个版本 |
| macOS | 14 或更新，Xcode Command Line Tools | 应用的最低系统版本是 macOS 14 |
| Windows | Visual Studio 生成工具（C++ 桌面开发）、WebView2 运行时 | Windows 11 自带 WebView2 |
| Linux | WebKitGTK 4.1、AppIndicator 等系统库 | Ubuntu / Debian 上执行 `bash scripts/ci/linux-deps.sh`（与 CI 使用同一份列表） |

Linux 上的完整功能需要 X11 会话；在 Wayland 会话里可以编译和运行，但取词、全局快捷键、截图与剪贴板监听不可用。

## 获取代码

```bash
git clone https://github.com/yzlltyyh/wisp.git
cd wisp
corepack enable
pnpm install --frozen-lockfile
```

Rust 依赖在第一次 `cargo` 或 `pnpm dev` 时下载。`vendor/` 下是通过 `[patch.crates-io]` 替换的几个上游 crate（wry、tao、tray-icon、notify、oar-ocr / oar-ocr-core），已经在仓库里，不需要额外操作；各补丁的内容与原因见 [vendor/README.md](../vendor/README.md)。

## 运行与构建

| 命令 | 作用 |
| --- | --- |
| `pnpm dev` | 开发运行（`tauri dev`，带 `fake-llm` feature）。前端热更新，Rust 改动自动重新编译 |
| `WISP_FAKE_LLM=1 pnpm dev` | 同上，并改用内置的假模型：不需要 Key，固定文本、可配置延迟，适合调界面与流程。离线翻译、离线识别与 PaddleOCR 识别也改用假引擎，编排器把离线模型当作已下载，不下载、不启动推理进程 |
| `pnpm -C apps/desktop dev:mock` | 只启动前端与浏览器 mock，在普通浏览器里开发界面，不需要编译 Rust（见下文） |
| `pnpm build:debug` | 打一个 debug 安装包（带 `fake-llm`，关闭更新器产物，不需要更新器私钥） |
| `pnpm tauri build --config src-tauri/tauri.no-updater.conf.json` | 本机打 release 安装包，不生成更新器签名 |

产物在 `target/<debug|release>/bundle/`。正式的 release 安装包只由 CI 的发布工作流生成（见[发布流程](#发布流程)）。

**浏览器 mock**：`dev:mock` 启动后打开 `http://localhost:1420/panel.html?scenario=translate.streaming-learn` 之类的地址。常用 URL 参数：`label`（`panel` / `settings` / `onboarding` / `pinned` / `clipfloat` / `overlay`）、`scenario`（场景 id，定义在 `apps/desktop/src/shared/ipc/mock/scenarios/`，id 前缀即文件名）、`theme=dark`、`os=windows|linux|macos`、`lang=en-US`、`reduced=1`（减少动效）、`material=solid`、`frame=native|css`。

**已经装有 Wisp 时**：Wisp 是单实例应用，本机正在运行安装版时，用相同 identifier 启动的开发构建会被转交给已运行的实例，自己直接退出。开发前请先退出安装版。确实需要并行运行时，可以给开发构建换一个 identifier（例如 `pnpm dev --config '{"identifier":"io.github.yzlltyyh.wisp.dev"}'`），但配置、数据与日志目录不随 identifier 变化，两个实例会共用同一份配置与剪贴板数据库，也会争抢同样的全局快捷键。

**命令行触发**：`wisp-desktop --trigger <动作>` 让正在运行的实例执行一个动作（`translate_selection`、`translate_input`、`ocr`、`ocr_translate`、`clipboard`），调试快捷键之外的路径时很方便。

## macOS 开发签名

每台 Mac 第一次开发前执行一次：

```bash
scripts/dev-sign-macos.sh --init
```

它在登录钥匙串里创建一张自签代码签名证书「Wisp Dev」并设为信任（系统会要求输入登录密码）。之后：

```bash
scripts/dev-sign-macos.sh --dev                        # 代替 pnpm dev：每次启动 wisp-desktop 前自动用该证书签名
pnpm build:debug && scripts/dev-sign-macos.sh          # debug 安装包：构建后重签
scripts/dev-sign-macos.sh --check                      # 查看证书与产物的签名状态
```

**原因**：macOS 的隐私授权（辅助功能、屏幕录制、输入监控）绑定在应用代码签名的 designated requirement 上。默认的 ad-hoc 签名没有证书，designated requirement 就是二进制的哈希，每次重新编译都会变，于是之前给的授权全部失效，每次都要重新去系统设置里勾选。用固定证书签名后，授权在重新编译后仍然有效。首次签名时钥匙串可能询问 codesign 能否使用私钥，选「始终允许」。

注意：

- `--dev` 只对这一次运行生效。不要把 `CARGO_TARGET_*_RUNNER` 写进 shell 配置，那会影响本机所有 Rust 项目。
- 从终端用 `pnpm dev` 启动的进程，授权会记在终端应用名下。要验证 Wisp 自己的授权流程，用 `pnpm build:debug` 打出 `.app`、重签后从访达或 `open` 启动。
- 这个脚本重签时不开 hardened runtime，也不带 entitlements，所以开发构建不受动态库校验的影响。发布构建由 tauri 的打包器签名，带 hardened runtime 与 `src-tauri/Entitlements.plist`（离线模型的 ORT 工作进程要加载下载的 ONNX Runtime 动态库，见[架构 · 离线模型与推理进程](architecture.md#离线模型与推理进程)）；可以用 `codesign -d --entitlements - <Wisp.app>` 确认。

## 测试与检查

提交 PR 之前在本机运行：

```bash
# Rust
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked

# 前端
pnpm lint && pnpm format:check && pnpm typecheck && pnpm test

# 一致性检查
pnpm i18n:check        # 两种语言的文案 key 与占位符一致；Rust 下发的 key 都有文案；没有硬编码的界面文字
pnpm check:contract    # 前后端命令清单一致；@tauri-apps/* 只在 shared/ipc 引用；动效没有魔法数；crates/* 不依赖 Tauri，彼此只横向依赖 wisp-types / wisp-text
pnpm check:size        # 生产构建前端，检查体积预算与入口依赖图
node scripts/bump.mjs --check   # 各处版本号一致
```

视改动范围另外运行：

| 改动 | 命令 |
| --- | --- |
| 依赖（`Cargo.lock`、`pnpm-lock.yaml`） | `pnpm run licenses` 重新生成第三方许可清单并一起提交；CI 用 `node scripts/licenses.mjs --check` 核对 |
| `cfg(windows)` / `cfg(linux)` 代码（在 macOS 上开发时） | `pnpm check:cross`（`scripts/cross-check.sh`：对 Windows / Linux 目标做类型检查与 clippy，不链接） |
| `wisp-types` 的类型或其他生成前端样例的 Rust 代码 | `node scripts/gen-fixtures.mjs` 重写 `apps/desktop/src/shared/ipc/fixtures/` 下的样例；只核对用 `--check` |
| 发布脚本、`cliff.toml`、`CHANGELOG.md` | `node scripts/release/self-test.mjs` |
| GitHub 工作流或 shell 脚本 | `actionlint`、`shellcheck` |
| 假模型、提示词评测 | `cargo test -p wisp-llm --features fake --locked`、`cargo test -p wisp-prompts --features eval --locked` |
| 文本规则（`crates/wisp-text`） | `cargo test -p wisp-text --locked` 与 `cargo test -p wisp-local --locked`：后者把 PP-OCRv6 的黄金样例与一组构造的版面拼成文本，与 `crates/wisp-local/tests/fixtures/assemble/` 逐字比对。规则有意改变时先审阅差异，确认后用 `UPDATE_GOLDEN=1 cargo test -p wisp-local --test assemble_golden` 重写 |
| 离线模型（`crates/wisp-local`、`models_svc/`、`wisp-platform` 的 `system` / `pressure` / `child`） | `cargo test -p wisp-local --features ort,fake --locked` 与 `cargo test -p wisp-platform --locked`（含 `child_guard`）；改了 Windows / Linux 的平台代码时加 `pnpm check:cross`。对真实运行时、模型与 PaddleOCR 服务的测试见下文[离线模型与 PaddleOCR 的真机测试](#离线模型与-paddleocr-的真机测试) |
| 多设备同步（`crates/wisp-sync`） | `cargo test -p wisp-sync --locked`：加解密与篡改检测、投影往返、合并性质（proptest）、多设备收敛模拟（内存远端）与 WebDAV 客户端（wiremock），都不需要真实账号。对真实服务器的测试默认忽略，用环境变量提供账号：`WISP_SYNC_TEST_URL=https://… WISP_SYNC_TEST_USER=… WISP_SYNC_TEST_PASS=… cargo test -p wisp-sync --locked --test live_webdav --test live_engine -- --ignored`。`live_webdav` 是传输层冒烟，只在地址下 `wisp/` 里的随机子目录中读写；`live_engine` 让两台模拟设备经真实服务器走完连接、加入、同步、删除与清空、清除服务器数据，在地址下单独的 `e2e-<随机 id>/` 目录里进行，不碰真实的同步数据。两者结束时（包括失败时）都会删除自己建的目录。不要把真实的地址与账号写进仓库 |
| 界面交互（焦点、键盘、逐帧布局） | 浏览器级用例：`pnpm -C apps/desktop exec playwright install webkit chromium`（第一次），然后 `pnpm -C apps/desktop exec playwright test`（配置在 `apps/desktop/playwright.config.ts`，运行 `src/**/__e2e__/*.e2e.ts`） |
| Linux 平台层 | `xvfb-run -a cargo test -p wisp-platform --test smoke_linux -- --ignored --test-threads=1`（需要 `bash scripts/ci/linux-deps.sh smoke` 安装的依赖） |

测试的放置约定：Rust 单元测试写在模块内的 `#[cfg(test)]`，集成测试放在 crate 的 `tests/`；需要系统权限或真实 GUI 的测试标 `#[ignore = "原因"]`。前端测试放在被测代码旁的 `__tests__/`，浏览器级用例放在 `__e2e__/`。

### 离线模型与 PaddleOCR 的真机测试

`crates/wisp-local` 的默认测试不需要网络、真实模型与令牌：下载器（wiremock 与脚本化服务器：续传、换源、校验失败、区间提取等）、解包的安全检查、存储布局、设备适配、llama-server 的参数与输出解析、帧协议与工作进程、文本拼装（黄金输出）、OTSL 表格、版面后处理、PaddleOCR 两种接口（wiremock）；`wisp-platform` 的 `child_guard`（`harness = false`，测试二进制自己充当守护进程）覆盖守护进程、Job Object 与孤儿清扫。对真实运行时、模型与服务的测试都标了 `#[ignore]`，需要手动运行；**模型与运行时不提交进仓库**。

**准备素材**：在一个被 `.gitignore` 忽略的目录里（建议 `artifacts/local-models/`）按下面的布局放好官方原件。下载地址与 SHA-256 以 `crates/wisp-local/src/catalog.rs` 为准，下载后逐个核对哈希（`shasum -a 256 <文件>`）。

```text
artifacts/local-models/
  raw/
    runtime/llama-b11669-bin-macos-arm64.tar.gz       运行时的发布包（解包测试用）
    runtime/onnxruntime-osx-arm64-1.28.3.tgz
    PP-OCRv6_tiny_det_onnx/inference.onnx             以模型仓库名为目录，放仓库里的原文件
    PP-OCRv6_tiny_rec_onnx/inference.onnx、inference.yml
    PP-OCRv6_small_det_onnx/…、PP-OCRv6_small_rec_onnx/…（同上）
    PP-DocLayoutV3_onnx/inference.onnx
    PaddleOCR-VL-1.6-GGUF/PaddleOCR-VL-1.6-GGUF.gguf、PaddleOCR-VL-1.6-GGUF-mmproj.gguf
    Index-Translate-2B-GGUF/Index-Translate-2B.Q4_K_M.gguf
    Index-Translate-9B-GGUF/Index-Translate-9B.Q4_K_M.gguf
  rt/
    llama-b11669/   把 llama.cpp 的发布包解在这里，得到 rt/llama-b11669/llama-b11669/llama-server
    ort-1.28.3/     把 ONNX Runtime 的发布包解在这里，得到 rt/ort-1.28.3/onnxruntime-osx-arm64-1.28.3/lib/…
```

这些真机测试按 macOS（Apple Silicon）的发布包编写；Linux 用对应的发布包，解包后的布局相同。测试在素材目录下用硬链接临时搭出 Wisp 的 `models/` 布局（不占额外空间），结束时删除；只打印耗时、内存与计数，不打印识别与翻译的内容。

| 变量 | 作用 |
| --- | --- |
| `WISP_LOCAL_TEST_DIR` | 素材目录的绝对路径；真机测试都从这里读，临时目录也建在这里 |
| `WISP_LAYOUT_TEST_IMAGE` | 版面检测与 VL 流水线用的图片（可选，默认 `docs/images/ocr-light.png`） |
| `WISP_VL_TEST_URL`、`WISP_VL_TEST_KEY` | VL 流水线连接的 llama-server 地址（例如 `http://127.0.0.1:8111`）与它的 API Key（可选）；`WISP_VL_TEST_PRINT=1` 时打印识别结果 |
| `WISP_TEST_DESKTOP_EXE` | 桌面端集成测试用的 Wisp 可执行文件（可选，默认取同一 target 目录下的 `wisp-desktop`） |
| `WISP_PADDLE_TEST_TOKEN`、`WISP_PADDLE_TEST_URL` | PaddleOCR 云端测试的访问令牌与服务地址（地址可选，默认 AI Studio 的任务接口）。令牌只从环境变量读，不要写进任何文件 |
| `WISP_MODELS_DIR` | 让开发构建的 Wisp 把离线模型放到别处，不写进真实的数据目录；只在 debug 构建或 `WISP_SMOKE=1` 时生效，发布版忽略 |

```bash
export WISP_LOCAL_TEST_DIR=$PWD/artifacts/local-models

# 守护进程、强制结束父进程与孤儿清扫，对真实的 llama-server（Index-Translate 2B）
cargo test -p wisp-platform --test child_guard -- --ignored
# llama-server：2B / 9B / VL 的冷热启动、一次翻译、常驻内存、API Key 生效、加载中取消、停止后无进程、--list-devices 的输出
cargo test -p wisp-local --test llama_live -- --ignored --nocapture --test-threads=1
# 官方发布包的解包
cargo test -p wisp-local --test runtime_extract -- --ignored --nocapture
# 真实网络的下载：PP-OCRv6 Tiny 连同运行时的完整安装、大文件续传、Hugging Face 源、Windows ONNX Runtime 的区间提取
cargo test -p wisp-local --test download_live -- --ignored --nocapture --test-threads=1

# PP-OCRv6 与官方产线输出的对比、延迟与内存；版面检测。先编译测试用的工作进程
cargo build -p wisp-local --features ort --example ort_worker
cargo test -p wisp-local --features ort --test ocr_live -- --ignored --nocapture ppocr_matches
cargo test -p wisp-local --features ort --test ocr_live -- --ignored --nocapture layout_detection

# VL 文档流水线：先在另一个终端启动加载 PaddleOCR-VL 的 llama-server，测完结束它
LLAMA_API_KEY=<随机字符串> $WISP_LOCAL_TEST_DIR/rt/llama-b11669/llama-b11669/llama-server \
  -m $WISP_LOCAL_TEST_DIR/raw/PaddleOCR-VL-1.6-GGUF/PaddleOCR-VL-1.6-GGUF.gguf \
  --mmproj $WISP_LOCAL_TEST_DIR/raw/PaddleOCR-VL-1.6-GGUF/PaddleOCR-VL-1.6-GGUF-mmproj.gguf \
  --host 127.0.0.1 --port 8111 -c 8192 -np 1 --no-webui --jinja --offline
WISP_VL_TEST_URL=http://127.0.0.1:8111 WISP_VL_TEST_KEY=<同一个字符串> \
  cargo test -p wisp-local --features ort --test ocr_live -- --ignored --nocapture vl_pipeline

# 桌面端：经模型服务跑翻译与 PP-OCRv6 的冷热路径、闲置释放与退出（不启动 GUI，测得的数字写到素材目录的 live-metrics.json）
cargo build -p wisp-desktop --bin wisp-desktop
cargo test -p wisp-desktop --lib -- --ignored live_translation_and_ppocr

# PaddleOCR 云端：三个预置模型各识别一张截图、连接测试（真实网络与令牌）
WISP_PADDLE_TEST_TOKEN=<令牌> cargo test -p wisp-local --test cloud_live -- --ignored --nocapture

# 结束后确认没有残留的推理进程
pgrep -fl 'llama-server|wisp-worker|wisp-guard'
```

在开发构建里手动验证下载与设置页时，用 `WISP_MODELS_DIR=<临时目录>` 把模型放到别处，验证完删除这个目录。


## 调试

- **日志**：日志文件在 macOS `~/Library/Logs/wisp/wisp.log`、Windows `%LOCALAPPDATA%\wisp\logs\wisp.log`、Linux `~/.local/state/wisp/logs/wisp.log`；debug 构建同时输出到终端。级别在「设置 → 通用 → 高级 → 日志级别」里调，改动立即生效。日志按设计不包含任何正文与 Key。
- **开发者模式**：「设置 → 通用 → 高级 → 开发者模式」在面板底部显示各阶段的延迟与帧率。
- **WebView 开发者工具**：debug 构建可以打开 WebView 的开发者工具（Tauri 在 debug 构建中默认启用）。
- **假模型**：`WISP_FAKE_LLM=1` 只对编入了 `fake-llm` feature 的构建生效（`pnpm dev`、`pnpm build:debug`），发布版忽略它。
- **冒烟模式**：`WISP_SMOKE=1 WISP_FAKE_LLM=1` 启动 fake-llm 构建，会在隔离的临时目录里自动走一遍启动、翻译、连续唤出面板、打开设置的流程，写出 `target/smoke-report.json` 后退出，不碰真实配置与数据。`node scripts/perf/run-smoke.mjs` 封装了这一步。
- **配置与数据**：配置文件可以直接编辑，保存后自动重新加载。需要一份干净的状态时，先退出 Wisp，再备份并移走配置目录与数据目录（位置见[架构 · 数据与配置](architecture.md#数据与配置)）。
- **跨平台差异**：窗口层的问题先确认属于哪种帧模式（macOS 为 Native，Windows / Linux 为 Css）；浏览器 mock 可以用 `frame=native|css` 模拟两种模式的前端行为。

### 开发用环境变量

除特别注明外，下面的变量只在 debug 构建或带 `fake-llm` feature 的构建（`pnpm dev`、`pnpm build:debug`）里生效，发布版忽略它们。

| 变量 | 作用 |
| --- | --- |
| `WISP_FAKE_LLM=1` | 改用内置的假模型，不需要 Key |
| `WISP_SMOKE=1` | 冒烟模式（配合 `WISP_FAKE_LLM=1`，见上文）；`WISP_SMOKE_REPORT=<路径>` 改写报告位置，默认 `target/smoke-report.json` |
| `WISP_PANEL_RECYCLE_SECS=<秒>` | 覆盖面板回收的延迟（macOS 默认隐藏 45 s 后回收，Windows / Linux 默认不回收）；`0` 或 `off` 关闭回收 |
| `WISP_PANEL_THROTTLING=throttle\|suspend\|disabled` | 覆盖面板 WebView 的后台节流策略（发布版固定为 `disabled`），用于对比空闲唤醒 |
| `WISP_DEV_UPDATER=1` | debug 构建默认不自动检查更新；设置后照常检查，用来调试更新流程 |
| `WISP_DEV_AUTOSTART=1` | debug 构建默认不登记开机启动；设置后照常登记 |
| `WISP_WEBVIEW_DEVKEYS=1` | Windows：不屏蔽 WebView2 的浏览器快捷键（F12 开发者工具、F5 重新加载等）；默认屏蔽，避免重新加载打断面板会话 |
| `WISP_X11_HOTKEYS=plugin` | Linux X11：全局快捷键改由 tauri-plugin-global-shortcut 注册（默认用平台层的 `XGrabKey`）。**发布版同样生效**，是某个桌面环境上快捷键失灵时不用换版本的退路 |
| `WISP_WINDOW_LAB=1`、`WISP_FRAME_LAB=<模式>` | 窗口层实验台与面板帧模式实验台（见下文） |
| `WISP_MODELS_DIR=<目录>` | 离线模型与推理组件改放到这个目录（默认是数据目录下的 `models/`）；只在 debug 构建或冒烟模式下生效 |
| `WISP_E2E_PORT=<端口>` | 浏览器级用例 preview 的端口，默认 1479 |
| `WISP_EVAL_KEY_<ID>` | 提示词评测运行器使用的 API Key（见下文） |

### 开发者工具

- **组件展示页**：浏览器 mock 里打开 `panel.html?scenario=shell.gallery`，列出设计令牌、全部组件的各种状态与动效 preset，可以切换 `theme`、`os`。源码在 `apps/desktop/src/panel/shell/gallery/`。
- **视觉稿**：`panel.html?scenario=design.<视图>`（如 `design.translate-done`、`design.ocr-done`、`design.clipboard-list`）与 `design.settings-models`、`design.onboarding-permissions` 是各视图、设置与引导窗口的静态视觉基准，实现改动后可以对照。源码在 `apps/desktop/src/panel/shell/design/`。
- **窗口层实验台**：先 `pnpm -C apps/desktop build`，再以 `WISP_WINDOW_LAB=1 WISP_WINDOW_LAB_EXIT=1` 运行带 `tauri/custom-protocol` 的 debug 构建（`target/debug/wisp-desktop`）。它不经业务流程，直接用真实的窗口层走一遍面板、固定窗口、覆盖层、设置窗口与面板回收等路径，每一步输出一行 `window_lab step=… ok|fail` 日志，并检查窗口泄漏；配置与数据放在隔离的临时目录。另有只做测量的模式（内存、冷唤出、延迟等），全部开关见 `apps/desktop/src-tauri/src/windows/window_lab.rs` 的模块注释。
- **帧模式实验台**：`WISP_FRAME_LAB=native|native-css-fill|css` 让面板反复进入、流式增长、收缩、退出，用于录屏核对与逐帧耗时度量，参数见 `apps/desktop/src-tauri/src/windows/frame_lab.rs` 的模块注释。
- **提示词评测**：`cargo run -p wisp-prompts --features eval --bin wisp-eval -- --provider-json <文件> --styles all` 用指定的服务把内置样本按各风格翻译一遍，结果写到 `target/eval-runs/`（`--out` 可改），自动断言汇总通过率，其余项留给人工检查；Key 从 `WISP_EVAL_KEY_<ID>` 读取（ID 为服务 id 的大写，非字母数字替换为 `_`）。用法见 `crates/wisp-prompts/src/eval/mod.rs` 的模块注释。支持图片的模型另跑 OCR 截图样本：截图（PNG）放在 `--ocr-dir`（默认 `<out>/ocr-samples/`），缺少的样本跳过，文件名与期望见同目录 `samples.rs` 的 `OCR_SAMPLES`；仓库里不放截图。段落模式要四张：`chat.png`（几条聊天消息，夹着昵称与时间）、`pdf.png`（两段以上的英文 PDF 正文，行尾有断词连字符）、`list.png`（带列表或一组设置项的设置页、网页）、`mixed.png`（中英混排的多行正文）。

## 目录约定

| 放什么 | 放在哪里 |
| --- | --- |
| 与操作系统打交道的代码 | `crates/wisp-platform`（trait 与三端实现）、`crates/wisp-capture`（截图）；窗口相关的放 `apps/desktop/src-tauri/src/windows/native/`。`#[cfg(target_os)]` 只写在这三处与 `lib.rs` 的插件注册里（见[架构 · 总览](architecture.md#总览)） |
| 跨 crate、跨 IPC 或要持久化的类型 | `crates/wisp-types`，同时更新 `apps/desktop/src/shared/ipc/types.ts` 与黄金样例 |
| 只看文字本身、要在多个 crate 里保持一致的规则（空格、断词、列表项……） | `crates/wisp-text`：不依赖任何 crate，只放确定性的纯文本规则；版面与几何留在调用方 |
| 新的 Tauri 命令 | `apps/desktop/src-tauri/src/ipc/cmd_*.rs` 与 `ipc/mod.rs` 的 `commands!`，前端封装在 `apps/desktop/src/shared/ipc/commands.ts`；参数在命令体第一步校验 |
| 提示词 | `crates/wisp-prompts/templates/` |
| 离线模型的目录（文件、大小、SHA-256、下载地址、运行时版本） | `crates/wisp-local/src/catalog.rs`；换文件或换版本时把 `CATALOG_REVISION` 加一，并更新设置页与文档里的大小 |
| 界面文案 | `apps/desktop/src/locales/zh-CN/` 与 `en-US/` 同时添加；托盘与系统通知的文案在 `apps/desktop/src-tauri/src/app/i18n.rs` |
| 动效时长与曲线 | `apps/desktop/src/shared/motion/` 的令牌，不写字面数值 |
| 颜色与其他视觉常量 | `apps/desktop/src/shared/styles/` 的设计令牌 |
| 生成的文件 | `apps/desktop/src/generated/`（许可清单）、`apps/desktop/src/shared/ipc/fixtures/`（黄金样例）：由脚本生成，不要手改 |
| 第三方 crate 的补丁 | `vendor/<crate>/`；改动、原因与升级时的处理写进 [vendor/README.md](../vendor/README.md)，源码里以 `Wisp patch` 注释标出 |

## CI

`.github/workflows/ci.yml`：

| 触发 | 作业 |
| --- | --- |
| push 到 `main`、PR | `lint`（Linux：actionlint、shellcheck、版本号、oxlint、prettier、tsc、i18n、rustfmt、两组 feature 的 clippy、契约检查、许可清单）；`test (linux)`、`test (windows)`（clippy、cargo test、vitest，Linux 另跑 feature 测试、X11 平台层用例、前端体积与脚本自检）；汇总作业 `test` |
| 改了前端 | 另跑 `e2e`：Playwright 官方容器里 WebKit 与 Chromium 两种引擎的浏览器级用例 |
| 只改文档（`docs/**`、`*.md`、`LICENSE`） | 只运行判定与汇总作业，其余跳过 |
| 手动触发（Actions → ci → Run workflow） | 可选 macOS 的 clippy / test、三端冒烟（`WISP_SMOKE` + 与基线比较的门禁）、视觉 QA 截图矩阵、Windows / Linux 的面板回收对照（`panel_recycle`，见 [scripts/perf/README.md](../scripts/perf/README.md)） |

PR 合并前需要 `lint` 与 `test` 通过。同一分支上的新提交会取消正在运行的旧运行。

## 性能测量

脚本在 `scripts/perf/`，详细用法见 [scripts/perf/README.md](../scripts/perf/README.md)。

```bash
# 冒烟：fake-llm 构建 → 启动 → 100 次连续唤出 → 写报告 → 与基线比较
pnpm tauri build --debug --no-bundle --features fake-llm --config src-tauri/tauri.no-updater.conf.json
node scripts/perf/run-smoke.mjs --profile debug

# macOS 长时间空闲：经 LaunchServices 启动 release 包，在隔离的 HOME 里预热后测量 CPU、唤醒与内存
pnpm perf:mac --app target/release/bundle/macos/Wisp.app --idle 600
```

测量 macOS 上 WebView 进程的内存时，Wisp 必须经 LaunchServices（访达或 `open`）启动，否则 WebKit 进程无法准确归属。本机已经在运行安装版时，测量用的构建要换一个 identifier（见[运行与构建](#运行与构建)）。

**README 与 CHANGELOG 里的性能数字**来自一台 Apple Silicon Mac（Mac17,2，macOS 26.4），测量条件与读数记录在 `scripts/perf/baselines/macos.json`（`notes` 字段写明了构建方式与测量条件）：

- **空闲**：release 构建经 LaunchServices 启动（`pnpm perf:mac --app …`），隔离的 HOME、更新检查关闭，屏幕解锁、无人操作。面板从未显示时预热 30 s、测 600 s；「用过之后」先由 `--exercise` 唤出输入与剪贴板面板各一次，再分别测面板回收之前与回收之后的空闲。读数：主进程约 13 MiB（用过之后约 15 MiB），WebView 进程合计约 49 MiB（回收前约 84 MiB、回收后约 53–54 MiB），空闲 CPU 合计约 0.01 %。
- **延迟**：冒烟模式的 100 次连续唤出，从触发到面板上屏的 p95 约 30 ms；启动后的第一次唤出与回收后重新加载的面板都在 100 ms 以内。
- 数字取整，并取多次运行中偏大的读数。其他机型与系统版本上会有差异；Windows 与 Linux 目前没有同样条件下的公开数字。

具体参数（`--warmup`、`--settle`、`--idle`、`--exercise`）与如何录制新的基线见 [scripts/perf/README.md](../scripts/perf/README.md)。

## README 截图

`docs/images/` 的截图由脚本按清单生成，不要手工截取或裁切：

```bash
pnpm -C apps/desktop exec playwright install webkit   # 第一次
pnpm qa:shots --readme                                # 重新生成全部 README 截图
pnpm qa:shots --readme --filter clipboard             # 只重做部分文件；--out <目录> 先输出到别处对比
```

清单在 `scripts/qa/readme-shots.ts`（场景、主题、视口、裁切框、输出宽度）。截图基于浏览器 mock 的场景，用 WebKit 以 2× 渲染后缩小；脚本最后核对 `README.md` 引用的图片都由清单生成。界面有可见变化时重新生成，并对照 README 里的 alt 文字确认内容仍然相符。

## 发布流程

版本号遵循 SemVer。发布由 `.github/workflows/release.yml` 完成：推送 `v*` tag 后，在 GitHub 上建一份**草稿** Release，三个平台分别构建、核对、上传，最后生成 `latest.json` 并把每个文件的 SHA-256 写进说明。草稿经人工核对后手动发布，已安装的客户端才会看到新版本。

### 1. 发版前检查

```bash
node scripts/bump.mjs --check
node scripts/release/self-test.mjs                          # 发布脚本、更新器配置、CHANGELOG 段落格式
node scripts/release/prepare.mjs --dry-run --tag v<版本>     # 预览 Release 说明（不访问 GitHub）
```

再加上[测试与检查](#测试与检查)的全套命令，并确认：

- 主分支的 CI 是绿的；
- `README.md` 与 `README.en.md` 的版本号、安装包文件名与系统要求一致，两份内容同步；
- 界面有变化时 README 截图已经更新。

改动了更新器、打包配置或升级了 `tauri` / `tauri-plugin-updater` 之后，在 macOS（Apple Silicon）上跑一次本机更新演练 `scripts/release/updater-rehearsal.sh`：它构建两个版本，用本机 HTTP 服务器代替 GitHub Release，让应用自己完成检查、下载、验签、安装与重启，并验证篡改包、错误密钥、改写版本号三种拒绝路径。演练全程使用一次性测试密钥、隔离的 HOME 与一次性钥匙串，不碰真实的配置与数据；加 `--interactive` 时由人在界面上走一遍更新流程。

### 2. 版本号与更新日志

```bash
node scripts/bump.mjs 0.2.0      # 同步根与 apps/desktop 的 package.json、Cargo.toml、Cargo.lock、tauri.conf.json
npx --yes git-cliff@2.14.2 --unreleased --tag v0.2.0 --prepend CHANGELOG.md
```

git-cliff 根据上一个正式版以来的提交在 `CHANGELOG.md` 顶部生成新段落（`feat` / `fix` / `perf` / `refactor` / `revert` 进入，其余类型不进入）。**人工审定这一段**：按功能概括、合并同类项、面向用户描述，不写 Markdown 链接与 HTML——这一段会原样成为 Release 说明与应用内的「更新内容」，应用内不渲染链接。段落末尾附一个 `### English summary` 小节，用几条英文概括本版本面向用户的变化（Release 说明与应用内的「更新内容」会原样包含两种语言）。`self-test.mjs` 会检查当前版本有对应段落且格式合规。

把版本号与 CHANGELOG 放在同一个提交里：`chore(release): v0.2.0`。

### 3. 打 tag

```bash
git tag -a v0.2.0 -m "Wisp 0.2.0"
git push origin main v0.2.0
```

带 `-` 的 tag（如 `v0.2.0-beta.1`）标为预发布；CHANGELOG 里没有它的段落时，由 git-cliff 生成说明。

### 4. 发布工作流

```text
prepare（ubuntu）
  发布脚本自检（含更新器配置守卫）→ 版本号与 tag 校验 → 生成 Release 说明 → 建或复用草稿
build（并行，单个平台失败不影响其他平台）
  macOS  macos-15        aarch64-apple-darwin      dmg、app.tar.gz(.sig)
  Windows windows-latest x86_64-pc-windows-msvc    NSIS exe(.sig)
  Linux  ubuntu-22.04    x86_64-unknown-linux-gnu  deb、rpm、AppImage(.sig)
  构建 → 签名信息、启动环境、体积与 Linux 依赖核对 → 上传到草稿
finalize（ubuntu）
  核对产物清单 → 用 tauri.conf.json 的公钥校验每个 .sig → 生成 latest.json → 把 SHA-256 写进说明
```

- prepare 会拒绝：更新器配置不是正式的那一份、tag 与版本号不一致、tag 指向的不是本次构建的提交、该版本已经发布过、正式 tag 只选了部分平台。
- 某个平台失败时，修好后在运行页面 **Re-run failed jobs**，产物仍上传到同一份草稿，finalize 跟着重跑。
- **手动触发**（Actions → release → Run workflow）：tag 留空生成测试草稿 `v<版本>-ci.<运行号>`（不创建 git tag，验证后删除）；可以只构建一个平台，也可以模拟缺少更新器私钥或 macOS 证书来演练降级分支。

### 5. 核对草稿

- tag 是 `v<版本>`，Actions 全绿；finalize 的摘要里产物齐全、签名全部有效；
- 草稿资源恰好是：`Wisp_<v>_aarch64.dmg`、`Wisp_<v>_aarch64.app.tar.gz(.sig)`、`Wisp_<v>_x64-setup.exe(.sig)`、`Wisp_<v>_amd64.deb(.sig)`、`Wisp-<v>-1.x86_64.rpm(.sig)`、`Wisp_<v>_amd64.AppImage(.sig)`、`latest.json`；
- Release 说明的变更段落、下载表、系统要求、首次打开说明、SHA-256 表都在；预发布标记与 tag 一致；
- 至少在一个平台上下载安装、启动，并核对下载文件的 SHA-256；
- 变更说明与主分支的 `CHANGELOG.md` 一致（草稿正文取自 **tag 上**的 CHANGELOG）。

**编辑草稿说明**：变更说明在 `<!-- wisp:notes:start -->` 与 `<!-- wisp:notes:end -->` 之间，可以直接在草稿页修改；两对标记（notes 与 checksums）都要保留。`latest.json` 的 `notes` 是 finalize 运行时从草稿里读取的，finalize 之后再改说明，要对 finalize 单独 Re-run job 一次。人工改过说明之后需要重跑时用 Re-run failed jobs；Re-run all jobs 会重新生成标记之外的部分。改过的措辞记得同步回 `CHANGELOG.md`。

### 6. 发布与发布之后

在草稿页点「Publish release」，或：

```bash
gh release edit v<版本> -R yzlltyyh/wisp --draft=false --latest
```

发布之后确认更新端点 `https://github.com/yzlltyyh/wisp/releases/latest/download/latest.json` 不带凭据即可访问，内容与草稿里的 `latest.json` 相同；已安装的旧版本在启动约 30 秒后（或托盘「检查更新」）能看到新版本。

已发布的版本不能重新发布，有问题时递增版本号。草稿从未发布、需要在新提交上重新出包时：删除草稿与 tag（远程与本地），在新的提交上重新打 annotated tag 并推送。

### 常见问题

| 现象 | 处理 |
| --- | --- |
| `更新器配置不是正式的那一份` | 端点、公钥或 no-updater 配置被改过（常见于把本机演练的 `--config` 写进了配置文件）。改回去，**不要更换公钥或端点**——已安装的客户端只认现在的这一对 |
| `tag … 与版本号 … 不一致` | 先 `node scripts/bump.mjs <版本>` 并提交，删掉旧 tag 后重打 |
| self-test 失败 | 多半是 CHANGELOG 缺当前版本的段落，或段落里有链接 / HTML |
| 配置了私钥但口令为空 | prepare 直接失败；补上 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` 后重跑 |
| 上传前核对失败、一个文件都没有上传 | 按日志里的 `::error::` 看缺哪个文件：有私钥时每个更新器产物都要有 `.sig` |
| finalize：签名的 key id 与配置的公钥不一致 | Secrets 里的私钥与 `tauri.conf.json` 的公钥不是一对；把正确的私钥放回 Secrets，不要换公钥 |
| 草稿的 tag 变成 `untagged-…` | `gh api -X PATCH repos/yzlltyyh/wisp/releases/<id> -f tag_name=v<版本>` 改回后再发布 |
| Linux 依赖核对失败 | 补 `tauri.conf.json` 的 `bundle.linux.deb.depends` / `rpm.depends`，或同步 `scripts/ci/linux-deps.sh` |
| 安装包体积超限 | 预算见 `scripts/check-size.mjs`（dmg ≤ 12 MB、NSIS ≤ 10 MB、deb / rpm ≤ 15 MB、AppImage ≤ 90 MB） |

## 更新器密钥与代码签名

**应用标识发布后不可修改**：productName `Wisp`、identifier `io.github.yzlltyyh.wisp`、仓库 `yzlltyyh/wisp`。改动 identifier 会让用户已有的系统授权、钥匙串条目、开机自启项与自动更新全部失效；改仓库名时要同步 `tauri.conf.json` 里的更新端点。

### Secrets

| Secret | 作用 | 缺失时 |
| --- | --- | --- |
| `TAURI_SIGNING_PRIVATE_KEY`、`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | 更新器签名（`.sig` 与 `latest.json`） | 仍生成全部安装包，但没有 `.sig` / `latest.json`，Release 说明顶部有警告，已安装的版本不会自动更新到这一版 |
| `MACOS_SELF_SIGNED_P12_BASE64`、`MACOS_SELF_SIGNED_P12_PASSWORD`（正式发布必需） | macOS 用固定的自签证书签名，更新后系统授权保持有效 | 正式发布（推 tag，或手动触发时填了 tag）在 prepare 里直接失败；tag 留空的测试草稿用 ad-hoc 签名（也可以勾选「模拟未配置 macOS 自签证书」演练这条分支） |

### 更新器密钥

更新器密钥对**只生成一次**，之后不能更换：已安装的客户端只认 `apps/desktop/src-tauri/tauri.conf.json` 里 `plugins.updater.pubkey` 的公钥，self-test 也会核对公钥的 key id 与更新端点。生成方法（只在项目初始化时做过一次，供参考）：

```bash
pnpm tauri signer generate -w <仓库之外的路径>/wisp.key   # 交互式设置口令
```

公钥写进 `plugins.updater.pubkey`；私钥文件的内容与口令分别存为上表的两个 secret（例如 `gh secret set TAURI_SIGNING_PRIVATE_KEY < <私钥文件>`，口令用 `gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD` 交互输入）。

- 私钥与口令只放在仓库之外：不要打印到终端日志、提交进仓库或贴进 issue。
- **正式私钥只在 CI 里使用**。本机需要签名演练时，用 `pnpm tauri signer generate` 生成一次性测试密钥对，并用 `--config` 同时覆盖公钥与端点（`updater-rehearsal.sh` 就是这样做的），不要用正式私钥在流水线之外签出任何包。
- 本机给 `tauri build` / `tauri bundle` 传私钥时，把私钥文件的**绝对路径**放进 `TAURI_SIGNING_PRIVATE_KEY`（`TAURI_SIGNING_PRIVATE_KEY_PATH` 对它们无效），口令只放在那一条命令的环境变量里。
- 私钥丢失只能换新公钥，并发布一个需要用户手动下载安装的版本。

**构建步骤里的私钥暴露面**：tauri-cli 在打包时签名，所以发布工作流的「构建」一步必须带着私钥；这一步同时运行 Vite 及其插件与全部依赖的 `build.rs`、过程宏，私钥对它们可见。风险靠锁定版本来降低（`pnpm install --frozen-lockfile`、cargo `--locked`）；`GITHUB_TOKEN` 不进入构建进程，只在「上传到草稿」一步出现。**引入新依赖时请留意这一点。**

### macOS 证书

发布用的自签证书（CN「Wisp Release」，有效期 20 年）已经生成并配置成上表的两个 secret，由维护者在仓库之外保管、备份。它与更新器密钥一样**只生成一次、不能更换**：系统授权（辅助功能、屏幕录制、输入监控）与钥匙串条目都绑定签名身份，换证书或哪一版退回 ad-hoc，所有用户更新后都要重新授权——所以正式发布缺少它时工作流直接失败。证书的生成方法（只做过一次，供参考）：用系统自带的 `/usr/bin/openssl`（LibreSSL）生成 RSA 3072、扩展用途为 `codeSigning` 的自签证书（配置与 `scripts/dev-sign-macos.sh` 的 `create_p12` 相同，只是证书名与有效期不同），`openssl pkcs12 -export` 导出 p12，再分别存为 secret：`base64 -i <p12> | gh secret set MACOS_SELF_SIGNED_P12_BASE64`、`gh secret set MACOS_SELF_SIGNED_P12_PASSWORD < <口令文件>`。

工作流里不要写 `APPLE_CERTIFICATE*` / `APPLE_SIGNING_IDENTITY: ${{ secrets.… }}` 这样的映射：未配置的 secret 会展开成空字符串，tauri-cli 会用空串覆盖配置里的 ad-hoc 签名。将来接入 Apple Developer ID 签名与公证、Windows 代码签名时，接入点见 `release.yml` 里「构建」一步的注释。

**改用 Apple 开发者证书签名时**，离线模型依赖主程序的 `com.apple.security.cs.disable-library-validation`（hardened runtime 下加载下载来的、ad-hoc 签名的 ONNX Runtime 动态库，原因见[架构 · 离线模型与推理进程](architecture.md#离线模型与推理进程)），换签名方式之后逐项核对：

1. `codesign -d --entitlements - <Wisp.app>` 的输出里仍有这一项；如果改由工作流自己调用 `codesign`，要带上 `--options runtime` 与 `--entitlements apps/desktop/src-tauri/Entitlements.plist`；
2. 公证通过（`xcrun notarytool submit … --wait` 的结果为 Accepted），`spctl --assess --type execute -v <Wisp.app>` 通过；
3. 在一台新机器上（没有开发证书、没有下载过模型）实测一次：下载 PP-OCRv6 并完成一次离线识别（确认 ORT 工作进程能加载下载的动态库），再下载 Index-Translate 2B 完成一次离线翻译（确认 llama-server 能启动），结束后没有残留的推理进程。
