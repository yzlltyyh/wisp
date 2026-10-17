# OCR 评测截图页面

提示词评测器（`wisp-eval`，见 `crates/wisp-prompts/src/eval/`）的 OCR 样本要读截图。仓库里不放截图，只放这些页面：`pnpm eval:ocr` 用 Playwright（Chromium，2 倍像素密度）把每个页面的 `#shot` 元素截成同名 PNG，写到 `target/eval-runs/ocr-samples/`，也就是评测器默认的 `--ocr-dir`。只生成截图用 `pnpm eval:ocr --render-only`，用法见 [docs/development.md](../../../../docs/development.md)「开发者工具」一节的「提示词评测」与「一条命令跑 OCR 评测」。

| 页面 | 截图 | 样本（`samples.rs` 的 `OCR_SAMPLES`） | 内容 |
| --- | --- | --- | --- |
| `terminal.html` | `terminal.png` | O1-terminal-lines | 终端里的 Python 报错：缩进、指向出错位置的 `^`，以及代码里本来的拼写错误（不该被纠正） |
| `table.html` | `table.png` | O2-table-markdown | 一级标题、说明、二级标题、带表头的表格与表注 |
| `formula.html` | `formula.png` | O3-formula-markdown、O3-formula-translate | 英文教材段落：行内公式与三个行间公式（求根公式、巴塞尔级数、高斯积分） |
| `icon.html` | `icon.png` | O4-icon-lines、O4-icon-translate | 一排工具栏图标，没有任何文字 |
| `chat.html` | `chat.png` | O5-chat-paragraphs | 群聊：居中的时间、昵称、在气泡里折成多行的长消息、几条短消息、一个表情贴图 |
| `pdf.html` | `pdf.png` | O6-pdf-paragraphs | 英文论文正文三段，衬线字体两端对齐；行尾有断词连字符（work-/station、inter-/face、expo-/nential、con-/flict），Wi-/Fi 在本来的连字符处折行，行中有 COVID-19、back-off |
| `list.html` | `list.png` | O7-list-paragraphs | 设置页：标题与说明、分组标题下三行设置项（名称、说明、开关）、项目符号列表（第二项折行）、两个按钮 |
| `mixed.html` | `mixed.png` | O8-mixed-paragraphs | 窄栏里两段中英混排：第一段汉字与英文、数字之间有空格，第二段没有；一处在网址的 `/` 之后折行 |

## 改页面时

- **确定性**：只用系统字体（`base.css` 里的 `--ui`、`--serif`、`--mono`），不引外部字体、图片、脚本或 CDN；渲染时任何网络请求都会被拦下并报错。不要用随机数、当前时间这类每次都变的东西。
- **手工断行**：`pdf.html` 与 `mixed.html` 的每一行是一个 `.l` 块，容器带 `.fit`；`fit.js` 按最宽一行定栏宽再两端对齐，所以断行位置只取决于源码，换机器渲染时断词连字符和被折开的网址都还在原处。改动这两页时让各行（段尾一行除外）的自然宽度相近；渲染脚本发现最窄的一行不到最宽一行的 90% 时会提示，否则两端对齐会把字距明显拉开。中英混排页的断行只落在两个汉字或标点之间，不落在汉字与英文的交界处（那里原文有没有空格，从图上看不出来）。
- **期望要对得上**：页面内容改了，同步检查 `samples.rs` 里对应样本的 `expect` 与 `checks.rs` 的自动断言是否仍然成立；新增截图样本时在这里加一个同名页面，渲染脚本会提示还没有页面的截图。
