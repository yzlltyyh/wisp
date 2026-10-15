# AGENTS.md

写给在本仓库工作的 AI 编码助手的约定。面向人的同一套规则见 [CONTRIBUTING.md](CONTRIBUTING.md)，更完整的说明见 [docs/development.md](docs/development.md) 与 [docs/architecture.md](docs/architecture.md)。

## 项目

Wisp 是常驻后台的轻量桌面工具：划词翻译、截图 OCR、剪贴板历史。Tauri 2（Rust）+ React 19 + Motion + Tailwind CSS 4，支持 macOS（Apple Silicon）、Windows（x64）、Linux（x64，完整功能需要 X11）。所有 AI 能力都通过用户自己配置的模型服务完成。

```text
apps/desktop/src/          前端：entries/（四个页面入口）、panel/、views/、settings/、onboarding/、pinned/、overlay/、shared/、locales/
apps/desktop/src-tauri/    Rust 应用（wisp-desktop）：app/、orchestrator/、windows/、hotkeys/、clipboard_svc/、settings/、ipc/、tray/、updater/
crates/wisp-types          跨 crate / IPC / 持久化的共享类型
crates/wisp-llm            四种协议的流式 LLM 客户端
crates/wisp-prompts        提示词模板（templates/）、风格、语言检测、后处理
crates/wisp-clipboard      剪贴板历史存储（SQLite + FTS5）
crates/wisp-capture        屏幕捕获、裁剪、编码
crates/wisp-platform       操作系统集成层（macOS / Windows / Linux 实现）
vendor/                    打了补丁的上游 crate（wry、tao、tray-icon、notify）
scripts/                   检查、版本号、性能、截图、发布脚本
```

## 命令

```bash
pnpm install --frozen-lockfile
pnpm dev                              # 开发运行；WISP_FAKE_LLM=1 时使用内置假模型，不需要 Key
pnpm -C apps/desktop dev:mock         # 只跑前端 + 浏览器 mock（不编译 Rust）
pnpm build:debug                      # debug 安装包

cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
pnpm lint && pnpm format:check && pnpm typecheck && pnpm test
pnpm i18n:check && pnpm check:contract && pnpm check:size
```

按需：`pnpm check:cross`（在 macOS 上对 Windows / Linux 目标做类型检查）、`node scripts/gen-fixtures.mjs`（改了 `wisp-types` 后重新生成前端黄金样例）、`pnpm licenses`（改了依赖后）、`node scripts/release/self-test.mjs`（改了发布脚本或 `CHANGELOG.md` 后）、`cargo test -p wisp-llm --features fake`、`cargo test -p wisp-prompts --features eval`、`pnpm -C apps/desktop exec playwright test`（浏览器级用例，配置在 `apps/desktop/playwright.config.ts`）。

完成改动前至少运行与改动相关的格式化、lint、类型检查与测试，并在结果里如实说明哪些检查跑过、哪些没有跑。

## 代码约定

- **注释用简体中文，标识符用英文。** 注释说明为什么这样做，不复述代码；注释应自成一体，把理由直接写出来，不用编号或文档章节号代替解释（引用公开的上游 issue / PR 链接可以）。
- **平台差异只放在 `crates/wisp-platform`、`crates/wisp-capture` 与 `apps/desktop/src-tauri/src/windows/native/`**（以及 `lib.rs` 的插件注册）。其他位置不写 `#[cfg(target_os)]`，通过 trait 调用。desktop 的 `windows` 模块与 `windows` crate 同名：引用自己的模块写 `crate::windows::…`，引用 crate 写 `::windows::…`。
- **`crates/*` 不依赖 Tauri。** crate 之间只能横向依赖 `wisp-types`（`wisp-prompts` 的 `eval` feature 例外）。
- **IPC**：命令、事件、字段、枚举值都用 snake_case；命令返回 `Result<T, IpcError>`；`ipc/` 只做参数校验与分发，字符串、浮点数与集合参数在命令体第一步校验。新增命令要同时改 `ipc/mod.rs` 的 `commands!` 与前端 `shared/ipc/commands.ts`。事件名引用 `wisp_types` 里的常量。
- **类型同步**：改 `crates/wisp-types` 时同步 `apps/desktop/src/shared/ipc/types.ts`，并重新生成 `shared/ipc/fixtures/`（不要手改生成的文件）。
- **前端**：`@tauri-apps/*` 只能在 `apps/desktop/src/shared/ipc/` 里引用；动效时长与曲线只用 `shared/motion/` 的令牌或 `--dur-*` / `--ease-*`；颜色只用设计令牌；覆盖层（`overlay/`）是原生 TS，不引入 React / Motion。
- **文案**：用户可见的字符串一律经 i18n，`zh-CN` 与 `en-US` 两份 locales 同时添加；Rust 显示的托盘 / 通知文案在 `src-tauri/src/app/i18n.rs`。
- **隐私**：不得把原文、译文、剪贴板内容、截图、API Key、请求头的值或 URL 查询参数写进日志、错误信息或诊断包。API Key 只能由 Rust 读取，不提供把 Key 返回前端的接口。
- **性能**：不引入常驻的周期定时器或轮询；新资源（连接、线程、钩子）按需创建、用完释放。面板首屏的五个视图保持静态导入；`pnpm check:size` 检查体积与入口依赖图。
- **Rust 细节**：`unsafe` 前写 `// SAFETY:`；错误类型用 `thiserror`；可能阻塞的系统调用放进 `spawn_blocking` 或工作线程并设超时。
- **依赖**：不随意新增依赖；需要时锁定精确版本，并运行 `pnpm licenses`。
- **应用标识不可更改**：identifier `io.github.yzlltyyh.wisp`、`tauri.conf.json` 里的更新器公钥与端点都不要改。

## 测试

- Rust 单元测试写在模块内 `#[cfg(test)]`，集成测试放 crate 的 `tests/`；需要系统权限或真实 GUI 的测试标 `#[ignore = "原因"]`。
- 前端测试放在被测代码旁的 `__tests__/`（Vitest），浏览器级用例放在 `__e2e__/`（Playwright）。
- `wisp-llm` 的协议测试使用录制的 SSE 样本，不需要真实 Key；不要在测试或示例里写入真实的 Key。

## 提交

提交标题遵循 Conventional Commits：`<type>(<scope>): <描述>`（`feat`、`fix`、`perf`、`refactor`、`docs`、`test`、`ci`、`build`、`chore` 等）。`feat` / `fix` / `perf` / `refactor` 会进入更新日志草稿，请从用户角度描述。只在被要求时提交或推送。

## 文档

用户可见的行为变化同步更新 `README.md` 与 `README.en.md`（两份一节对一节）、`docs/user-guide.md`、`docs/providers.md`。`CHANGELOG.md` 的版本段落会原样成为 Release 说明与应用内的更新内容，不写 Markdown 链接与 HTML，末尾附 `### English summary`。
