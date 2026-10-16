//! `ClipboardService`：启动时只安装监听、不打开数据库；托盘就绪且 Panel 预热完成后的空闲时刻一次性打开数据库并加载
//! RecentCache。
//!
//! 本文件是对外的异步外壳：会阻塞的工作（数据库、平台读写、主线程往返）
//! 一律放进阻塞线程池，逻辑本身在 `engine` / `paste` / `retention` 等模块的 `Engine` 里。
//!
//! `start` 之后常驻三个后台任务，都是事件驱动、空闲时零唤醒：
//! 1. 监听工作任务（`watch::run`）：停在 Notify 上，平台回调转交序号后读取并记录；
//! 2. 空闲清理调度（`retention::run`）：停在 Notify 上，有活时等「面板隐藏且 30 s 无变更」；
//! 3. 设置订阅：剪贴板设置变化时调整监听与保留策略。
//!
//! 图片缩略图：`ClipStore::ingest` 不等缩略图就返回（`image.thumb_url = null`），先 emit `clipboard:changed`
//! 的 `inserted`；随后在后台工作线程调用 `ClipStore::make_thumb(id)`，拿到更新后的元数据时 `RecentCache::update` 并
//! emit `updated`（前端按 id 原地替换，不改变顺序）。
//!
//! 剪贴板流程的显式建模：粘贴是一个「阶段 × 事件」的状态机（`paste::paste_step`，单飞并按
//! 面板会话去重，取消 / 重入 / 失败各有转移），事件投递有「视图已显示 / 准备中 / 隐藏」三态（`outbox`），空闲清理是
//! 纯函数 `retention::plan` 驱动的调度；转移表都有单测。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tauri_plugin_opener::OpenerExt;
use wisp_clipboard::BlobKind;
use wisp_types::{
    ClipDragData, ClipFilter, ClipInspect, ClipItemDetail, ClipItemMeta, ClipPage, ClipboardPayload, ClipboardSettings,
    OcrHistoryItem, OsKind, PasteOn, PasteOutcome, QueueStatus, SendTarget, SourceApp,
};

use crate::bus::{Bus, Command, TriggerOrigin};
use crate::error::{IpcError, IpcErrorCode};
use crate::settings::SettingsService;
use crate::state::Deps;

use super::engine::{Engine, store_error};
use super::paste::{PasteHost, PasteRequest};
use super::port::{AppHost, Exec, Host, PoolExec, Port};
use super::queue::QueueEnd;
use super::texts::{self, Text};
use super::{assets, retention, watch};

/// 剪贴板服务
pub struct ClipboardService {
    app: tauri::AppHandle,
    settings: Arc<SettingsService>,
    bus: Bus,
    engine: Arc<Engine>,
    started: AtomicBool,
}

impl ClipboardService {
    /// 构造（不安装监听、不打开数据库）；注册 `wisp://` 的 `img/`、`thumb/`、`icon/`、`ocrimg/` 资源来源。
    /// 暂停状态从 state.json 恢复（托盘的「暂停」跨重启保持）
    pub fn new(d: &Deps) -> Arc<Self> {
        let host: Arc<dyn Host> = Arc::new(AppHost::new(
            d.app.clone(),
            Arc::clone(&d.settings),
            Arc::clone(&d.perf),
        ));
        let port: Arc<dyn Port> = Arc::clone(&d.platform) as Arc<dyn Port>;
        let exec: Arc<dyn Exec> = Arc::new(PoolExec);
        let paused = d.settings.state().read(|s| s.clipboard_paused);
        let engine = Engine::new(
            port,
            host,
            exec,
            OsKind::current(),
            d.settings.paths().data_dir.clone(),
            paused,
        );
        assets::register(&d.assets, &engine);
        Arc::new(Self {
            app: d.app.clone(),
            settings: Arc::clone(&d.settings),
            bus: d.bus.clone(),
            engine,
            started: AtomicBool::new(false),
        })
    }

