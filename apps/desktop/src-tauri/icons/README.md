# 图标

造型：靛蓝圆角方块里一团发光的「鬼火」（will-o'-the-wisp），外焰白、内焰青蓝；托盘图标是同一火焰的单色剪影，
内焰镂空，16–18 pt 下仍可辨认。

## 源稿（`src/`，SVG，改图只改这里）

- `src/app-icon.svg`：应用图标，1024 画布，按 macOS 图标网格留出 100 px 边距与投影；
- `src/tray-template*.svg`：macOS 菜单栏模板图（黑色 + alpha，36 px = 18 pt @2x）；
- `src/tray-light*.svg`：Windows / Linux **浅色**任务栏用（深色字形，32 px）；
- `src/tray-dark*.svg`：Windows / Linux **深色**任务栏用（白色字形，32 px）；
- `src/tray-{light,dark}{,-paused,-attention}-{16,20,24}.svg`：同一造型的**小尺寸手调稿**。32 px 稿直接缩到 16 px 时，
  暂停角标的两条竖线（1.15 px）与镂空内焰会被抗锯齿成灰糊，所以小尺寸单独绘制：内焰按尺寸收缩（16 px 时 0.78）让
  外焰边框至少 2 px；暂停竖线、感叹号与角标的镂空全部落在整像素上（16 px：竖线 2 × 5 px、间隔 1 px；感叹号 1 px 宽）。

每种托盘图标三个状态：正常、`-paused`（剪贴板监听已暂停：右下角暂停角标）、`-attention`（权限缺失 / 快捷键注册失败：
右下角感叹号角标；Windows / Linux 的角标为琥珀色，macOS 模板图随菜单栏明暗着色）。角标处的火焰镂空一圈，
小尺寸下角标与火焰不粘连。

## 生成

1. SVG → PNG：用浏览器按原尺寸渲染、透明背景导出（例如 Chrome 的无头模式；任何能精确栅格化 SVG 的工具都可以）：
   - `src/app-icon.svg` → `source-1024.png`（1024 px）；
   - `src/tray-*.svg` → `tray/tray-*.png`（模板图 36 px，Windows / Linux 32 px）；
   - 小尺寸手调稿 `src/tray-*-{16,20,24}.svg` → `tray/tray-*-{16,20,24}.png`（按原尺寸，不缩放）；
   - 32 px 稿再按 40 / 48 px 渲染 → `tray/tray-*-{40,48}.png`（矢量放大，不是位图放大）。
2. 应用图标各尺寸：在 `apps/desktop` 下执行 `pnpm tauri icon src-tauri/icons/source-1024.png -o src-tauri/icons`，
   生成后删除 `android/`、`ios/`、`Square*Logo.png`、`StoreLogo.png`。

## 使用（`src/tray/icon.rs`）

| 文件 | 用途 |
| --- | --- |
| `tray/tray-template.png` | macOS，`icon_as_template(true)` |
| `tray/tray-template-paused.png` / `-attention.png` | macOS，监听暂停 / 权限缺失（同为模板图） |
| `tray/tray-light.png` / `-paused` / `-attention` | Windows / Linux 浅色任务栏（32 px） |
| `tray/tray-dark.png` / `-paused` / `-attention` | Windows / Linux 深色任务栏、Linux 默认（32 px） |
| `tray/tray-{light,dark}{,-paused,-attention}-{16,20,24,40,48}.png` | 同上，按托盘图标的实际像素尺寸选用 |

状态图标在托盘状态变化时切换（`TrayIcon::set_icon`），文件名与上表一致。

**Windows 按 DPI 选尺寸**：通知区域图标是 `SM_CXSMICON` 大小——100 % 缩放 16 px、125 % 20 px、150 % 24 px、
200 % 32 px（无后缀的文件）、250 % 40 px、300 % 48 px。给系统一张恰好等大的图，它就不会再重采样；介于两档之间
（例如 175 % = 28 px）时取**较大**的一档交给系统缩小。缩放变化（`WM_DPICHANGED` / 换显示器）时重新选一次。
Linux 的 StatusNotifierItem 由宿主按需缩放，32 px 即可；也可以按面板报告的尺寸选用。
