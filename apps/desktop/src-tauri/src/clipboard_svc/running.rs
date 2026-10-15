//! 「从正在运行的应用中选择」（排除名单选择器；`apps_running` 命令）。
//!
//! 平台层 `AppInfo::user_apps` 给出用户认得出的应用（macOS 普通应用 + 顶层菜单栏应用，Windows 有任务栏窗口的进程，
//! X11 客户端列表 + 有 `.desktop` 的本用户进程；id 与剪贴板来源同一口径），这里再：去掉 Wisp 自己 → 按 id 去重（不区分
//! 大小写）→ 补图标 → 按名称排序。
//!
//! 图标：与剪贴板来源同一个缓存（`blobs/icons/`，`icon/<key>.png`）。没有缓存的在这里向平台取一次并落盘（顺带补上
//! 历史里同一应用还没有图标的条目），最多 [`ICON_WORKERS`] 个线程并行——第一次打开选择器时几十个应用约几十毫秒，之后
//! 全部命中缓存；平台取不到的记进「取不到」名单，本进程内不再重取（前端显示通用图标）。历史数据库还没打开（启动后的
//! 预加载之前、剪贴板历史停用）时不为图标打开它，图标一律为 null。

use wisp_types::{AppRef, SourceApp};

use super::engine::Engine;

/// 并行取图标的线程数（平台取图标是 IO + 解码，几毫秒一个）
const ICON_WORKERS: usize = 4;

/// 去掉 Wisp 自己、空 id，按 id 去重（不区分大小写，保留第一次出现的）
pub(crate) fn dedup_apps(apps: Vec<AppRef>, own_id: &str) -> Vec<AppRef> {
    let mut out: Vec<AppRef> = Vec::with_capacity(apps.len());
    for mut app in apps {
        let id = app.id.trim();
        if id.is_empty() || id.eq_ignore_ascii_case(own_id) || out.iter().any(|a| a.id.eq_ignore_ascii_case(id)) {
            continue;
        }
        app.id = id.to_owned();
        out.push(app);
    }
    out
}

/// 显示名：平台给的名字去掉首尾空白，空的退回 id
fn display_name(app: &AppRef) -> String {
    match app.name.trim() {
        "" => app.id.clone(),
        name => name.to_owned(),
    }
}

impl Engine {
    /// 正在运行的应用（见模块文档）。**会阻塞**（平台枚举 + 取图标），只在阻塞线程上调用
    pub(crate) fn apps_running(&self) -> Vec<SourceApp> {
        let apps = dedup_apps(self.port.apps().user_apps(), &self.host.self_app().id);
        let icons: Vec<Option<String>> = match self.ready_store() {
            None => vec![None; apps.len()],
            Some(store) => {
                let mut icons = vec![None; apps.len()];
                let chunk = apps.len().div_ceil(ICON_WORKERS).max(1);
                std::thread::scope(|scope| {
                    for (part, out) in apps.chunks(chunk).zip(icons.chunks_mut(chunk)) {
                        let store = &store;
                        scope.spawn(move || {
                            for (app, slot) in part.iter().zip(out.iter_mut()) {
                                *slot = self.ensure_icon(store, app);
                            }
                        });
                    }
                });
                icons
            }
        };
        let mut out: Vec<SourceApp> = apps
            .iter()
            .zip(icons)
            .map(|(app, icon_url)| SourceApp {
                id: app.id.clone(),
                name: display_name(app),
                icon_url,
            })
            .collect();
        out.sort_by_cached_key(|a| a.name.to_lowercase());
        out
    }
}