    /// 按设置安装监听（监听开关打开且没有暂停时）；不打开数据库。同时启动监听工作任务、空闲清理调度与设置订阅
    /// （只启动一次）。监听装不上（Wayland 没有 data-control、X11 缺 XFIXES）时返回错误，其余功能照常
    pub fn start(&self) -> Result<(), IpcError> {
        if !self.started.swap(true, Ordering::AcqRel) {
            self.spawn_tasks();
        }
        self.engine.apply_settings_now(&self.settings.get().clipboard);
        self.engine.reconcile_watch()
    }

    fn spawn_tasks(&self) {
        tauri::async_runtime::spawn(watch::run(Arc::clone(&self.engine)));
        // 启动后第一次空闲的孤儿 blob 清理与保留策略由数据库打开的那一刻排（清理从不为自己打开数据库）
        tauri::async_runtime::spawn(retention::run(Arc::clone(&self.engine)));
        let engine = Arc::clone(&self.engine);
        let mut rx = self.settings.subscribe();
        rx.borrow_and_update();
        tauri::async_runtime::spawn(async move {
            while rx.changed().await.is_ok() {
                let clipboard = rx.borrow_and_update().clipboard.clone();
                engine.apply_settings(&clipboard);
            }
        });
    }

    /// 托盘就绪且 Panel 预热完成后的空闲时刻一次性打开数据库并加载 RecentCache。**会阻塞**，bootstrap 在
    /// 阻塞线程池里调用
    pub fn preload_cache(&self) {
        self.engine.preload();
    }

    /// 设置变化时调整监听与记录策略（服务自己订阅了设置，通常不需要外部调用）。不阻塞
    pub fn apply_settings(&self, s: &ClipboardSettings) {
        self.engine.apply_settings(s);
    }

    /// 暂停 / 恢复记录（托盘开关；暂停时完全注销监听）。不阻塞：状态立即生效并写入 state.json，
    /// 监听的注销 / 重新安装在后台进行；恢复时不补录暂停期间的复制
    pub fn set_paused(&self, paused: bool) {
        self.engine.set_paused(paused);
    }

    /// 是否已暂停
    pub fn paused(&self) -> bool {
        self.engine.paused()
    }

    /// 历史不可用的原因（i18n key，`ClipboardPayload.unavailable`）与监听状态（`ClipboardPayload.recording`）。不阻塞、
    /// 不触发打开数据库
    pub fn history_state(&self) -> (Option<&'static str>, wisp_types::ClipboardRecording) {
        (self.engine.history_issue(), self.engine.recording_state())
    }

    /// 当前剪贴板里的内容已经记进历史（正在记录，且最近记录的序号就是现在的序号）。取词的模拟复制据此决定备份失败时
    /// 能否放弃恢复。**会阻塞**（读一次平台序号，微秒级）
    pub fn current_content_recorded(&self) -> bool {
        self.engine.current_content_recorded()
    }

    /// 首页（来自内存缓存，不查库）。只有在预加载完成之前就唤出面板时会先打开数据库（阻塞约十几毫秒）；页面里有文件
    /// 条目时最多再等 5 ms 的 exists 检查（只 stat）。编排器在阻塞线程上调用
    pub fn first_page(&self) -> ClipPage {
        self.engine.first_page()
    }

    /// 分页
    pub async fn recent(&self, offset: u32, limit: u32, f: ClipFilter) -> Result<ClipPage, IpcError> {
        self.blocking(move |e| e.recent(offset, limit, f)).await?
    }

    /// 搜索
    pub async fn search(&self, q: String, f: ClipFilter, limit: u32) -> Result<ClipPage, IpcError> {
        self.blocking(move |e| e.search(&q, f, limit)).await?
    }

    /// 详情（Space 预览）
    pub async fn detail(&self, id: i64) -> Result<Option<ClipItemDetail>, IpcError> {
        self.blocking(move |e| e.detail(id)).await?
    }

