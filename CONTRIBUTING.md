# 参与贡献

简体中文 · [English](CONTRIBUTING.en.md)

感谢你愿意改进 Wisp。本文说明如何报告问题、提交改动，以及代码与提交的约定。环境搭建、构建与测试命令见[开发指南](docs/development.md)，代码结构见[架构](docs/architecture.md)。

## 报告问题

在 [GitHub Issues](https://github.com/yzlltyyh/wisp/issues/new/choose) 选择对应的模板：

- **问题报告**：写明 Wisp 版本（「设置 → 关于」）、操作系统与版本（Linux 请注明桌面环境与 X11 / Wayland 会话）、复现步骤、期望与实际的结果。能附上**诊断包**最好：「设置 → 关于 → 诊断信息 → 导出诊断包」，导出的 zip 只含脱敏后的日志与设置，不含 API Key 与任何原文、译文、剪贴板内容（内容清单见[使用说明](docs/user-guide.md#诊断导出与反馈)）。排查前可以先把日志级别临时调到「调试」再复现一次。
- **功能建议**：描述你想解决的场景，而不只是想要的界面。提交之前请先看看 README 的[不做什么](README.md#不做什么)——Wisp 刻意只做截图 OCR、划词翻译与剪贴板三件事。

**不要在 issue、截图或日志里贴出 API Key**；截图里出现 Key、个人信息或剪贴板内容时请先遮掉。**安全问题请不要公开提交**，按 [SECURITY.md](SECURITY.md) 私下报告。参与本项目的每个人都应遵守[行为准则](CODE_OF_CONDUCT.md)。

## 提交改动

1. 较大的改动（新功能、行为变化、新增依赖）请先开一个 issue 讨论方向，避免白费功夫。
2. Fork 仓库，从 `main` 拉出功能分支，一个 PR 只做一件事。
3. 按下文的约定写代码与测试，提交前在本机跑一遍[测试与检查](docs/development.md#测试与检查)。
4. 发起 PR，按模板说明改了什么、为什么、怎么验证的；涉及界面的改动附上前后截图或录屏，并注明在哪些平台上验证过。
5. CI 的 `lint` 与 `test` 必须通过，维护者审阅后合并。

改动影响用户可见的行为时，同步更新相应的文档：[README](README.md) 与 [README.en.md](README.en.md)、[使用说明](docs/user-guide.md)、[模型服务](docs/providers.md)。

## 代码约定

### 通用

- **注释用简体中文，标识符用英文**。注释说明「为什么」：设计取舍、平台的怪癖、不明显的约束；不要复述代码在做什么。
- **issue、PR 描述与提交信息用中文或英文均可**。
- **用户可见的文字一律走 i18n**：界面文案在 `apps/desktop/src/locales/zh-CN/` 与 `en-US/` 同时添加；托盘与系统通知的文案在 `apps/desktop/src-tauri/src/app/i18n.rs`。`pnpm i18n:check` 会拦下硬编码的界面文字。
- **平台差异收敛在平台层**：`#[cfg(target_os)]` 只写在 `crates/wisp-platform`（取词、粘贴、焦点等系统能力）、`crates/wisp-capture`（截图）、`apps/desktop/src-tauri/src/windows/native/`（窗口）以及 `lib.rs` 的插件注册里，其他位置通过 trait 调用（见[架构 · 总览](docs/architecture.md#总览)）。
- **隐私**：日志只记录元数据（耗时、状态码、模型名等），绝不写入原文、译文、剪贴板内容、截图、API Key、请求头的值或 URL 查询参数。
- **依赖**：新增依赖需要说明理由，并锁定精确版本；改了 lockfile 后运行 `pnpm run licenses` 重新生成第三方许可清单并一起提交。

### Rust

- `cargo fmt` 与 `cargo clippy --workspace --all-targets -- -D warnings` 必须干净。
- 公开项写中文文档注释；`unsafe` 块前写 `// SAFETY:` 注释说明为什么成立。
- 错误类型用 `thiserror`，日志用 `log` 宏。
- 可能阻塞的系统调用（无障碍接口、剪贴板、X11）要有超时，不要在 async 上下文里直接阻塞，放进 `spawn_blocking` 或工作线程。
- `crates/*` 不得依赖 Tauri，保证可以脱离 WebView 测试。
- 跨 IPC 的类型放在 `crates/wisp-types`，同步修改 `apps/desktop/src/shared/ipc/types.ts` 并重新生成黄金样例（`node scripts/gen-fixtures.mjs`）。

### 前端

- TypeScript 严格模式；`pnpm lint`（oxlint）与 `pnpm format:check`（Prettier）必须通过。
- `@tauri-apps/*` 只能在 `apps/desktop/src/shared/ipc/` 里引用，其他代码通过那里的封装调用。
- 动效的时长与曲线只引用 `shared/motion/` 的令牌或 `--dur-*` / `--ease-*` CSS 变量，颜色只用设计令牌；`pnpm check:contract` 会检查。
- 交互组件要能用键盘操作，带 `aria-*` 属性与可见的焦点环；「减少动效」打开时只保留简短的淡入淡出。

## 提交信息

提交标题遵循 [Conventional Commits](https://www.conventionalcommits.org/zh-hans/)：

```text
<type>(<scope>): <描述>
```

- **type**：`feat`（新功能）、`fix`（修复）、`perf`（性能）、`refactor`（重构）、`revert`、`docs`、`test`、`ci`、`build`、`chore`、`style`。
- **scope**（可选）：受影响的模块，例如 `panel`、`overlay`、`clipboard`、`llm`、`macos`、`windows`、`linux`。
- **描述**：一句话说明改了什么，中文或英文均可，不加句号。需要解释原因时写在正文里。
- **破坏性变更**：在类型后加 `!`（`feat(settings)!: …`），或在正文写 `BREAKING CHANGE: …`。

`feat`、`fix`、`perf`、`refactor`、`revert` 会被 git-cliff 收进下一版更新日志的草稿，请从用户的角度描述；其余类型以及范围为 `ci`、`build`、`deps`、`e2e`、`qa` 的提交不会进入。

## 测试要求

- 新功能附带测试；修复缺陷附带能复现该缺陷的回归测试。
- Rust 单元测试写在模块内的 `#[cfg(test)]`，集成测试放在 crate 的 `tests/`；需要系统权限或真实 GUI 的测试标 `#[ignore = "原因"]`。
- 前端组件与逻辑用 Vitest，放在被测代码旁的 `__tests__/`；jsdom 复现不了的交互（真实焦点、组合按键、逐帧布局）写成 Playwright 浏览器级用例，放在被测代码旁的 `__e2e__/`（`pnpm -C apps/desktop exec playwright test` 运行全部）。
- 平台相关的改动请注明在哪些系统上手工验证过。只能在本机验证一个平台时，至少用 `pnpm check:cross` 确认另外两个平台能通过类型检查。

## README 中英同步

`README.md`（简体中文）与 `README.en.md`（English）一节对一节、内容同等完整。改动其中一份时请在同一个 PR 里同步另一份，尤其是版本号与安装包文件名、系统要求、默认快捷键、设置项与按钮的名称、已知限制。英文版里的界面文案照抄应用里的实际字符串（`apps/desktop/src/locales/en-US/`、`apps/desktop/src-tauri/src/app/i18n.rs`）。中文 README 的标题锚点（如 `#安装`）被 Release 说明与使用说明引用，请不要改名。

## 许可

提交到本仓库的贡献按 [MIT 许可](LICENSE) 发布。
