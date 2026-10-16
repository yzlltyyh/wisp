# 第三方补丁（vendor/）

这里是打了补丁的第三方 crate 源码副本，经根 `Cargo.toml` 的 `[patch.crates-io]` 引入。它们不是工作区成员
（`[workspace] exclude = ["vendor"]`），`cargo fmt` / `cargo clippy --workspace` 不检查它们。

每个副本都来自 crates.io 发布的版本，除下面列出的改动外与上游逐字一致；副本不带 `examples/`、`Cargo.toml.orig`、
`.cargo_vcs_info.json` 等与构建无关的文件。补丁位置在源码里都以 `Wisp patch` / `Wisp vendoring` 注释标出。

| crate | 版本 | 上游 commit | 平台 | 解决的问题 |
| --- | --- | --- | --- | --- |
| [wry](#wry-0570) | 0.57.0 | `792d0359ba6501a4fc360ece17de2ae42329a47c` | macOS | 建 WebView 时抢前台；自定义协议处理器泄漏；后台线程完成协议响应导致崩溃 |
| [tao](#tao-0371) | 0.37.1 | `37b7e8bc90a050e93be988df636f322c3ef147b6` | macOS | 销毁窗口后视图与窗口委托对象不释放 |
| [notify](#notify-820) | 8.2.0 | `a1d7c2d8f80786679d58ec6d5986a1d4278bc8cf`（`notify/` 子目录） | Windows | 文件监听线程每 100 ms 空转唤醒 |
| [tray-icon](#tray-icon-0251) | 0.25.1 | `b09ceda9f7357b4a1f6bc361fd912a7dfca077cc` | Windows | 托盘「离开检测」定时器停不下来，UI 线程持续唤醒 |
| [oar-ocr / oar-ocr-core](#oar-ocr--oar-ocr-core-0100) | 0.10.0 | `f755256f260d3760a062a6051d23826958972c33` | 全部 | 只用文字检测与识别，却经特性合并链进全部图片解码器（含 AV1 编码器）和带 C 代码的 `tokenizers`；检测框与识别输入和官方 PaddleOCR 产线有像素级差异 |

所有副本都在 `src/lib.rs` 里加了 crate 级 `#![allow(warnings)]`：路径依赖不像 registry 依赖那样带
`--cap-lints allow`，CI 的 `RUSTFLAGS=-D warnings` 会把上游的弃用 / 未知 cfg 等警告变成错误。

每个补丁都有一条守护单测：`Cargo.lock` 里的这个 crate 不再来自 vendor（没有 `source`）、版本变了、或补丁代码丢了时失败，
提醒升级时处理补丁。

---

## wry 0.57.0

**1. 遵守 `focused(false)`，不在建 WebView 时激活应用**（`src/wkwebview/mod.rs`，`src/lib.rs` 的 `focused` 文档注释）

上游在 macOS 上每建一个 WebView 就调用一次 `NSApplication.activate()`，`focused(false)` 不起作用。Wisp 的面板、截图
Overlay、固定窗口都是非激活窗口（非激活 NSPanel，不激活应用地上屏），建窗口这一步如果激活应用，前台会从用户正在用的
应用抢走、菜单栏随之切换。补丁让 `activate()` 只在 `attributes.focused` 为 true 时调用；设置 / 引导窗口仍以
`focused(true)` 创建，行为不变。

上游相关：[tauri-apps/wry#1866](https://github.com/tauri-apps/wry/pull/1866)（同一问题的修复 PR）、
[tauri-apps/tauri#12055](https://github.com/tauri-apps/tauri/issues/12055)、
[tauri-apps/tauri#14102](https://github.com/tauri-apps/tauri/issues/14102)。

**2. 自定义协议处理器不再泄漏**（`src/wkwebview/mod.rs`、`src/wkwebview/class/url_scheme_handler.rs`）

每个自定义协议（Wisp 有 `wisp` / `tauri` / `ipc` 三个）的处理器由 `new` 创建（+1），`setURLSchemeHandler:forURLScheme:`
又 retain 一次，而 `new` 的那一次从没释放——每建一个 WebView 泄漏三个处理器对象。补丁在注册之后立即放掉自己的引用，
处理器随 WebView 的配置一起释放；处理器类加上 `dealloc`，释放 `webview_id` 的 C 字符串再调用 `[super dealloc]`。

**3. 异步协议响应在主线程完成**（`src/wkwebview/class/url_scheme_handler.rs`）

异步自定义协议（Tauri 的 `ipc://` 在异步命令完成的线程上、Wisp 的 `wisp://`）调用响应闭包时，上游在那个线程上
「核对任务有效 → `didReceiveResponse:` / `didReceiveData:` / `didFinish` → 释放 WebView 引用」。这期间主线程可能
销毁 WebView（WebKit 先 `stopURLSchemeTask:`），随后的 `did*` 抛出 Objective-C 异常，release 构建（`panic = "abort"`）
随即 abort，WebView 的最后一次释放也可能落在后台线程。补丁让闭包不在主线程时把这一整段经
`dispatch_async_f(&_dispatch_main_q, …)` 投递到主线程执行（libdispatch 属于 libSystem，不新增依赖）；已在主线程
（同步处理器、404 路径）时原地执行，与上游相同。

**4. 去掉 `[[example]]` 目标**（`Cargo.toml`）：副本不带 `examples/`。

守护单测：`windows::native::tests::wry_is_patched_so_unfocused_webviews_do_not_activate_the_app`
（`apps/desktop/src-tauri/src/windows/native/mod.rs`）。

升级 tauri / wry 时：先看上游是否已让 macOS 遵守 `focused`、`url_scheme_handler::create` 之后是否释放了处理器、响应闭包
是否已在主线程完成 `did*` 调用；都修好了就删掉 `[patch.crates-io]` 的 wry 与本目录，否则把仍需要的改动移植到新版本。
改完在 macOS 上跑一次 window lab（`WISP_WINDOW_LAB_INPUT_PROBE=1` 时连同 `overlay_keys`），确认 Overlay 能收到按键、
Wisp 在系统层面没有到前台。

---

## tao 0.37.1

**1. `TaoView::dealloc` 调用 `[super dealloc]`**（`src/platform_impl/macos/view.rs`）

上游覆写的 `dealloc` 只释放了自己的状态，没有调用 `[super dealloc]`——视图对象本身以及 NSView 持有的一整套对象
（`_NSViewLayoutAux`、`NSTextInputContext`、`NSTrackingArea`、子视图数组……）永远不释放，每销毁一个窗口（面板回收 /
卸载、Overlay、固定窗口、设置窗口）留下一套。补丁先注销 `initWithTao:` 里登记的 `NSViewFrameDidChangeNotification`
观察者，再调用 `[super dealloc]`。父类写死为 `NSView`：按对象当前的类取父类的话，对象被 KVO 换过 isa 时会递归。

**2. `TaoWindowDelegate::dealloc` 调用 `[super dealloc]`**（`src/platform_impl/macos/window_delegate.rs`）

同样补上 super 调用（父类 `NSResponder`），之前先从 `NSDistributedNotificationCenter` 注销 `initWithTao:` 里登记的主题
变化观察者——对象真正释放之后，不能再有通知送到它。窗口的 `delegate` 在 `windowWillClose:` 里已经清掉（上游原有），
AppKit 不会再给它发消息。

**3. 去掉 `[[example]]` 目标**（`Cargo.toml`）：副本不带 `examples/`、`Cargo.lock`。

上游相关：[tauri-apps/tao#1327](https://github.com/tauri-apps/tao/issues/1327)（macOS 关闭窗口后 NSWindow、wry 的父视图与
`TaoWindowDelegate` 不释放）。另外，tao 0.37.1 的 `create_window` 给 NSWindow 多 retain 了一次，这一处不在补丁里，
由应用层的 `windows::native::adopt_window`（`apps/desktop/src-tauri/src/windows/native/macos.rs`）补偿，本补丁不改窗口的
引用计数。

守护单测：`windows::native::tests::tao_is_patched_so_destroyed_windows_free_their_views`
（`apps/desktop/src-tauri/src/windows/native/mod.rs`）。

升级 tauri / tao 时：先看上游的 `view.rs` / `window_delegate.rs` 的 `dealloc` 是否已经调用 super；修复了就删掉
`[patch.crates-io]` 的 tao 与本目录，否则把第 1、2 处移植到新版本。改完在 macOS 上跑 window lab 的
`WISP_WINDOW_LAB_RECYCLE_STRESS=220`（看 malloc 斜率），并用 `NSZombieEnabled=YES` 跑一次完整实验台（确认没有过度释放）。

---

## notify 8.2.0

Wisp 常驻一个 notify 监听做设置热加载（`settings::watcher`，Windows 上监听配置目录）。上游 Windows 后端
（`ReadDirectoryChangesWatcher`）的服务线程 `notify-rs windows loop` 在循环里 `WaitForSingleObjectEx(wakeup_sem, 100, TRUE)`
——每 100 ms 超时醒一次去 `try_recv` 命令通道，磁盘上什么都没变也一样。监听存在多久，进程就多久保持约 10 次/秒的唤醒
（Wisp 的空闲预算是主进程 ≤ 0.2 次/秒）。macOS（FSEvents）与 Linux（inotify + mio）的后端没有这个问题；notify 9.0.0-rc.5
仍是同样的写法。

**1. 无限期的可警报等待**（`src/windows.rs`，`ReadDirectoryChangesServer::run`）

等待改为 `INFINITE`（仍是可警报等待）。不需要轮询的理由：

- 监听句柄发出的每个命令（`watch` / `unwatch` / `Drop` 的 `Stop`）在 `send` 之后都会 `ReleaseSemaphore(wakeup_sem)`，服务
  线程随即醒来处理；信号量上限为 1，多条命令只醒一次，`try_recv` 循环一次取完；命令在 `try_recv` 之后、等待之前到达时
  信号量已是 1，等待立即返回；
- `ReadDirectoryChangesW` 的完成例程（`handle_event`，包括它在目录被删 / 出错时投递的 `Unwatch`）作为 APC 在这次可警报
  等待里执行，执行完等待返回 `WAIT_IO_COMPLETION`，循环回到 `try_recv`。

**2. `configure` 之后唤醒服务线程**（`src/windows.rs`，`ReadDirectoryChangesWatcher::configure`）

上游这里没有唤醒、靠 100 ms 的轮询捎带处理；改成无限期等待之后不补的话，`rx.recv()` 会永远等下去。Wisp 不调用
`configure`，补上是为了补丁本身正确。

效果：Windows 上同一份代码只换 notify 的对照，按线程计数（`scripts/perf/windows-thread-wakeups.ps1`，120 s 窗口）——
`notify-rs windows loop` 线程约 10 次/秒 → 0，空闲主进程合计约 10 次/秒 → ≤ 0.1 次/秒；`settings::watcher` 的单测（含真实的
ReadDirectoryChangesW 事件、rename 保存、`Drop` 停止的端到端用例）在 Windows 上照常通过。

守护单测：`settings::watcher::tests::notify_is_patched_so_the_windows_watcher_sleeps_until_an_event`
（`apps/desktop/src-tauri/src/settings/watcher.rs`）。

升级 notify 时：先看上游（notify-rs/notify，`src/windows.rs` 的 `ReadDirectoryChangesServer::run`）是否已去掉这个超时；去掉了
就删掉 `[patch.crates-io]` 的 notify 与本目录，否则把第 1、2 处移植到新版本，并在 Windows 上用
`scripts/perf/windows-thread-wakeups.ps1` 复测空闲唤醒。

---

## tray-icon 0.25.1

tauri 2.12.2 的托盘实现。Windows 的托盘图标收不到 `WM_MOUSELEAVE`，tray-icon 用一个 15 ms 的定时器
（`WM_USER_LEAVE_TIMER_ID`）检测「光标离开图标」：悬停时每次光标移动（托盘回调的 `WM_MOUSEMOVE`）都记下位置、重设
定时器，定时器的一拍看光标还在不在图标矩形里，不在就 `KillTimer` 并发 `TrayIconEvent::Leave`。上游的定时器处理先
`userdata.last_position.take()`，只有取到 `Some` 才检查——光标在图标上停住一拍（15 ms）之后，位置已被取走，之后的每一拍
都是 `None`、直接返回，**既不检查也不关定时器**，直到下一次 `WM_MOUSEMOVE` 重新写入位置。于是：

- 光标一直停在图标上（例如右键菜单按 Esc 关掉之后人离开）：定时器一直走，UI 线程每秒约 64 次 `WM_TIMER`；
- 停住之后**第一下移动就已经在图标外**（停在图标边缘再移开、或停住后快速甩出）：壳层不会再给这个窗口发
  `WM_MOUSEMOVE`（光标已不在图标上），定时器从此常驻，直到下一次悬停；
- 光标或图标矩形读不到时（安全桌面、图标被隐藏、资源管理器重启）上游同样直接返回、保留定时器。

0.26.1 仍是同样的写法。Wisp 只用 `Click`（按下时刷新权限 / 惰性检查更新），不用 `Enter` / `Move` / `Leave`。

**1. 离开检测定时器有界**（`src/platform_impl/windows/mod.rs`）

- `WM_MOUSEMOVE`（重设定时器的那一支）把这次读到的图标矩形缓存进 `leave_rect`、停表计数 `leave_ticks` 清零（上游在这里
  本来就为事件读一次 `Shell_NotifyIconGetRect`，缓存不增加开销）；
- `WM_TIMER`：每一拍只读光标（`GetCursorPos`），与缓存的矩形比较——不再每拍跨进程问资源管理器。光标不在图标里（或光标 /
  矩形读不到）：`KillTimer`、`entered = false`、发 `Leave`。光标仍在图标里：计数加一，连续 `LEAVE_TIMER_STILL_TICKS`（32）
  拍——约 0.5 s 没有新的 `WM_MOUSEMOVE`，即光标停住了——就 `KillTimer`，`entered` 保持为 true；下一次在图标上移动的
  `WM_MOUSEMOVE` 重新起表。定时器最多比悬停多走约 0.5 s，光标停在哪里都没有常驻的周期唤醒；
- 代价：光标停住超过约 0.5 s、之后第一下移动就落在图标外时，收不到这次 `Leave`（上游在同一情形下同样收不到，还会让定时器
  常驻）。

`Cargo.lock` 里 tray-icon 的依赖保持与 registry 版本相同的解析（`windows-sys 0.61.2`）：补丁 crate 重新解析时 cargo 会挑
工作区里已有的 0.60.2，需要手工改回，`cargo metadata --locked` 通过。

守护单测：`tray::tests::tray_icon_is_patched_so_the_leave_timer_never_outlives_the_hover`
（`apps/desktop/src-tauri/src/tray/mod.rs`）。

升级 tauri / tray-icon 时：先看上游（tauri-apps/tray-icon，`src/platform_impl/windows/mod.rs` 的 `WM_TIMER` 处理）是否已修复；
修复了就删掉 `[patch.crates-io]` 的 tray-icon 与本目录，否则把第 1 处移植到新版本。两种情况都在 Windows 上跑一次
`scripts/perf/windows-tray-probe.ps1`（`-Mode Input` 与 `-Mode Post`），看悬停、停住、移开之后 UI 线程的唤醒。

---

## oar-ocr / oar-ocr-core 0.10.0

本地 PP-OCRv6 的检测与识别用 oar-ocr 的 `OAROCRBuilder`（det + rec），只在 ORT 工作进程里执行。上游的依赖声明让三样
用不到的东西进了安装包：

- `oar-ocr`、`oar-ocr-core` 依赖 `image` 与 `imageproc` 时都用默认特性，`imageproc` 的默认特性又打开 `image/default`。
  Cargo 的特性合并让整个工作区的 `image` 打开全部解码器（avif + rav1e 编码器、exr、tiff、webp、gif、qoi……），
  `wisp-capture` 里按内容猜格式的 `image::load_from_memory` 于是把它们都链接进来；
- `oar-ocr-core` 无条件依赖 `tokenizers`（`onig` 特性带 Oniguruma 的 C 代码），而它只被公式识别用到；
- 随之而来的 92 个 crate 进了第三方许可清单（其中 rav1e、exr 等是 BSD 许可）。

同一份 release profile 的最小程序实测（macOS arm64，只解 png + jpeg 的基线 567 584 B）：直接依赖上游并调用 det + rec
为 1 959 872 B（+1.39 MB），换成本补丁后 1 066 304 B（**+0.50 MB**）。

**1. 新增 `formula` 特性**（两个 crate 的 `Cargo.toml`；`oar-ocr-core/src/domain/adapters/mod.rs`、
`oar-ocr-core/src/predictors/mod.rs`、`oar-ocr/src/oarocr/structure.rs`）

`oar-ocr-core` 的 `tokenizers` 改为可选，`formula = ["dep:tokenizers"]`；公式识别的适配器与预测器模块按它编译。
`oar-ocr` 新增 `formula = ["oar-ocr-core/formula"]`，结构化产线（`OARStructureBuilder`）里公式识别的字段、构造、
`with_formula_recognition` 与识别步骤按它编译；关掉时 `recognize_formulas` 返回空、公式区域不在 OCR 前被涂白（与
上游「没配公式模型」时的行为相同）。三处只在开了公式识别时才有意义的单测同样按它编译。

**2. 新增 `image-formats` 特性**（两个 crate 的 `Cargo.toml`）

`image`、`imageproc` 改为 `default-features = false`（OCR 只处理内存里的像素，`imageproc` 用到的轮廓、形态学、距离变换
都不依赖它的可选特性），`image-formats = ["image/default"]`（`oar-ocr` 同时转发给 `oar-ocr-core`）。

两个新特性都在 `default` 里，默认构建与上游行为一致；Wisp 以 `default-features = false` 依赖 `oar-ocr`，两者都不开。
补丁按可以提交给上游的形态写（改动只是特性开关与 `#[cfg]`），已在开与不开两种组合下编译通过。

**3. 其他**：`oar-ocr` 去掉 `[[example]]` 目标（副本不带 `examples/`、`docs/`）；`oar-ocr-core` 的发布包里没有许可证文件，
副本放了同一仓库（`oar-ocr` 发布包里）的 Apache-2.0 `LICENSE`。两个发布包都没有 NOTICE 文件。

**4. 预处理与后处理与官方 PaddleOCR 产线逐像素对齐**（`oar-ocr-core/src/processors/opencv/` 新增；
`processors/resize_detection.rs`、`db_bitmap.rs`、`db_score.rs`、`db_postprocess.rs`、`sorting.rs`、`resize_recognition.rs`、
`models/recognition/crnn.rs`、`utils/transform.rs`、`utils/mod.rs`；`oar-ocr/src/oarocr/processors.rs`）

官方产线（PaddleX 3.7）的结果由 OpenCV 与 pyclipper 的定点运算、坐标约定与并列取舍决定，上游的近似实现让检测框差一两个像素、
个别边界区域过不了 `box_thresh`，识别输入也不同（黄金样例 6 张图 × tiny / small 只有 317 / 397 行「文字一致且 IoU ≥ 0.9」）。
补丁把这些步骤按 OpenCV 4.10 / Clipper 6.4.2 的通用代码路径逐步移植（`processors::opencv`，注明出处并保留许可声明），
接到原来的位置上：

- 检测输入尺寸：比例与截断用双精度，32 对齐用 Python `round()`（银行家舍入，`max(int(round(x / 32) * 32), 32)`）；
  上游的 `(x + 16) / 32 * 32` 把恰为 16 奇数倍的边向上取，例如高 528 得 544（官方 512）；
- 缩放：`cv2.resize` 的 `INTER_LINEAR`（半像素中心、11 位定点系数、纵向的移位舍入、恰好 2 倍缩小时走 `INTER_AREA`）。
  上游用 image-rs 的 `Triangle`，缩小时会加宽核（抗锯齿），与 OpenCV 差几个灰度级；`DetResizeForTest` 默认用移植版
  （`with_filter` 仍可换回 image-rs 的滤波器），识别输入同样换成移植版；
- DB 后处理：`cv2.minAreaRect`（Sklansky 凸包 + 旋转卡壳，float 运算与并列取舍照搬）与 `cv2.boxPoints`；`box_score_fast` 用
  `cv2.fillPoly` 的掩码（坐标先截断成整数，含 8 连通的边线像素）并以双精度求均值，`box_thresh` 按十进制值比较；`unclip` 用
  pyclipper 的整数外扩（坐标截断、圆角 `ArcTolerance 0.25`、远离零舍入，输出的起点与 Clipper 相同），上游是 Clipper2
  的浮点外扩；轮廓按 `cv2.findContours(RETR_LIST)` 的顺序（发现顺序的倒序）取前 `max_candidates` 个；缩放回原图用双精度
  乘法与银行家舍入；
- 文本行裁剪：按 PaddleX `CropByPolys.get_minarea_rect_crop` 先取检测框的最小外接矩形再裁；`get_rotate_crop_image` 的输出
  尺寸是两组对边长度最大值的截断（上游四舍五入），在整张图上用 `cv2.warpPerspective(INTER_CUBIC, BORDER_REPLICATE)` 的
  定点双三次（1/32 像素量化、`A = -0.75`、15 位系数）采样，上游是浮点 Catmull-Rom（`A = -0.5`）并先裁出外接矩形
  （边缘像素被复制而不是取真实邻点）；整数的轴对齐框直接拷贝（与双三次在整数位置的结果相同）；
- 识别输入宽度：`int(48 * max_wh_ratio)`、`ceil(48 * w / h)` 用双精度（上游 f32 偶尔差一列），超过 3200 时整批拉到 3200；
- 阅读顺序：`sort_quad_boxes` 按第一个顶点（左上角）排序，与 PaddleX `SortQuadBoxes` 相同（上游用外接矩形的最小值）。

移植代码的许可：OpenCV 4.5.0 起整体以 Apache-2.0 发布，这几个源文件头部仍保留原有的 3-clause BSD 声明（Intel / Willow Garage / OpenCV Foundation / Itseez），
两者都要求二进制分发时随附许可与版权声明，所以在 `scripts/licenses-extra.json` 登记为 `opencv`（Apache-2.0 AND BSD-3-Clause）；Clipper 6.4.2 是 BSL-1.0
（只要求源码保留声明，已在 `clipper.rs` 头部保留），同样登记一项。两份声明原文都在 `processors/opencv/` 的源码注释里。

改动以「Wisp patch」注释标出，接口保持兼容（新增 `processors::opencv`、`utils::get_minarea_rect_crop`、
`DBPostProcess::apply_map` / `box_score_fast_cv`、`OCRResize::paddle_widths`、`DetResizeForTest::opencv_linear`），
上游自带的单测在补丁后仍全部通过（需要真实 ONNX Runtime 的一项除外）。对齐由 `crates/wisp-local/tests/ocr_opencv_parity.rs`
守护：参考数据（`tests/fixtures/ppocr/parity/`）由官方产线所用的同一批 Python 函数生成，缩放、裁剪逐字节相同，
最小外接矩形逐位相同，外扩顶点序列与 DB 框完全相同。补丁后同一份像素输入的黄金样例（`ocr_live`）在两档上文字与框完全一致。

守护单测：`oar_ocr_is_patched_so_unused_image_codecs_and_c_code_stay_out`（`crates/wisp-local/tests/ocr_vendor_patch.rs`，补丁 1–3）；
`crates/wisp-local/tests/ocr_opencv_parity.rs`（补丁 4，补丁代码丢失时编译失败，行为偏离时比对失败）。

升级 oar-ocr 时：先看上游是否已有等价的特性开关（`imageproc` 不开默认特性、`tokenizers` 可选）与等价的对齐实现；有了就删掉
`[patch.crates-io]` 的两行与本目录，改用上游的特性；没有就把上面几处移植到新版本，重新跑 `pnpm run licenses`，用
`cargo tree -i rav1e`、`cargo tree -i onig_sys` 确认它们不在依赖图里，并跑 `ocr_opencv_parity` 与 `ocr_live` 的黄金样例。