    /// 统一的粘贴管线（`paste.rs`）：按来源写入（带自写抑制）→ touch 来源条目 → 按宿主交接 → 发键。面板宿主立即隐藏面板
    /// （无退出动画，等窗口离屏 ≤ 100 ms）、只复制时弹 toast；悬浮窗不隐藏、先交还键盘焦点；按键发出后记
    /// `perf.paste(method, selected_at, …)`。
    ///
    /// 单飞：粘贴进行中再来的请求、同一面板会话的第二次粘贴、面板已不显示剪贴板视图时的请求都返回 `conflict`
    /// （不写入、不 touch、不 toast），前端静默忽略即可
    pub async fn paste(&self, req: PasteRequest) -> Result<PasteOutcome, IpcError> {
        self.blocking(move |e| e.paste(req)).await?
    }

    /// 收藏 / 取消收藏
    pub async fn pin(&self, id: i64, pinned: bool) -> Result<ClipItemMeta, IpcError> {
        self.blocking(move |e| e.pin(id, pinned)).await?
    }

    /// 给收藏命名（`name` 已清理；None 清除名称）。命名即收藏
    pub async fn set_name(&self, id: i64, name: Option<String>) -> Result<ClipItemMeta, IpcError> {
        self.blocking(move |e| e.set_name(id, name.as_deref())).await?
    }

    /// 条目的检视：识别出的片段与可用的「粘贴为…」变换（最近 8 条有缓存）
    pub async fn inspect(&self, id: i64) -> Result<ClipInspect, IpcError> {
        self.blocking(move |e| e.inspect(id)).await?
    }

    /// 纯文本的检视（识别结果等，按当前的识别设置）。识别只扫前 16 KB，JSON 变换要看整段：放进阻塞线程池
    pub async fn text_inspect(&self, text: String) -> Result<ClipInspect, IpcError> {
        let smart = self.settings.get().clipboard.smart;
        tauri::async_runtime::spawn_blocking(move || super::smart::inspect_text(&text, &smart))
            .await
            .map_err(|e| IpcError::internal(format!("inspect task failed: {e}")))
    }

    /// 剪贴板视图的载荷（面板打开、悬浮剪贴板就绪时）：先取首页（可能顺带打开数据库），再读历史状态——打开失败 / 库来自
    /// 更新版本时横幅随这一次载荷就出现。**会阻塞**（首页可能要打开数据库）
    pub fn payload(&self, settings: &wisp_types::Settings) -> ClipboardPayload {
        let page = self.first_page();
        let (unavailable, recording) = self.history_state();
        ClipboardPayload {
            page,
            paused: self.paused(),
            hint_bar: settings.clipboard.show_hint_bar,
            paste_on_double_click: settings.clipboard.paste_on == PasteOn::DoubleClick,
            unavailable: unavailable.map(str::to_owned),
            recording,
            favorites_collapsed: self.favorites_collapsed(),
            queue_unavailable: self.engine.queue_unavailable(),
        }
    }

    /// 拖出条目时的拖拽数据（`drag.rs`）
    pub async fn drag_data(&self, ids: Vec<i64>) -> Result<ClipDragData, IpcError> {
        self.blocking(move |e| e.drag_data(&ids)).await?
    }

    /// 连续粘贴（`queue.rs`）：立即经调用窗口的宿主粘贴第一条，之后每按一次粘贴键粘贴下一条
    pub async fn queue_start(
        &self,
        ids: Vec<i64>,
        host: PasteHost,
        selected_at: std::time::Instant,
    ) -> Result<QueueStatus, IpcError> {
        self.blocking(move |e| e.queue_start(ids, host, selected_at)).await?
    }

    /// 结束连续粘贴（没有进行中的队列时什么都不做）
    pub async fn queue_cancel(&self) -> Result<(), IpcError> {
        self.blocking(|e| e.queue_end(QueueEnd::Cancelled)).await
    }

