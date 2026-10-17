# Wisp 文档

## 使用

| 文档 | 内容 |
| --- | --- |
| [README](../README.md)（[English](../README.en.md)） | 功能概览（含离线模型）、三端安装、接入模型、默认快捷键、隐私、已知限制、反馈方式 |
| [使用说明](user-guide.md) | 三大场景的操作、翻译风格、OCR 的识别模式与排版、剪贴板（收藏、智能识别、粘贴为、连续粘贴、悬浮剪贴板）、离线模型（模型与推理组件、下载、选用、内存、性能参考）、PaddleOCR 云端识别、多设备同步与同步的隐私、快捷键速查、全部设置项、文件位置、常见问题与排错、诊断导出 |
| [模型服务](providers.md) | 四种协议、Base URL 规则、服务模板、识图能力判定、「思考」参数、错误提示、各服务的注意事项、PaddleOCR 文字识别服务（地址、访问令牌、模型、连接测试与错误） |
| [更新日志](../CHANGELOG.md) | 每个版本面向用户的变更 |

## 参与开发

| 文档 | 内容 |
| --- | --- |
| [CONTRIBUTING](../CONTRIBUTING.md)（[English](../CONTRIBUTING.en.md)） | 如何提 issue 与 PR、代码风格、提交信息、测试要求 |
| [开发指南](development.md) | 环境要求、构建与运行、macOS 开发签名、测试与检查命令（含文本规则的黄金样例、离线模型与 PaddleOCR 的真机测试）、调试与开发者工具（含提示词与 OCR 评测）、目录约定、CI、性能测量、README 截图、发布流程、更新器密钥与代码签名 |
| [架构](architecture.md) | 进程与模块结构、各 crate 的职责、IPC、窗口体系、平台层、多设备同步、离线模型与推理进程、数据与配置、安全与隐私、性能设计、Cargo feature |
| [性能脚本](../scripts/perf/README.md) | 空闲采样、冒烟、性能记录格式、门禁与基线、面板回收对照 |
| [第三方补丁](../vendor/README.md) | `vendor/` 下各 crate 的补丁内容、原因与升级时的处理 |
| [提示词模板](../crates/wisp-prompts/templates/README.md) | 模板文件的格式约定与命名 |
| [OCR 评测截图页面](../crates/wisp-prompts/eval/ocr-pages/README.md) | `pnpm eval:ocr` 渲染的截图页面、与评测样本的对应、改页面时保持确定性的要求 |
| [PP-OCRv6 黄金样例](../crates/wisp-local/tests/fixtures/ppocr/README.md) | 离线 OCR 与官方产线比对用的参考输出、参数与重新生成方法 |
| [图标](../apps/desktop/src-tauri/icons/README.md) | 应用与托盘图标的源稿与生成方式 |
| [AGENTS.md](../AGENTS.md) | 给 AI 编码助手的项目约定 |
| [安全策略](../SECURITY.md) | 如何私下报告安全问题 |
| [行为准则](../CODE_OF_CONDUCT.md) | 参与本项目时的行为约定 |

`images/` 是 README 使用的界面截图，由 `pnpm qa:shots --readme` 生成（见[开发指南 · README 截图](development.md#readme-截图)）。其中几张同时是离线 OCR 真机测试的输入图，重新生成后要按 [PP-OCRv6 黄金样例](../crates/wisp-local/tests/fixtures/ppocr/README.md) 重做对应的参考输出。

## 术语

| 术语 | 含义 |
| --- | --- |
| 取词 | 获取用户在任意应用中当前选中的文字 |
| 面板（Panel） | 靠近光标或选区出现的浮动小窗口，翻译结果、OCR 结果、剪贴板列表、输入框共用 |
| 覆盖层（Overlay） | 截图框选时覆盖整个屏幕的透明全屏窗口，每个显示器一个 |
| 固定窗口（Pinned） | 把面板里的结果「固定」出来的可拖动独立窗口 |
| 悬浮剪贴板 | 剪贴板视图变成的单例小窗口，默认置顶，点条目直接粘贴进当前应用而不抢焦点 |
| 收藏 | 剪贴板里不会被自动清理、可以命名并按名称搜索的条目（旧版本称「置顶」） |
| 智能识别 | 在剪贴板条目与 OCR 结果的文字里认出电话、邮箱、链接、验证码、单号、金额等，以芯片形式单独粘贴 |
| 排版（Layout） | OCR 结果的换行整理方式（`OcrLayout`）：原样、去掉空行、合并段内换行（`reflow`，只在纯文本模式）、合并为一行；后三种与剪贴板「粘贴为…」的同名变换共用 `wisp-text` 里的规则 |
| 风格（Style） | 翻译时使用的一组提示词与参数，例如「技术」「口语」 |
| 服务（Provider） | 一个模型服务接入点：协议 + Base URL + API Key + 模型列表；PaddleOCR 这类只用于 OCR 的文字识别服务也以服务的形式添加 |
| 离线模型 | 下载到这台设备上运行的识别与翻译模型（PP-OCRv6、PaddleOCR-VL、Index-Translate）；选择只属于本机，不随同步 |
| 推理组件 | 运行离线模型的开源程序：llama.cpp 的 llama-server 与 ONNX Runtime，随第一个需要它的模型按需下载 |
| 槽位 | 一个推理进程及其生命周期：`ort`（ORT 工作进程）、`vl`、`translate`（各一个 llama-server），按需启动、闲置释放 |
| 扩展触发 | 快捷键之外的可选触发方式：选中文字后的浮标、双击修饰键、鼠标中键 / 侧键 |
| 同步数据 | 存放在你自己的 WebDAV 存储里、用同步密码加密的设置与使用记录；每台设备只写自己的文件 |
| 帧模式 | 面板窗口与卡片的关系：Native（macOS，窗口即卡片）与 Css（Windows / Linux，透明窗口内由 CSS 画卡片） |
