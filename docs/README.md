# Wisp 文档

## 使用

| 文档 | 内容 |
| --- | --- |
| [README](../README.md)（[English](../README.en.md)） | 功能概览、三端安装、接入模型、默认快捷键、隐私、已知限制、反馈方式 |
| [使用说明](user-guide.md) | 三大场景的操作、翻译风格、OCR 模式、剪贴板、快捷键速查、全部设置项、文件位置、常见问题与排错、诊断导出 |
| [模型服务](providers.md) | 四种协议、Base URL 规则、服务模板、识图能力判定、「思考」参数、错误提示、各服务的注意事项 |
| [更新日志](../CHANGELOG.md) | 每个版本面向用户的变更 |

## 参与开发

| 文档 | 内容 |
| --- | --- |
| [CONTRIBUTING](../CONTRIBUTING.md)（[English](../CONTRIBUTING.en.md)） | 如何提 issue 与 PR、代码风格、提交信息、测试要求 |
| [开发指南](development.md) | 环境要求、构建与运行、测试与检查命令、调试与开发者工具、目录约定、CI、性能测量、发布流程、更新器密钥 |
| [架构](architecture.md) | 进程与模块结构、各 crate 的职责、IPC、窗口体系、平台层、数据与配置、安全、性能设计 |
| [性能脚本](../scripts/perf/README.md) | 空闲采样、冒烟、性能记录格式、门禁与基线、面板回收对照 |
| [第三方补丁](../vendor/README.md) | `vendor/` 下各 crate 的补丁内容、原因与升级时的处理 |
| [提示词模板](../crates/wisp-prompts/templates/README.md) | 模板文件的格式约定与命名 |
| [图标](../apps/desktop/src-tauri/icons/README.md) | 应用与托盘图标的源稿与生成方式 |
| [AGENTS.md](../AGENTS.md) | 给 AI 编码助手的项目约定 |
| [安全策略](../SECURITY.md) | 如何私下报告安全问题 |
| [行为准则](../CODE_OF_CONDUCT.md) | 参与本项目时的行为约定 |

`images/` 是 README 使用的界面截图，由 `pnpm qa:shots --readme` 生成（见[开发指南 · README 截图](development.md#readme-截图)）。

## 术语

| 术语 | 含义 |
| --- | --- |
| 取词 | 获取用户在任意应用中当前选中的文字 |
| 面板（Panel） | 靠近光标或选区出现的浮动小窗口，翻译结果、OCR 结果、剪贴板列表、输入框共用 |
| 覆盖层（Overlay） | 截图框选时覆盖整个屏幕的透明全屏窗口，每个显示器一个 |
| 固定窗口（Pinned） | 把面板里的结果「固定」出来的可拖动独立窗口 |
| 风格（Style） | 翻译时使用的一组提示词与参数，例如「技术」「口语」 |
| 服务（Provider） | 一个模型服务接入点：协议 + Base URL + API Key + 模型列表 |
| 扩展触发 | 快捷键之外的可选触发方式：选中文字后的浮标、双击修饰键、鼠标中键 / 侧键 |
| 帧模式 | 面板窗口与卡片的关系：Native（macOS，窗口即卡片）与 Css（Windows / Linux，透明窗口内由 CSS 画卡片） |