    /// 结束连续粘贴，不等它卸载完（托盘菜单：在主线程上调用，不能阻塞）
    pub fn queue_cancel_later(&self) {
        let engine = Arc::clone(&self.engine);
        drop(tauri::async_runtime::spawn_blocking(move || {
            engine.queue_end(QueueEnd::Cancelled)
        }));
    }

    /// 全局 Esc：有进行中的连续粘贴就结束它，返回是否处理了。**会阻塞**（卸载钩子）
    pub fn queue_escape(&self) -> bool {
        self.engine.queue_escape()
    }

    /// 连续粘贴的进度（托盘状态行用；没有进行中的队列时为 None）。不阻塞
    pub fn queue_status(&self) -> Option<QueueStatus> {
        self.engine.queue_status()
    }

    /// 列表顶部的「收藏」分组是否折叠（本机状态，state.json）
    pub fn favorites_collapsed(&self) -> bool {
        self.settings.state().read(|s| s.clipboard_favorites_collapsed)
    }

    /// 折叠 / 展开「收藏」分组：合并写盘（连点几次只写一次）
    pub fn set_favorites_collapsed(&self, collapsed: bool) {
        self.settings
            .state_arc()
            .update_deferred(move |s| s.clipboard_favorites_collapsed = collapsed);
    }

    /// 删除；置顶项要求 force（前端负责二次确认）
    pub async fn delete(&self, id: i64, force: bool) -> Result<(), IpcError> {
        self.blocking(move |e| e.delete(id, force)).await?
    }

    /// 清空，返回删除条数
    pub async fn clear(&self, keep_pinned: bool) -> Result<u32, IpcError> {
        self.blocking(move |e| e.clear(keep_pinned)).await?
    }

    /// 原生「另存为」对话框保存图片；用户取消时返回 None
    pub async fn save_image(&self, id: i64) -> Result<Option<String>, IpcError> {
        let detail = self.require_detail(id).await?;
        let (Some(key), Some(image)) = (
            detail.image_url.as_deref().and_then(|u| u.strip_prefix("img/")),
            detail.meta.image.as_ref(),
        ) else {
            return Err(IpcError::invalid_argument("clipboard item is not an image"));
        };
        let store = self
            .engine
            .ready_store()
            .ok_or_else(|| IpcError::not_found("clipboard history unavailable"))?;
        let source = store.blob_path(BlobKind::Img, key);
        let lang = crate::app::i18n::resolve(self.settings.get().general.ui_language);
        let file_name = format!("wisp-image-{}x{}.png", image.width, image.height);
        let Some(dest) = crate::app::files::save_dialog(
            &self.app,
            texts::tr(lang, Text::SaveImageTitle),
            &file_name,
            texts::tr(lang, Text::ImageFilter),
            &["png"],
        )
        .await?
        else {
            return Ok(None);
        };
        let dest = with_png_extension(dest);
        let target = dest.clone();
        tauri::async_runtime::spawn_blocking(move || std::fs::copy(&source, &target))
            .await
            .map_err(|e| IpcError::internal(format!("save image task failed: {e}")))??;
        log::info!("clipboard image saved id={id}");
        Ok(Some(dest.to_string_lossy().into_owned()))
    }

    /// 在文件管理器中显示文件（第一个仍然存在的文件；只 stat，不读内容）
    pub async fn reveal(&self, id: i64) -> Result<(), IpcError> {
        let detail = self.require_detail(id).await?;
        let files = detail
            .files
            .filter(|f| !f.is_empty())
            .ok_or_else(|| IpcError::invalid_argument("clipboard item has no files"))?;
        let app = self.app.clone();
        // stat 与外壳调用（Linux 走 D-Bus、Windows 走 COM）都可能阻塞：放进阻塞线程池
        tauri::async_runtime::spawn_blocking(move || {
            let path = files
                .into_iter()
                .find(|f| f.exists && Path::new(&f.path).exists())
                .map(|f| f.path)
                .ok_or_else(|| IpcError::not_found("files no longer exist"))?;
            app.opener()
                .reveal_item_in_dir(&path)
                .map_err(|e| IpcError::new(IpcErrorCode::Platform, format!("cannot reveal file: {e}")))
        })
        .await
        .map_err(|e| IpcError::internal(format!("reveal task failed: {e}")))?
    }

