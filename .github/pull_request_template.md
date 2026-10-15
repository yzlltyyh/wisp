## 改了什么 / What

<!-- 简要说明改动内容与原因；相关 issue 写 Closes #123 / Briefly describe the change and why; link issues with Closes #123 -->

## 怎么验证的 / How was it tested

<!-- 运行了哪些测试与检查；在哪些平台上手工验证过 / Tests and checks you ran; platforms you verified on -->

- [ ] macOS
- [ ] Windows
- [ ] Linux（X11 / Wayland）

## 截图或录屏 / Screenshots

<!-- 涉及界面时附上改动前后的对比 / For UI changes, include before / after -->

## 检查清单 / Checklist

- [ ] 提交标题遵循 Conventional Commits / Commit titles follow Conventional Commits
- [ ] [测试与检查](https://github.com/yzlltyyh/wisp/blob/main/docs/development.md#测试与检查)里的全部命令都已通过（Rust 的 fmt / clippy / test，`pnpm lint`、`format:check`、`typecheck`、`test`、`i18n:check`、`check:contract`、`check:size`，`node scripts/bump.mjs --check`），并按改动范围跑了表格里的附加检查 / All commands in [Tests and checks](https://github.com/yzlltyyh/wisp/blob/main/docs/development.md#测试与检查) pass, plus the extra checks for the areas you changed
- [ ] 改了依赖时运行了 `pnpm licenses` 并提交了重新生成的许可清单 / Ran `pnpm licenses` and committed the result if dependencies changed
- [ ] 改了文案时 zh-CN 与 en-US 都已补齐 / Both locales updated if UI text changed
- [ ] 新功能或修复附带了测试 / Tests added for new behavior or fixes
- [ ] 用户可见的变化已同步文档（README 中英两份、使用说明等）/ Docs updated for user-visible changes
- [ ] 没有在代码、日志或截图里包含 API Key 或个人数据 / No API keys or personal data in code, logs, or screenshots
