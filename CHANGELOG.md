# 更新日志

每个版本的段落先由 [git-cliff](https://git-cliff.org) 根据 Conventional Commits 生成，再经人工审定、按功能概括；版本号遵循 SemVer。

Each version ends with an English summary.

## 未发布

### 新功能

- 多设备同步：在「设置 → 同步」连接你自己的 WebDAV 存储（坚果云、123 云盘、Nextcloud、群晖 NAS 等），在多台 Mac、Windows 与 Linux 设备之间同步设置、模型服务、翻译风格、术语表、API Key、剪贴板历史（含单张不超过 10 MB 的图片）与识别历史；每一类都可以单独关闭。引导页的「接入模型」一步也可以直接去连接同步。
- 端到端加密：数据在本机用同步密码派生的密钥加密后才上传，服务器只看到密文；没有 Wisp 账号，也不经过中转服务器。同步密码可以一键生成；它不保存、也无法找回，忘记后只能清除服务器上的数据重新开始，各设备本机的数据不受影响。
- 自动同步：改动后约 2 秒上传，打开面板、托盘菜单或设置窗口时取回其他设备的改动；不在后台定时轮询，暂时连不上时稍后自动重试。也可以关闭自动同步，只在点「立即同步」时同步。
- 冲突自动合并，不弹对话框：同一项以最后的改动为准，不同项的改动都会保留。加入已有的同步数据时，服务器上已有的设置优先，本机独有的模型服务、术语与历史会保留，不会删除本机已有的数据；以前同步过的设备再连接回来时，会应用它离开期间其他设备上的删除与清空。
- 剪贴板的复制、置顶、删除与清空会同步到其他设备，粘贴只影响本机；同步来的条目同样经过本机的排除名单、疑似密钥检测与保留设置。服务器上的历史受保留设置限制，不会无限增长。
- 全局快捷键、风格快捷键与面板材质只在同一系统的设备之间同步；开机启动、剪贴板记录开关、疑似密钥检测、识别历史开关与日志、性能选项只属于本机。
- 同步页显示状态与进度、同步过的设备，提供「立即同步」「更新密码…」「断开同步」与「清除服务器上的数据」；同步需要处理时，托盘状态行会提示，成功时不打扰。

### 修复

- 用 Windows 记事本等编辑器保存、带 UTF-8 BOM 的 settings.json、styles.json、glossary.json 与 state.json 不再被当成损坏的文件而恢复默认。
- 设置窗口里的模型服务与术语表改为逐条保存：手动修改配置文件或从其他设备同步来的改动，不会被设置窗口里打开着的旧内容整组覆盖。

### English summary

- Multi-device sync: connect WebDAV storage you own (Nutstore, 123pan, Nextcloud, a Synology NAS, and others) in Settings → Sync to sync settings, model services, translation styles, the glossary, API keys, clipboard history (including images up to 10 MB each), and recognition history across Macs, Windows PCs, and Linux machines. Each kind of data can be turned off separately, and onboarding links straight to sync.
- End-to-end encryption: everything is encrypted on your device with a key derived from your sync password before it's uploaded, so the server only sees ciphertext. There is no Wisp account and no relay server. The sync password is never stored and can't be recovered; if you forget it, clear the data on the server and start over — the data on each device is kept.
- Automatic sync uploads changes within about two seconds and picks up changes from your other devices when you open a panel, the tray menu, or Settings. Wisp never polls in the background and retries quietly when the server can't be reached; you can also turn automatic sync off and use Sync now.
- Conflicts are merged automatically without dialogs. Joining existing sync data never deletes anything on your device: settings already on the server take priority, and model services, glossary terms, and history that only this device has are kept. A device that synced before and reconnects picks up anything deleted or cleared elsewhere while it was away.
- Clipboard copies, pins, deletions, and clears sync across devices, while pasting stays local; synced items still pass this device's exclusions, secret detection, and retention limits, which also keep the history on the server bounded.
- Shortcuts and panel material sync only between devices on the same operating system; launch at login, clipboard recording, secret detection, the recognition history switch, and logging and performance options stay on each device.
- Fixes: configuration files saved with a UTF-8 BOM (for example by Windows Notepad) are no longer treated as corrupt and reset; providers and glossary entries in Settings are now saved one at a time, so changes made in the files or synced from another device aren't overwritten by stale values in an open Settings window.

## 0.1.1（2026-10-14）

修复版本：修正 0.1.0 发布之后发现的面板拖动、Windows 分数缩放下的截图框选与 macOS Dock 图标问题。

### 修复

- 所有悬浮面板（翻译、输入、OCR 结果、剪贴板）都可以按住头部或空白处拖动；正文、输入框与按钮上按下不会触发拖动，正文仍可选中复制。拖动后的位置在译文流式长高、切换视图时保持不变，长高伸出屏幕时整体推回可见区域；拖动只影响本次打开，剪贴板的「记住位置」照旧。
- 拖动面板的过程中失去焦点不再把面板关掉。
- Windows 在 125 %、150 %、175 % 等分数缩放下，截图框选的四条边线画长或画短、遮罩的亮区与选框对不上、暗化带够不到屏幕边缘，窗口悬停高亮的边框同样走样；放大镜在缩放比例不同的显示器之间移动后尺寸不对。现在边线、亮区与高亮都正好贴合选区。
- macOS 打开设置窗口时，Dock 里的图标显示为空白占位图，现在显示 Wisp 的图标。

### English summary

- All floating panels (translation, input, OCR result, clipboard) can be dragged by the header or any empty area; text stays selectable, and the dragged position holds while a translation streams in or the view changes.
- Losing focus while dragging a panel no longer closes it.
- Windows: under fractional display scaling (125 %, 150 %, 175 % and so on), the screenshot selection edges, the mask cutout, and the window hover highlight now line up exactly with the selection; the magnifier keeps the right size when it moves between monitors with different scaling.
- macOS: while Settings is open, the Dock shows the Wisp icon instead of a blank placeholder.

## 0.1.0（2026-10-13）

第一个版本：截图 OCR、划词翻译与剪贴板三件事，常驻后台、随叫随到。支持 macOS 14+（Apple Silicon）、Windows 10 / 11（x64）与 Linux x64（完整功能需要 X11 会话，Wayland 下多数功能不可用）。

### 划词翻译

- 在任意应用里选中文字按 ⌥D（Windows / Linux：Ctrl+Alt+D），译文在选区旁边逐字流出；面板不抢焦点，可以继续在原应用里滚动、选择。
- 取词优先走系统无障碍接口（macOS 辅助功能、Windows UI Automation、X11 PRIMARY 选区），读不到时才模拟一次复制，并把剪贴板原样还原。
- 自动判断翻译方向：原文已是目标语言时译成第二语言；语言条可以交换方向或临时改目标语言。
- 6 种内置风格（自然、技术、学术、口语、学习、直译）与自定义风格，⌘1–9 一键切换；风格可以单独绑定模型和全局快捷键，提示词可编辑、可一键还原。
- 单词与短语给出词典式结果：音标、词性、释义与例句。
- 润色、校对（逐处标出改动）与「提升译文」；原文对照；把结果固定成可拖动的独立窗口；超长文本先确认再翻译；相同内容 10 分钟内直接取缓存。
- 术语表：指定词条的译法或保持不译，可导入、导出。
- 没有选中文字时打开输入框，直接输入或粘贴要翻译的内容。

### 截图 OCR

- ⌥S（Windows / Linux：Ctrl+Alt+S）冻结屏幕后框选：放大镜与尺寸标签、悬停高亮整个窗口、拖动中按住空格平移选区、⌘A 全屏，支持多显示器。
- 交给视觉模型识别，三种模式：逐行（保留换行与缩进）、段落（合并段内断行）、结构化（标题、列表、表格转为 Markdown）。
- 结果可直接编辑，默认自动复制；一键翻译到同一面板的下半部分，风格与目标语言随时切换。
- 截图直接翻译（⌥⇧S / Ctrl+Alt+Shift+S），需要时再查看原文。
- 可选的识别历史（默认关闭，文字与截图只保存在本机），可以回看、复制、重新翻译。

### 剪贴板

- 记录复制过的文本、富文本、图片与文件，自动识别链接、颜色与代码；⌥V（Windows / Linux：Ctrl+Alt+V）唤出，打开即可打字搜索。
- 中文一两个字也能搜到的全文检索、按类型筛选、置顶、全文与大图预览。
- 回车粘贴回原来的应用，另有纯文本粘贴、仅复制与 ⌘1–9 快速粘贴；常见终端自动改用各自的粘贴键。
- 隐私：跳过疑似密码、令牌与私钥的内容，默认排除 1Password、Bitwarden、KeePassXC 等密码管理器，并遵守应用标记的「不要记录」。
- 保留策略：最多条数、保留天数与单条大小上限，在空闲时清理。

### 接入模型

- 四种协议：OpenAI Chat Completions、OpenAI Responses、Anthropic Messages、Google Gemini，全部流式输出。
- 16 个服务模板（OpenAI、Anthropic、Gemini、DeepSeek、OpenRouter、通义千问、智谱、火山方舟、Kimi、硅基流动、MiniMax、Groq、Ollama、LM Studio、中转站等），Base URL 可自定义并实时显示最终请求地址，可以附加请求头。
- 连接测试、拉取模型列表并标出视觉与推理模型；翻译与 OCR 分别选默认模型；按服务的方式关闭「思考」以降低延迟，参数不被接受时自动调整后重试。
- API Key 只保存在系统凭据存储（macOS 钥匙串、Windows 凭据管理器、Linux Secret Service），界面不回显。

### 体验

- 5 个全局快捷键，三端各有一套避开常见冲突的默认键；录制时检测冲突，注册失败时自动改用备选键并在托盘提示。
- 可选的扩展触发：选中文字后的浮标、双击修饰键、鼠标中键与侧键（默认关闭，关闭时不安装任何系统钩子）。
- 首次启动引导：授权、接入模型、试一试。
- 浅色与深色、8 种强调色或自定义颜色、毛玻璃或纯色材质、字号与面板尺寸、减少动效；中英文界面。
- 托盘菜单、开机启动、单实例；`--trigger <动作>` 命令行参数，可以绑定到系统快捷键。
- 检查更新（默认只提示）与应用内安装，更新包经签名校验。
- 一键导出诊断包：日志与脱敏后的设置，不含 API Key 与任何正文。

### 轻量

- 常驻时几乎不占资源：在 Apple Silicon Mac 上，空闲时 CPU 约 0.01 %、主进程约 13 MiB（用过面板之后约 15 MiB），另有系统 WebView 进程约 49 MiB（面板刚用过的几十秒里约 84 MiB，面板隐藏约 45 秒后回收，回收后约 53–54 MiB）。
- 按下快捷键到面板出现：连续唤出时 95 % 在约 30 ms 内；启动后的第一次唤出、回收后重新加载的面板与带缩略图的剪贴板列表也都在 100 ms 以内。
- macOS 安装包约 7.6 MB。

### English summary

- First release: screenshot OCR, selection translation, and clipboard history in one panel that stays in the background, for macOS 14+ (Apple Silicon), Windows 10 / 11 (x64), and Linux x64 (full functionality requires an X11 session).
- Selection translation: ⌥D (Ctrl+Alt+D on Windows / Linux) streams the translation next to the selection without taking focus; automatic direction, six built-in styles plus custom styles, dictionary-style results for words and phrases, polishing and proofreading, a glossary, and an input box when nothing is selected.
- Screenshot OCR: ⌥S (Ctrl+Alt+S) freezes the screen for selection with a magnifier, window highlighting, and multi-monitor support; three recognition modes (lines, paragraphs, structured Markdown); editable results that are copied automatically; one-step translation of the result, direct screenshot translation, and an optional local recognition history.
- Clipboard: records text, rich text, images, and files; ⌥V (Ctrl+Alt+V) opens a searchable list; paste back into the previous app, paste as plain text, and pin items; likely passwords, tokens, and password managers are skipped; retention limits are configurable.
- Models: OpenAI Chat Completions, OpenAI Responses, Anthropic Messages, and Google Gemini, all streamed; 16 provider templates; connection tests and model lists that flag vision and reasoning models; API keys stay in the system credential store.
- Experience: five global shortcuts with conflict detection, optional extra triggers, first-run onboarding, light and dark themes with accent colors, Chinese and English UI, a tray menu, launch at login, signed in-app updates, and a diagnostics export that contains no keys or content.
- Lightweight: on an Apple Silicon Mac it idles at about 0.01 % CPU, with about 13 MiB for the main process and about 49 MiB for the system WebView processes; repeated invocations show the panel within about 30 ms (95th percentile).