    /// 用默认程序打开链接（只允许常见的网页 / 邮件 / 电话等 scheme，挡住剪贴板注入的自定义协议）
    pub async fn open_link(&self, id: i64) -> Result<(), IpcError> {
        let meta = match self.engine.cached(id) {
            Some(m) => m,
            None => self.require_detail(id).await?.meta,
        };
        let link = meta
            .link
            .ok_or_else(|| IpcError::invalid_argument("clipboard item is not a link"))?;
        if !is_openable_link(&link) {
            return Err(IpcError::invalid_argument("link scheme is not allowed"));
        }
        let app = self.app.clone();
        tauri::async_runtime::spawn_blocking(move || {
            app.opener()
                .open_url(link, None::<&str>)
                .map_err(|e| IpcError::new(IpcErrorCode::Platform, format!("cannot open link: {e}")))
        })
        .await
        .map_err(|e| IpcError::internal(format!("open link task failed: {e}")))?
    }

    /// 送翻译 / 识别文字，都经总线以 `TriggerOrigin::Clipboard` 发起（编排器用 crossfade 在同一面板里显示）。
    /// - 翻译：文本条目的正文；
    /// - 识别文字：图片条目的原图，在阻塞线程里读出并解码之后交给编排器，与截图 OCR 走同一段启动流程（重新识别、
    ///   识别历史、缩略图都沿用）。原图丢了或解码失败时在这里返回错误，不打开面板
    pub async fn send_to(&self, id: i64, target: SendTarget) -> Result<(), IpcError> {
        match target {
            SendTarget::Translate => {
                let detail = self.require_detail(id).await?;
                let text = detail
                    .text
                    .filter(|t| !t.trim().is_empty())
                    .ok_or_else(|| IpcError::invalid_argument("clipboard item has no text"))?;
                let text = truncate_at_char_boundary(text, crate::ipc::MAX_TEXT_BYTES);
                self.bus.send(Command::TranslateText {
                    text,
                    origin: TriggerOrigin::Clipboard,
                });
                Ok(())
            }
            SendTarget::Ocr => {
                let detail = self.require_detail(id).await?;
                let key = detail
                    .image_url
                    .as_deref()
                    .and_then(|u| u.strip_prefix("img/"))
                    .ok_or_else(|| IpcError::invalid_argument("clipboard item is not an image"))?;
                let store = self
                    .engine
                    .ready_store()
                    .ok_or_else(|| IpcError::not_found("clipboard history unavailable"))?;
                let path = store.blob_path(BlobKind::Img, key);
                let image = tauri::async_runtime::spawn_blocking(move || {
                    let bytes = std::fs::read(&path).map_err(|_| IpcError::not_found("clipboard image is missing"))?;
                    wisp_capture::decode_image(&bytes).map_err(|e| IpcError::internal(e.to_string()))
                })
                .await
                .map_err(|e| IpcError::internal(format!("image decode task failed: {e}")))??;
                log::info!(
                    "clipboard image sent to OCR id={id} {}x{}",
                    image.width(),
                    image.height()
                );
                self.bus.send(Command::OcrImage {
                    image,
                    origin: TriggerOrigin::Clipboard,
                });
                Ok(())
            }
        }
    }

    /// OCR 自动复制、复制按钮兜底；会正常进入历史（来源记为 Wisp）。**会阻塞**（同步写系统剪贴板，Windows 最长约 1.5 s），
    /// 在阻塞线程上调用。KDE Wayland 等写不了时原样返回平台错误（裸 key，前端 `useCopy` 据此退回 `navigator.clipboard`）
    pub fn copy_text(&self, text: &str) -> Result<(), IpcError> {
        self.engine.copy_text(text)
    }

