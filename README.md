<div align="center">

简体中文 · [English](README.en.md)

<img src="docs/images/icon.png" width="96" height="96" alt="Wisp 图标">

# Wisp

**看见、理解、记住。** 常驻后台的轻量桌面伴侣：截图 OCR · 划词翻译 · 剪贴板。

macOS（Apple Silicon）· Windows（x64）· Linux（x64）　|　v0.1.1　|　MIT

[下载与安装](#安装) · [接入模型](#接入模型服务) · [快捷键](#默认快捷键) · [使用说明](docs/user-guide.md) · [更新日志](CHANGELOG.md) · [反馈](#反馈)

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/translate-dark.png"><img src="docs/images/translate-light.png" width="760" alt="在笔记应用里选中一段英文，按快捷键后译文在选区下方逐字流出"></picture>

</div>

Wisp 把三件最常用的小事收进一个随叫随到的面板：选中文字立刻翻译，框选屏幕立刻识别文字，复制过的东西随时找回。它平时安静地待在菜单栏 / 托盘里，几乎不占资源；按下快捷键，面板就出现在你正在看的地方，用完即走，不抢焦点。

所有 AI 能力都由你自己接入的模型完成——OpenAI、Anthropic、Gemini、DeepSeek、通义、Kimi，或者本机的 Ollama。没有 Wisp 账号、没有遥测、没有中转，API Key 保存在系统的凭据存储里，不写进配置文件。在多台电脑上使用时，可以经你自己的 WebDAV 存储同步，数据在本机加密后才上传。

## 功能

### 划词翻译

在任意应用里选中文字，按 <kbd>⌥ D</kbd>（Windows / Linux：<kbd>Ctrl + Alt + D</kbd>）。译文在选区下方逐字流出，面板不会抢走焦点，你可以继续在原应用里滚动、选择。

- **自动方向**：原文已经是你的目标语言时，自动译成第二语言（默认中文 ⇄ 英文）；语言条可以随时交换方向。
- **6 种风格**：自然、技术、学术、口语、学习（附词汇与语法讲解）、直译，<kbd>⌘ 1–9</kbd> 一键切换；也可以新建自己的风格，绑定专用模型和全局快捷键。
- **词典式结果**：选中单个单词或短语时给出音标、释义与例句。
- 润色、校对（逐处标出改动）、提升译文；原文对照；固定成独立窗口；术语表；没有选中文字时直接输入。

<p align="center"><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/dictionary-dark.png"><img src="docs/images/dictionary-light.png" width="640" alt="词典式结果：serendipity 的音标、两条释义和例句"></picture></p>

### 截图 OCR

按 <kbd>⌥ S</kbd>（Windows / Linux：<kbd>Ctrl + Alt + S</kbd>），屏幕定格，拖出一个区域，松手即开始识别。

- 放大镜与尺寸标签帮你对齐像素；鼠标停在窗口上会高亮整个窗口，单击即选中；拖动中按住空格可以平移选区；<kbd>⌘ A</kbd> 选中整个屏幕。
- 三种识别模式：**纯文本**（逐行转录，保留换行与缩进）、**段落**（合并段内断行，适合文章）、**结构化**（标题、列表、表格转成 Markdown）。
- 识别结果可以直接修改，默认自动复制；点「翻译」在同一个面板里得到译文。<kbd>⌥ ⇧ S</kbd> 截图后直接给出译文。

<p align="center"><img src="docs/images/overlay.png" width="720" alt="框选屏幕区域：选区四角的控制点、尺寸标签与像素放大镜"></p>
<p align="center"><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/ocr-dark.png"><img src="docs/images/ocr-light.png" width="640" alt="OCR 面板：截图缩略图、识别出的中文文字，下半部分是英文译文"></picture></p>

### 剪贴板

按 <kbd>⌥ V</kbd>（Windows / Linux：<kbd>Ctrl + Alt + V</kbd>）唤出历史，直接打字搜索，回车粘贴回原来的应用。

- 记录文本、富文本、图片和文件，自动认出链接、颜色与代码；中文一两个字也能搜到。
- <kbd>⇧ ↩</kbd> 纯文本粘贴、<kbd>⌘ ↩</kbd> 只复制不粘贴、<kbd>⌘ 1–9</kbd> 快速粘贴、空格预览全文或大图、<kbd>⌘ P</kbd> 置顶。
- 疑似密码、令牌和私钥的内容不记录；1Password、Bitwarden、KeePassXC 等密码管理器默认排除；遵守应用标记的「不要记录」。

<p align="center"><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/clipboard-dark.png"><img src="docs/images/clipboard-light.png" width="640" alt="剪贴板面板：置顶区与最近区，混合了文本、链接、代码、颜色、图片和文件"></picture></p>

### 多设备同步

在「设置 → 同步」连接你自己的 WebDAV 存储（坚果云、123 云盘、Nextcloud、群晖 NAS 等），就能在多台 Mac、Windows 与 Linux 电脑之间同步设置、模型服务与 API Key、翻译风格、术语表、剪贴板历史（含图片）与识别历史。同步默认关闭。

- **端到端加密**：数据在本机用你设置的同步密码加密后才上传，服务器只能看到密文；没有 Wisp 账号，也不经过中转服务器。
- **自动、安静**：改动后几秒内上传，使用 Wisp 时取回其他设备的改动，不在后台定时轮询；冲突自动合并，不弹对话框。
- **加入不删本机数据**：新设备加入时与服务器上的数据合并，本机独有的模型服务、术语与历史都会保留；以前同步过的设备再连接回来时，会应用它离开期间其他设备上的删除与清空。
- **该分开的分开**：快捷键只在同一系统的设备之间同步；开机启动、剪贴板记录开关等只属于本机。

详见使用说明的[多设备同步](docs/user-guide.md#多设备同步)与[同步与隐私](docs/user-guide.md#同步与隐私)。

### 也很在意的细节

- **跟手**：触发之后立刻有反馈；首个字到达前是骨架动画，不会出现空白面板；动效可以一键减少。
- **键盘优先，也照顾触控板**：五个动作都有全局快捷键，面板里的每个操作都有键位；不依赖鼠标中键或侧键。
- **外观**：浅色 / 深色、8 种强调色或自定义颜色、毛玻璃或纯色、字号与面板宽度都可以调；中文与英文界面。
- **可选的扩展触发**：选中文字后出现的浮标、双击修饰键、鼠标中键 / 侧键（默认关闭，关闭时不安装任何系统钩子）。
- **轻**：常驻时几乎不占资源。在 Apple Silicon Mac 上，空闲时 CPU 约 0.01 %，主进程约 13–15 MiB，系统 WebView 进程约 50 MiB（面板刚用过的几十秒里会多一些，见[已知限制](#已知限制)）。按下快捷键到面板出现，连续唤出时 95 % 在约 30 ms 内，启动后的第一次唤出与带缩略图的剪贴板列表也都在 100 ms 以内。macOS 安装包约 7.6 MB。

## 安装

从 [最新的 Release](https://github.com/yzlltyyh/wisp/releases/latest) 下载对应平台的安装包（历史版本见 [Releases](https://github.com/yzlltyyh/wisp/releases)）。只提供下面三种平台架构，不提供 Intel Mac、Windows ARM64 与 universal 包。

| 平台 | 安装包 | 系统要求 |
| --- | --- | --- |
| macOS · Apple Silicon | `Wisp_0.1.1_aarch64.dmg` | macOS 14 Sonoma 或更新 |
| Windows · x64 | `Wisp_0.1.1_x64-setup.exe` | Windows 10 / 11 |
| Linux x64 · Debian / Ubuntu | `Wisp_0.1.1_amd64.deb` | Ubuntu 22.04 / 24.04（选 Ubuntu on Xorg）、Debian 12+，或其他提供 X11 会话的发行版¹ |
| Linux x64 · Fedora | `Wisp-0.1.1-1.x86_64.rpm` | Fedora；完整功能需要 X11 桌面（Xfce、MATE、Cinnamon 等）¹；RHEL 系需先启用 EPEL² |
| Linux x64 · 其他发行版 | `Wisp_0.1.1_amd64.AppImage` | glibc 2.35+、FUSE 2；完整功能同样需要 X11 会话¹ |

¹ Linux 上**只有 X11 会话功能完整**。Ubuntu 25.10 起（包括 26.04 LTS）和 Fedora 43 起的默认 GNOME 已经没有 Xorg 会话——安装包照样能装，但只能按 Wayland 的限制使用（见 [Linux](#linux)）。

² rpm 的依赖用的是 Fedora 的包名（`webkit2gtk4.1`、`libayatana-appindicator-gtk3`）：RHEL / AlmaLinux / Rocky Linux 上后者只在 EPEL 里，且没有在 RHEL 系上验证过；openSUSE 等包名不同的发行版请用 AppImage。

每个版本的 Release 说明末尾列出了全部安装包的 SHA-256，下载后可以核对：macOS / Linux 用 `shasum -a 256 <文件>`，Windows 用 PowerShell 的 `Get-FileHash <文件>`。

### macOS

1. 打开 dmg，把 **Wisp** 拖进「应用程序」文件夹（不要直接在 dmg 或「下载」里运行，否则无法设置开机启动）。
2. **首次打开**：Wisp 没有经过 Apple 公证，第一次打开会被系统拦下（提示「未打开 Wisp」或「Apple 无法验证」）。点「完成」，然后打开「系统设置 → 隐私与安全性」，滚动到底部的「安全性」，点 **「仍要打开」** 并确认。

   也可以在终端里去掉下载隔离标记，之后双击即可打开：

   ```bash
   xattr -dr com.apple.quarantine /Applications/Wisp.app
   ```

3. Wisp 常驻在菜单栏，平时没有 Dock 图标；打开设置窗口期间会出现在 Dock 和 ⌘Tab 中。第一次启动会打开引导，按提示授予两项权限：
   - **辅助功能**：读取选中的文字、把剪贴板条目粘贴回原应用；
   - **屏幕录制**：截图 OCR。授权后需要点「重新启动 Wisp」才会生效。macOS 15 起系统会定期再次确认屏幕录制授权，照常允许即可。

   「输入监控」只在你启用双击修饰键等扩展触发时才需要。
4. 如果钥匙串询问 Wisp 能否使用钥匙串里保存的 API Key，输入登录密码后选「始终允许」（通常只在更新版本之后出现）。

> [!IMPORTANT]
> 这一版只做了 ad-hoc 签名，没有固定的签名身份：**每次更新到新版本后，macOS 都会把 Wisp 当成一个新应用**。更新后菜单栏里 Wisp 的菜单会提示「更新后需要重新授权」——到「系统设置 → 隐私与安全性」的「辅助功能」和「屏幕录制」里，先用「−」移除旧的 Wisp，再重新添加或打开开关；钥匙串会要求输入登录密码，然后选「始终允许」。

### Windows

1. 运行 `Wisp_0.1.1_x64-setup.exe`。安装包没有代码签名，SmartScreen 提示「Windows 已保护你的电脑」时，点 **「更多信息 → 仍要运行」**。浏览器提示「不常下载」时选择保留。
2. 安装到当前用户，不需要管理员权限。电脑上缺少 WebView2 运行时（部分 Windows 10）时，安装程序会自动下载它，需要联网。
3. Wisp 常驻在通知区域。图标可能被收进「^」溢出菜单，可以把它拖到任务栏上常显。
4. 卸载：Windows 11 在「设置 → 应用 → 已安装的应用」，Windows 10 在「设置 → 应用 → 应用和功能」里找到 Wisp。

### Linux

```bash
# Debian / Ubuntu
sudo apt install ./Wisp_0.1.1_amd64.deb

# Fedora（RHEL / AlmaLinux / Rocky Linux 先启用 EPEL；openSUSE 等请用 AppImage）
sudo dnf install ./Wisp-0.1.1-1.x86_64.rpm

# AppImage（Ubuntu 22.04 需要 libfuse2，24.04 起是 libfuse2t64）
chmod +x Wisp_0.1.1_amd64.AppImage && ./Wisp_0.1.1_amd64.AppImage
```

> [!IMPORTANT]
> **完整功能需要 X11 会话。** 可以选的环境：
>
> - Ubuntu 22.04 / 24.04：登录界面右下角的齿轮里选「Ubuntu on Xorg」；Debian 12 / 13 选「GNOME on Xorg」；
> - Xfce、MATE、Cinnamon 等仍以 X11 为默认会话的桌面：Xubuntu、Ubuntu MATE、Linux Mint，以及 Fedora 的对应版本；仍提供「Plasma (X11)」会话的 KDE（如 Kubuntu 24.04）也可以。
>
> **Ubuntu 25.10 起（包括 26.04 LTS）和 Fedora 43 起的默认 GNOME 已经没有 Xorg 会话**，Kubuntu 26.04 与 Fedora KDE 默认也只装 Wayland 会话。在这些环境里 Wisp 只能按下表的 Wayland 一栏工作：没有全局快捷键、取词、截图和剪贴板记录，主要剩下输入翻译（经托盘或 `--trigger` 打开）。Wayland 的全局快捷键与截图门户在[路线图](#路线图)里。

- **托盘**：GNOME 需要「AppIndicator and KStatusNotifierItem Support」扩展（Ubuntu 默认已启用）。没有托盘时 Wisp 会打开设置窗口并发通知，所有功能仍可用快捷键触发。
- **保存 API Key** 需要 Secret Service（GNOME Keyring 或 KWallet）。没有时 Key 只保存在本次运行中，设置页会给出提示。

| 功能 | X11 | Wayland |
| --- | --- | --- |
| 全局快捷键 | ✓ | ✗ 用系统快捷键调用 `wisp-desktop --trigger <动作>` 代替 |
| 读取选中的文字 | ✓（PRIMARY 选区，必要时模拟复制） | ✗ 以输入模式打开，先复制再粘贴进去 |
| 截图 OCR | ✓ | ✗ 显示「截图失败」，说明为「这一版在 Wayland 会话下无法截图。需要截图时，请切换到 X11 会话」（这一版还没有接入 Wayland 的截图门户） |
| 剪贴板记录 | ✓ | ✗ 不记录新的复制 |
| 粘贴回原应用 | ✓ | 只复制，提示你按 <kbd>Ctrl + V</kbd> |
| 扩展触发 | ✓ | ✗ |
| 面板透明与圆角 | ✓ | 不透明 |

`--trigger` 可用的动作：`translate_selection`、`translate_input`、`ocr`、`ocr_translate`、`clipboard`。deb / rpm 安装的命令是 `wisp-desktop`，AppImage 直接用 AppImage 文件的路径；Wisp 已在运行时，命令会转给正在运行的实例。

## 接入模型服务

Wisp 不内置任何模型和 Key，翻译和 OCR 都通过你自己的服务完成。首次启动的引导里就可以接入，之后在「设置 → 模型与服务」管理。

<p align="center"><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/settings-providers-dark.png"><img src="docs/images/settings-providers-light.png" width="640" alt="设置窗口的「模型与服务」页：OpenAI、DeepSeek、Ollama 三个服务与翻译、OCR 的默认模型"></picture></p>

1. **添加服务**：从模板里选一个——OpenAI、OpenAI（Responses API）、Anthropic、Google Gemini、DeepSeek、OpenRouter、通义千问（阿里云百炼）、智谱 GLM、火山方舟（豆包）、月之暗面 Kimi、硅基流动、MiniMax、Groq、Ollama（本机）、LM Studio（本机），或「中转站 / 自定义兼容端点」。模板会预填协议、地址和推荐参数，都可以改。
2. **填写 Base URL 与 API Key**，点「测试连接」：Wisp 会拉取模型列表，并发一条极短的流式请求验证整条链路。本机的 Ollama / LM Studio 通常不需要 Key。
3. **选择默认模型**：翻译和 OCR 分别选。OCR 只列出支持图片的模型；风格也可以单独绑定模型。

各服务的注意事项、识图能力的判定、「关闭思考」等高级选项与错误说明见[模型服务](docs/providers.md)。

### 四种协议

| 协议 | 适用 | 请求地址（`{base}` 为规范化后的 Base URL） |
| --- | --- | --- |
| OpenAI Chat Completions | 兼容性最好，绝大多数服务、中转站与本机模型都支持 | `POST {base}/chat/completions` |
| OpenAI Responses | OpenAI 官方的新接口（Wisp 一律带 `store: false`，不在服务端留存） | `POST {base}/responses` |
| Anthropic Messages | Claude 系列 | `POST {base}/messages` |
| Google Gemini | Gemini 系列，对话模型都能识图 | `POST {base}/models/{模型}:streamGenerateContent?alt=sse` |

### Base URL 怎么填

只需要填到版本段，不要带 `/chat/completions` 之类的接口路径——编辑时下方会实时显示最终的请求地址。

- 地址里没有版本段时自动补上：OpenAI / Anthropic 协议补 `/v1`，Gemini 补 `/v1beta`；
- 已经有版本段（`/v1`、`/v3`、`/v1beta` 等）时原样使用；
- 服务的路径不带版本段、又不想被补上时，在末尾加 `#`，Wisp 会按填写的路径请求。

| 填写 | 最终请求 |
| --- | --- |
| `https://api.openai.com` | `https://api.openai.com/v1/chat/completions` |
| `https://dashscope.aliyuncs.com/compatible-mode/v1` | `…/compatible-mode/v1/chat/completions` |
| `https://ark.cn-beijing.volces.com/api/v3` | `…/api/v3/chat/completions` |
| `https://api.anthropic.com` | `https://api.anthropic.com/v1/messages` |
| `https://generativelanguage.googleapis.com` | `…/v1beta/models/{模型}:streamGenerateContent?alt=sse` |
| `http://localhost:11434`（Ollama） | `http://localhost:11434/v1/chat/completions` |
| `https://relay.example.com/openai#` | `https://relay.example.com/openai/chat/completions` |

### API Key 存在哪里

Key 只写入系统凭据存储：macOS 钥匙串与 Linux Secret Service 里服务名为 `wisp`、账户为服务 ID；Windows 凭据管理器里是「Windows 凭据 → 普通凭据」下的 `<服务 ID>.wisp`。保存后界面只显示「已保存在钥匙串」之类的状态，不会回显，也不会出现在配置文件、日志和诊断包里；界面进程拿不到 Key，请求由后台进程直接发出。删除服务时会一并删除它的 Key。

开启[多设备同步](#多设备同步)并勾选「API Key」时，Key 在本机加密后随模型服务同步到你的其他设备，到达后同样只写入那台设备的系统凭据存储；在一台设备上删除服务，其他设备上的这个服务与它的 Key 也会删除。

## 默认快捷键

| 动作 | macOS | Windows | Linux |
| --- | --- | --- | --- |
| 划词翻译 | <kbd>⌥ D</kbd> | <kbd>Ctrl + Alt + D</kbd> | <kbd>Ctrl + Alt + D</kbd> |
| 输入翻译 | <kbd>⌥ ⇧ D</kbd> | <kbd>Ctrl + Alt + Shift + D</kbd> | <kbd>Ctrl + Alt + Shift + D</kbd> |
| 截图 OCR | <kbd>⌥ S</kbd> | <kbd>Ctrl + Alt + S</kbd> | <kbd>Ctrl + Alt + S</kbd> |
| 截图直接翻译 | <kbd>⌥ ⇧ S</kbd> | <kbd>Ctrl + Alt + Shift + S</kbd> | <kbd>Ctrl + Alt + Shift + S</kbd> |
| 剪贴板 | <kbd>⌥ V</kbd> | <kbd>Ctrl + Alt + V</kbd> | <kbd>Ctrl + Alt + V</kbd> |

首选键被系统或其他应用占用时，Wisp 自动改用备选键并在菜单栏 / 托盘菜单里提示；两个都不可用时留空，等你在「设置 → 快捷键」里重新录制。

| 动作 | macOS 备选 | Windows 备选 | Linux 备选 |
| --- | --- | --- | --- |
| 划词翻译 | <kbd>⌃ ⌥ D</kbd> | <kbd>Alt + Shift + D</kbd> | <kbd>Super + Alt + D</kbd> |
| 输入翻译 | <kbd>⌥ A</kbd> | <kbd>Ctrl + Alt + A</kbd> | <kbd>Ctrl + Alt + A</kbd> |
| 截图 OCR | <kbd>⌃ ⌥ S</kbd> | <kbd>Alt + Shift + S</kbd> | <kbd>Super + Alt + S</kbd> |
| 截图直接翻译 | <kbd>⌃ ⌥ ⇧ S</kbd> | <kbd>Alt + Shift + T</kbd> | <kbd>Super + Alt + T</kbd> |
| 剪贴板 | <kbd>⇧ ⌘ V</kbd> | <kbd>Alt + Shift + V</kbd> | <kbd>Super + Alt + V</kbd> |

面板里的常用键：<kbd>Esc</kbd> 关闭，<kbd>⌘ C</kbd> 复制，<kbd>⌘ R</kbd> 重新生成，<kbd>⌘ P</kbd> 固定 / 置顶，<kbd>⌘ ,</kbd> 打开设置（Windows / Linux 把 <kbd>⌘</kbd> 换成 <kbd>Ctrl</kbd>）。完整键位见[使用说明](docs/user-guide.md#快捷键速查)。

## 隐私

- **不收集任何数据**：没有遥测、没有 Wisp 账号、没有内置 Key，也不经过任何中转服务器。
- **只有三类网络请求**：发给你自己配置的模型服务的请求（你选中的文字或框选的截图）；检查更新时访问 GitHub（可以在「设置 → 通用 → 检查更新」里关闭）；以及你自己配置并开启的同步服务器（WebDAV，默认关闭）。关闭检查更新、也没有开启同步时，Wisp 不会自己发出任何请求。剪贴板内容不会为了预览而联网。
- **数据默认留在本机**：剪贴板历史保存在本机的 SQLite 数据库（数据目录下的 `clipboard.db` 与 `blobs/`，未加密，依赖系统账户与磁盘加密保护）；截图只在内存里处理，开启「识别历史」后才会保存到本机。开启同步后，你选择同步的内容在本机加密后才上传到你自己的 WebDAV 存储；服务器看不到内容，只能看到文件的数量、大小与修改时间（见[同步与隐私](docs/user-guide.md#同步与隐私)）。
- **日志不含正文**：日志只记录耗时、状态码等元数据，不记录原文、译文、剪贴板内容、截图与 Key，也不记录同步服务器的地址、账号与密码；「设置 → 关于 → 诊断信息 → 导出诊断包」导出的日志与设置同样经过脱敏。

## 已知限制

- **未签名、未公证**：macOS 首次打开要手动放行，且每次更新后要重新授权（见[安装 · macOS](#macos)）；Windows 首次运行有 SmartScreen 提示。
- **Windows**：测试覆盖不如 macOS 充分，可能还有未发现的兼容性问题；遇到问题请按[反馈](#反馈)一节提交。
- **Linux**：完整功能需要 X11 会话；Ubuntu 25.10 起（包括 26.04 LTS）和 Fedora 43 起的默认 GNOME 只有 Wayland 会话，多数功能不可用（见[安装 · Linux](#linux)）。Linux 版本主要经过 CI 与自动化测试，真机上的测试有限。
- **Gemini 接口的退役风险**：Gemini 协议使用 `generateContent` / `streamGenerateContent`。Google 正在推广新的 Interactions API，如果旧接口将来下线，Gemini 协议会失效；届时可以改用 Gemini 的 OpenAI 兼容端点（协议选 OpenAI Chat Completions，Base URL 填 `https://generativelanguage.googleapis.com/v1beta/openai`）。
- **快捷键**：把 <kbd>⌥</kbd> + 字母设为快捷键后，这个组合原本输入的特殊字符（如 ∂、ß、√）在所有应用里都无法再输入；在终端开启了「Option 作为 Meta」的用户会失去 `M-d` 等组合，可以改用备选键。密码框等安全输入状态下快捷键可能不触发。
- **取词**：少数应用不通过无障碍接口提供选中的文字，Wisp 会改为模拟一次复制（并还原剪贴板）；Windows 上以管理员身份运行的窗口无法读取，也无法自动粘贴进去。
- **同步**：只支持 WebDAV（HTTP Basic 认证；除局域网地址外必须用 https）。Wisp 不在后台定时同步，另一台设备上的改动要等你使用 Wisp 时才会取回。剪贴板里的文件条目与超过 10 MB 的图片不同步。同步密码不能直接修改，忘记后也无法找回，只能清除服务器上的数据后重新连接（各设备本机的数据不受影响）。
- **内存**：面板刚用过的几十秒里系统 WebView 会多占一些内存（macOS 上约 84 MiB、峰值约 90 MiB，直到面板隐藏约 45 秒后回收），回收后约 53–54 MiB（从没打开过面板时约 49 MiB）；主进程从没打开过面板时约 13 MiB，用过之后约 15 MiB。Windows 的 WebView2 通常比 macOS 占用更多。

## 不做什么

Wisp 只做截图 OCR、划词翻译和剪贴板这三件事，把它们做轻、做快。下面这些是刻意不做的，界面里也不会出现入口：

- 应用启动器与文件搜索（Spotlight、Raycast 已经做得很好）；
- 插件系统与脚本扩展；
- 通用的 AI 聊天窗口（多轮对话）；
- Wisp 账号与中转服务器——同步只经过你自己的 WebDAV 存储，数据在本机加密后才上传；
- 内置模型、内置 Key 或中转服务——始终用你自己接入的模型服务；
- 以本地 OCR 引擎（Tesseract、PaddleOCR 等）为主路径——识别交给视觉模型，本机 OCR 最多作为离线时的备用（见路线图）；
- 语音输入与朗读（TTS）；
- 移动端。

## 路线图

计划中的方向（尚未实现，界面里也没有入口）：

- 完善 Windows 版的兼容性；
- 更多服务模板；本机 OCR 引擎作为离线时的备用方案（不是主路径）；
- 更完整的 Linux 测试，以及 Wayland 的全局快捷键与截图门户（GlobalShortcuts / Screenshot portal）；
- macOS Developer ID 签名与公证、Windows 代码签名；Homebrew Cask、winget、AUR 等分发渠道；
- 剪贴板数据库加密、按图片中的文字检索剪贴板图片。

朗读译文（TTS）属于上面的「不做什么」；以后是否作为一个小功能加入，会单独评估。

## 开发

技术栈：Tauri 2（Rust）+ React 19 + Motion + Tailwind CSS 4。需要 Rust 1.96.1（由 `rust-toolchain.toml` 锁定）、Node ≥ 24.15（或 22.22.2 以上的 22.x）、pnpm ≥ 12.3.4（`corepack enable` 后自动使用）；Linux 另需 WebKitGTK 4.1 等系统库（`bash scripts/ci/linux-deps.sh`）。

```bash
pnpm install --frozen-lockfile
pnpm dev                          # 开发运行（WISP_FAKE_LLM=1 时使用内置的假模型，不需要 Key）
pnpm -C apps/desktop dev:mock     # 只启动前端与浏览器 mock，用于界面开发
pnpm build:debug                  # 打一个 debug 安装包（不需要更新器私钥）
cargo test --workspace && pnpm test
```

macOS 的隐私授权与钥匙串访问绑定在代码签名上，默认的 ad-hoc 签名每次重新编译都会变，之前给的授权随之失效。执行一次 `scripts/dev-sign-macos.sh --init` 创建开发证书，之后用 `scripts/dev-sign-macos.sh --dev` 代替 `pnpm dev`，授权在重新编译后仍然有效，详见[开发指南 · macOS 开发签名](docs/development.md#macos-开发签名)。

- [开发指南](docs/development.md)：环境、构建与测试命令、调试、CI 与发布流程；
- [架构](docs/architecture.md)：进程与模块结构、IPC、窗口体系、平台层、多设备同步、数据与配置；
- [参与贡献](CONTRIBUTING.md)：如何提 issue 与 PR、代码风格与提交约定。

全部文档的索引见 [docs/](docs/README.md)。

## 项目状态

当前版本 **v0.1.1**（2026-10-14，[Release 页面](https://github.com/yzlltyyh/wisp/releases/tag/v0.1.1)）：所有悬浮面板都可以拖动，修正了 Windows 分数缩放下的截图框选与 macOS 打开设置时的 Dock 图标。

三大场景与设置、引导、自动更新、诊断导出等功能都已可用（首个版本 0.1.0 于 2026-10-13 发布，已由 0.1.1 取代）。macOS 是测试最充分的平台，Windows 与 Linux 的测试覆盖仍在完善。完整的变更记录见 [CHANGELOG.md](CHANGELOG.md)。

## 反馈

问题与建议请提交到 [GitHub Issues](https://github.com/yzlltyyh/wisp/issues/new/choose)（选择「问题报告」或「功能建议」模板），写明：

- Wisp 版本（「设置 → 关于」）与系统版本（例如 macOS 15.5、Windows 11 24H2、Ubuntu 24.04 on Xorg）；
- 操作步骤、期望的结果与实际的结果，能附截图或录屏更好；
- **诊断包**：「设置 → 关于 → 诊断信息 → 导出诊断包」导出一个 zip，作为附件上传。诊断包包含脱敏后的日志与设置、翻译风格、版本与系统信息和取词方式的统计（完整清单见[使用说明](docs/user-guide.md#诊断导出与反馈)），不含 API Key、同步服务器的地址、账号与密码，也不含任何原文、译文、剪贴板内容（导出前可以解压检查）。排查前可以先把「设置 → 通用 → 高级 → 日志级别」临时调到「调试」再复现一次。

**不要在 Issue、截图或日志里贴出 API Key**（同步用的应用密码与同步密码也一样）；截图里出现 Key、个人信息或剪贴板内容时请先遮掉。

**安全问题请不要公开提交**，请按 [SECURITY.md](SECURITY.md) 私下报告。参与开发见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 许可

[MIT](LICENSE) © 2026 yzlltyyh

Wisp（/wɪsp/）是一缕轻烟、一束微光——轻、快、几乎感觉不到它的存在，正是这个软件想做到的样子。
