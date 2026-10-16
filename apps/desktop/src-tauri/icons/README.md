# 图标

造型：「拾字小妖精」——一只衔着小纸片的小精灵：单眼、头顶一缕卷翘、身后两道尾迹，意思是随手把文字拾起来。
品牌蓝 `#3266E3`，深色 `#17233C`。应用图标是品牌蓝圆角方块里的白色剪影；托盘与菜单栏用为小尺寸简化过的字形
（`wisp-mark-micro`：尾迹更短、眼睛更大、纸片改为实心），16 px 下仍可辨认。

## 源稿（`src/`，SVG，改图只改这里）

画布都是 `0 0 100 100`（横排字标为 `0 0 285 100`）。

| 文件 | 内容 |
| --- | --- |
| `src/app-icon.svg` | 应用图标：80 × 80 的品牌蓝圆角方块（圆角 18），四周留 10 % 透明边距，里面是白色主标识。设置「关于」页与引导的欢迎页直接引用这张图 |
| `src/app-icon-small.svg` | 应用图标的小尺寸稿：同样的方块，字形换成 `wisp-mark-micro`，用于 48 px 及以下 |
| `src/app-icon-dark.svg` | 深色备选（`#17233C` 底），目前不参与打包 |
| `src/wisp-mark.svg` | 主标识，单色 `currentColor`；`wisp-mark-blue.svg` / `wisp-mark-white.svg` 是定色版本 |
| `src/wisp-mark-micro.svg` | 小尺寸字形：托盘、菜单栏，以及 Windows / Linux 浮标圆盘上的字形（`wisp-platform` 的 `common/bobber_glyph.rs` 照它的路径绘制） |
| `src/wisp-logo-horizontal.svg` | 横排字标：主标识 + 「wisp」，文字已转为轮廓，不需要安装字体 |
| `src/tray-template{,-paused,-attention}.svg` | macOS 菜单栏模板图（纯黑 + alpha，系统按菜单栏明暗着色） |
| `src/tray-light{,-paused,-attention}.svg` | Windows / Linux **浅色**任务栏用：深色字形（`#17233C`） |
| `src/tray-dark{,-paused,-attention}.svg` | Windows / Linux **深色**任务栏用：白色字形 |

托盘图标三个状态：正常、`-paused`（剪贴板记录已暂停：字形缩到 86 %，右下角暂停角标）、`-attention`（权限缺失 /
快捷键注册失败：右下角感叹号角标）。角标与字形同色，和字形之间留出空隙，小尺寸下也不粘连。每种托盘图标只有一份
矢量稿，各个尺寸都由它按目标尺寸直接渲染。

## 生成

1. SVG → PNG：按目标尺寸渲染、透明背景导出（例如用 Chrome 无头模式直接打开 SVG、视口设为目标尺寸后截图；任何能
   精确栅格化 SVG 的工具都可以）：
   - 应用图标：`src/app-icon-small.svg` 渲染 16 / 24 / 32 / 48 px，`src/app-icon.svg` 渲染 64 / 128 / 256 / 512 /
     1024 px。其中 32 → `32x32.png`、64 → `64x64.png`、128 → `128x128.png`、256 → `128x128@2x.png`、
     512 → `icon.png`、1024 → `source-1024.png`；
   - 托盘：`src/tray-template*.svg` → `tray/tray-template*.png`（36 px = 18 pt @2x）；`src/tray-{light,dark}*.svg` →
     `tray/tray-{light,dark}*.png`（32 px），以及同名加 `-16` / `-20` / `-24` / `-40` / `-48` 后缀的各尺寸（矢量按
     各自尺寸渲染，不做位图缩放）。
2. `icon.icns`：把第 1 步的 PNG 放进 `icon.iconset/`——`icon_16x16.png`（16）、`icon_16x16@2x.png`（32）、
   `icon_32x32.png`（32）、`icon_32x32@2x.png`（64）、`icon_128x128.png`（128）、`icon_128x128@2x.png`（256）、
   `icon_256x256.png`（256）、`icon_256x256@2x.png`（512）、`icon_512x512.png`（512）、`icon_512x512@2x.png`（1024），
   再执行 `iconutil -c icns icon.iconset -o icon.icns`。
3. `icon.ico`：16 / 24 / 32 / 48 / 64 / 256 px 六个 PNG 条目，用任意 ICO 打包工具生成。
4. `docs/images/icon.png`（README 顶部的图标）与 `128x128@2x.png` 是同一个文件（见 `scripts/qa/readme-shots.ts`）。

也可以在 `apps/desktop` 下执行 `pnpm tauri icon src-tauri/icons/source-1024.png -o src-tauri/icons` 一次生成第 1 步的
应用图标与第 2、3 步的文件（生成后删除 `android/`、`ios/`、`Square*Logo.png`、`StoreLogo.png`），但它把 1024 px
大图直接缩小，48 px 及以下不会用到小尺寸稿。

## 使用（`src/tray/icon.rs`）

| 文件 | 用途 |
| --- | --- |
| `tray/tray-template.png` | macOS，`icon_as_template(true)` |
| `tray/tray-template-paused.png` / `-attention.png` | macOS，监听暂停 / 权限缺失（同为模板图） |
| `tray/tray-light.png` / `-paused` / `-attention` | Windows 浅色任务栏（32 px，200 % 缩放） |
| `tray/tray-dark.png` / `-paused` / `-attention` | Windows 深色任务栏（32 px，200 % 缩放）；Linux 一律用它（另加描边，见下） |
| `tray/tray-{light,dark}{,-paused,-attention}-{16,20,24,40,48}.png` | 同上的各尺寸版本，Windows 按缩放选用（见下） |

状态图标在托盘状态变化时切换（`TrayIcon::set_icon`），文件名与上表一致。「有新版本」的蓝点与疑似密钥时闪一次的琥珀色点
没有对应的文件，运行时合成在右下角；Linux 上在 `tray-dark*` 外加一圈半透明深色描边，浅色面板上也清晰。圆点与描边都按
图标边长等比缩放（有下限，16 px 上仍看得出、轮廓不断）。

**按缩放选尺寸**：Windows 通知区域图标是 `SM_CXSMICON` 大小——100 % 缩放 16 px、125 % 20 px、150 % 24 px、
200 % 32 px（无后缀的文件）、250 % 40 px、300 % 48 px。`wisp-platform` 按主显示器（通知区域所在的主任务栏）当前的
DPI 查询这个尺寸，`icon.rs` 从尺寸表里选恰好等大的一张交给系统，它就不会再重采样；介于两档之间（例如 175 % = 28 px）
时取**较大**的一档交给系统缩小，超过 300 % 时用 48 px。不需要定时器：每次托盘刷新（任何托盘状态变化）、托盘菜单打开时、
任一应用窗口的缩放变化（`ScaleFactorChanged`）、以及系统外观监听收到主显示器缩放变化（`WM_DPICHANGED`）或显示器配置
变化（`WM_DISPLAYCHANGE`）时重新选一次，尺寸没变就不重新设置图标。macOS 菜单栏固定使用 36 px 模板图；Linux 的
StatusNotifierItem 由宿主按面板尺寸缩放，应用拿不到实际尺寸，固定使用 32 px。