    /// 供 OCR 历史使用。数据库还没打开时打开它（**会阻塞**）；历史不可用时为 None
    pub fn store(&self) -> Option<Arc<wisp_clipboard::ClipStore>> {
        self.engine.open_store("ocr_history").ok()
    }

    /// OCR 历史列表
    pub async fn ocr_history_list(&self, offset: u32, limit: u32) -> Result<Vec<OcrHistoryItem>, IpcError> {
        self.blocking(move |e| {
            e.open_store("ocr_history")?
                .ocr_history_list(offset, limit)
                .map_err(store_error)
        })
        .await?
    }

    /// 删除一条 OCR 历史，推送 `ocr_history:changed` 的 `removed`
    pub async fn ocr_history_delete(&self, id: i64) -> Result<(), IpcError> {
        self.blocking(move |e| e.ocr_history_delete(id)).await?
    }

    /// 清空 OCR 历史：数据表与 blobs 一起清理，推送 `cleared`
    pub async fn ocr_history_clear(&self) -> Result<(), IpcError> {
        self.blocking(|e| e.ocr_history_clear()).await?
    }

    /// 记一条 OCR 历史：`ocr.history` 开启时写入、按 `ocr.history_limit` 裁剪并推送
    /// `ocr_history:changed`；关闭时什么都不做。**会阻塞**（编排器在阻塞线程上调用），返回新记录的 id
    pub fn ocr_history_record(&self, rec: &super::ocr_history::OcrRecord) -> Option<i64> {
        match self.engine.ocr_history_record(rec) {
            Ok(id) => id,
            Err(e) => {
                log::warn!("record ocr history failed: {}", e.message);
                None
            }
        }
    }

    // ---------- 多设备同步（见 `sync.rs`） ----------

    /// 安装 / 卸载本机改动的同步通知（同步开启 / 关闭时调用）。不阻塞
    pub fn set_sync_sink(&self, sink: Option<Arc<dyn super::sync::ClipSyncSink>>) {
        self.engine.set_sync_sink(sink);
    }

    /// 已打开的数据库，供同步只读地导出（`sync_rows` / `sync_export` / `ocr_sync_rows` / `ocr_sync_export*`）。
    /// **不触发打开**：数据库还没打开时为 None，同步应稍后再试，而不是为自己打开数据库
    pub fn sync_store(&self) -> Option<Arc<wisp_clipboard::ClipStore>> {
        self.engine.ready_store()
    }

    /// 应用一批远端剪贴板改动，结果与输入一一对应。**会阻塞**（图片解码、数据库），只在阻塞线程上调用
    /// （同步引擎的 `LocalStore` 实现就在阻塞线程上）
    pub fn apply_remote_batch_blocking(
        &self,
        changes: Vec<super::sync::ClipRemoteChange>,
        superseded: Option<&super::sync::Superseded>,
    ) -> Vec<super::sync::RemoteOutcome> {
        self.engine.apply_remote_batch(changes, superseded)
    }

    /// [`Self::apply_remote_batch_blocking`] 的异步包装（在阻塞线程池里执行）
    pub async fn apply_remote_batch(
        &self,
        changes: Vec<super::sync::ClipRemoteChange>,
        superseded: Option<super::sync::Superseded>,
    ) -> Result<Vec<super::sync::RemoteOutcome>, IpcError> {
        self.blocking(move |e| e.apply_remote_batch(changes, superseded.as_ref()))
            .await
    }

    /// 应用远端的「清空」（同步层没有记录的条目按时间判断）。**会阻塞**
    pub fn apply_remote_clear_blocking(&self, clear: &super::sync::ClipRemoteClear) -> super::sync::RemoteOutcome {
        self.engine.apply_remote_clear(clear)
    }

    /// [`Self::apply_remote_clear_blocking`] 的异步包装
    pub async fn apply_remote_clear(
        &self,
        clear: super::sync::ClipRemoteClear,
    ) -> Result<super::sync::RemoteOutcome, IpcError> {
        self.blocking(move |e| e.apply_remote_clear(&clear)).await
    }

    /// 应用一批远端识别历史改动，结果与输入一一对应。**会阻塞**
    pub fn apply_remote_ocr_blocking(
        &self,
        changes: Vec<super::sync::OcrRemoteChange>,
        superseded: Option<&super::sync::OcrSuperseded>,
    ) -> Vec<super::sync::RemoteOutcome> {
        self.engine.apply_remote_ocr(changes, superseded)
    }

    /// [`Self::apply_remote_ocr_blocking`] 的异步包装
    pub async fn apply_remote_ocr(
        &self,
        changes: Vec<super::sync::OcrRemoteChange>,
        superseded: Option<super::sync::OcrSuperseded>,
    ) -> Result<Vec<super::sync::RemoteOutcome>, IpcError> {
        self.blocking(move |e| e.apply_remote_ocr(changes, superseded.as_ref()))
            .await
    }

    /// 正在运行的应用（`apps_running` 命令）：平台
    /// `AppInfo::user_apps` → 去掉 Wisp 自己、按 id 去重 → 补图标（没缓存的向平台取一次并落盘）→ 按名称排序（`running.rs`）
    pub async fn apps_running(&self) -> Result<Vec<SourceApp>, IpcError> {
        self.blocking(|e| e.apps_running()).await
    }

    async fn require_detail(&self, id: i64) -> Result<ClipItemDetail, IpcError> {
        self.detail(id)
            .await?
            .ok_or_else(|| IpcError::not_found("clipboard item not found"))
    }

    /// 在阻塞线程池里执行
    async fn blocking<T: Send + 'static>(&self, f: impl FnOnce(&Engine) -> T + Send + 'static) -> Result<T, IpcError> {
        let engine = Arc::clone(&self.engine);
        tauri::async_runtime::spawn_blocking(move || f(&engine))
            .await
            .map_err(|e| IpcError::internal(format!("clipboard task failed: {e}")))
    }
}

/// 链接能否交给系统打开：常见的网页 / 邮件 / 电话 / 磁力链接，且不含空白与控制字符。
/// 自定义协议（`ms-msdt:`、`vscode:` 等）一律拒绝：剪贴板内容可能由网页注入
pub(crate) fn is_openable_link(link: &str) -> bool {
    const SCHEMES: [&str; 7] = ["https://", "http://", "mailto:", "tel:", "sms:", "magnet:?", "ftp://"];
    if link.is_empty() || link.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return false;
    }
    let lower = link.to_ascii_lowercase();
    SCHEMES.iter().any(|s| lower.starts_with(s) && lower.len() > s.len())
}

/// 用户没写扩展名时补上 `.png`
fn with_png_extension(path: PathBuf) -> PathBuf {
    if path.extension().is_some() {
        path
    } else {
        path.with_extension("png")
    }
}

/// 截到不超过 `max` 字节（落在字符边界上）
fn truncate_at_char_boundary(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_common_link_schemes_open() {
        for ok in [
            "https://example.com",
            "http://例子.测试/路径",
            "mailto:a@b.co",
            "tel:+8613800000000",
            "magnet:?xt=urn:btih:abc",
            "HTTPS://EXAMPLE.COM",
        ] {
            assert!(is_openable_link(ok), "{ok}");
        }
        for bad in [
            "",
            "https://",
            "javascript://%0aalert(1)",
            "ms-msdt://id",
            "file:///etc/passwd",
            "vscode://file/x",
            "https://a.com/x y",
            "https://a.com\n",
        ] {
            assert!(!is_openable_link(bad), "{bad}");
        }
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate_at_char_boundary("中文".to_owned(), 4), "中");
        assert_eq!(truncate_at_char_boundary("abc".to_owned(), 8), "abc");
        assert_eq!(with_png_extension(PathBuf::from("/tmp/a")), PathBuf::from("/tmp/a.png"));
        assert_eq!(
            with_png_extension(PathBuf::from("/tmp/a.PNG")),
            PathBuf::from("/tmp/a.PNG")
        );
    }
}
