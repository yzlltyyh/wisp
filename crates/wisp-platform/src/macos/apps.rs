//! 应用信息：图标、显示名、正在运行的应用（剪贴板来源、排除名单与快捷键冲突的邻居探测用）。
//!
//! 图标：`NSWorkspace.iconForFile(bundle 路径)` → 按请求尺寸选最合适的表示 → 高质量插值绘制到 size_px × size_px 的
//! sRGB 位图 → PNG。进程内按（应用, 尺寸）缓存少量结果；持久化缓存（blobs/icons）由调用方负责。

use std::collections::VecDeque;
use std::sync::Arc;

use objc2::AnyThread;
use objc2_app_kit::{NSApplicationActivationPolicy, NSBitmapImageFileType, NSBitmapImageRep, NSImage, NSWorkspace};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext, CGImageAlphaInfo,
    CGInterpolationQuality, kCGColorSpaceSRGB,
};
use objc2_foundation::{NSDictionary, NSFileManager, NSRect, NSString};
use parking_lot::Mutex;
use wisp_types::AppRef;

use super::Ctx;
use super::focus::{app_ref, running_app};
use super::util::pool;
use crate::traits::AppInfo;

/// 进程内图标缓存的条目上限（64 px PNG 约 4–10 KB）
const ICON_CACHE_CAP: usize = 48;
/// 图标边长上限（防御异常参数）
const MAX_ICON_PX: u32 = 1024;

/// AppInfo 实现
pub(crate) struct MacApps {
    ctx: Arc<Ctx>,
    icons: Mutex<VecDeque<IconEntry>>,
}

/// 图标缓存条目：(应用键, 边长, PNG)
type IconEntry = (String, u32, Arc<Vec<u8>>);

impl MacApps {
    pub(crate) fn new(ctx: Arc<Ctx>) -> Self {
        Self {
            ctx,
            icons: Mutex::new(VecDeque::new()),
        }
    }

    fn cached(&self, key: &str, size: u32) -> Option<Arc<Vec<u8>>> {
        let mut icons = self.icons.lock();
        let pos = icons.iter().position(|(k, s, _)| k == key && *s == size)?;
        // 命中的条目移到队尾（近似 LRU）
        let entry = icons.remove(pos)?;
        let png = entry.2.clone();
        icons.push_back(entry);
        Some(png)
    }

    fn remember(&self, key: String, size: u32, png: Arc<Vec<u8>>) {
        let mut icons = self.icons.lock();
        if icons.len() >= ICON_CACHE_CAP {
            icons.pop_front();
        }
        icons.push_back((key, size, png));
    }
}

/// 应用的 bundle 路径：AppRef.path → 正在运行的实例 → LaunchServices 按 bundle id 查找
fn bundle_path(app: &AppRef) -> Option<String> {
    if let Some(p) = app.path.as_deref().filter(|p| !p.is_empty()) {
        return Some(p.to_owned());
    }
    if let Some(path) = running_app(app).and_then(|r| r.bundleURL()).and_then(|u| u.path()) {
        return Some(path.to_string());
    }
    let id = NSString::from_str(&app.id);
    NSWorkspace::sharedWorkspace()
        .URLForApplicationWithBundleIdentifier(&id)
        .and_then(|u| u.path())
        .map(|p| p.to_string())
}

/// 把 NSImage 渲染成 size × size 的 PNG
fn render_png(image: &NSImage, size: u32) -> Option<Vec<u8>> {
    let side = f64::from(size);
    let mut proposed = NSRect::new(CGPoint::new(0.0, 0.0), CGSize::new(side, side));
    // SAFETY: proposed 指向有效的 NSRect；context / hints 允许为空
    let source = unsafe { image.CGImageForProposedRect_context_hints(&mut proposed, None, None) }?;
    // SAFETY: 系统导出的颜色空间名常量
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
    let px = size as usize;
    // SAFETY: data 为空时由 CG 自行分配；行字节数 = 宽 × 4
    let ctx = unsafe {
        CGBitmapContextCreate(
            std::ptr::null_mut(),
            px,
            px,
            8,
            px * 4,
            Some(&space),
            CGImageAlphaInfo::PremultipliedLast.0,
        )
    }?;
    CGContext::set_interpolation_quality(Some(&ctx), CGInterpolationQuality::High);
    CGContext::draw_image(
        Some(&ctx),
        CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(side, side)),
        Some(&source),
    );
    let out = CGBitmapContextCreateImage(Some(&ctx))?;
    let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), &out);
    let props = NSDictionary::new();
    // SAFETY: 空属性字典
    let data = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &props) }?;
    Some(data.to_vec())
}

impl AppInfo for MacApps {
    fn icon_png(&self, app: &AppRef, size_px: u32) -> Option<Vec<u8>> {
        let size = size_px.clamp(1, MAX_ICON_PX);
        let key = if app.id.is_empty() {
            app.path.clone().unwrap_or_default()
        } else {
            app.id.to_ascii_lowercase()
        };
        if let Some(png) = self.cached(&key, size) {
            return Some(png.as_ref().clone());
        }
        let png = pool(|| {
            let path = bundle_path(app)?;
            let image = NSWorkspace::sharedWorkspace().iconForFile(&NSString::from_str(&path));
            render_png(&image, size)
        })?;
        let png = Arc::new(png);
        self.remember(key, size, png.clone());
        Some(png.as_ref().clone())
    }

    fn display_name(&self, app: &AppRef) -> String {
        pool(|| {
            if let Some(name) = running_app(app).and_then(|r| r.localizedName()) {
                return name.to_string();
            }
            if let Some(path) = bundle_path(app) {
                let name = NSFileManager::defaultManager().displayNameAtPath(&NSString::from_str(&path));
                let name = name.to_string();
                let name = name.strip_suffix(".app").unwrap_or(&name).to_owned();
                if !name.is_empty() {
                    return name;
                }
            }
            if app.name.is_empty() {
                app.id.clone()
            } else {
                app.name.clone()
            }
        })
    }

    fn running_apps(&self) -> Vec<AppRef> {
        self.list(|_, _| true)
    }

    fn user_apps(&self) -> Vec<AppRef> {
        self.list(|regular, app| is_user_facing(regular, &app.id, app.path.as_deref()))
    }
}

impl MacApps {
    /// 正在运行的普通应用与菜单栏应用（regular + accessory，有 bundle id、不含自己），按 id 去重；`keep(是否 regular,
    /// AppRef)` 再筛一遍
    fn list(&self, keep: impl Fn(bool, &AppRef) -> bool) -> Vec<AppRef> {
        pool(|| {
            let workspace = NSWorkspace::sharedWorkspace();
            let mut seen = std::collections::HashSet::new();
            workspace
                .runningApplications()
                .iter()
                .filter_map(|a| {
                    let policy = a.activationPolicy();
                    let regular = policy == NSApplicationActivationPolicy::Regular;
                    let wanted = (regular || policy == NSApplicationActivationPolicy::Accessory)
                        && !a.isTerminated()
                        && a.processIdentifier() != self.ctx.own_pid
                        && a.bundleIdentifier().is_some();
                    wanted.then(|| (regular, app_ref(&a)))
                })
                .filter(|(regular, r)| keep(*regular, r))
                .map(|(_, r)| r)
                .filter(|r| seen.insert(r.id.to_ascii_lowercase()))
                .collect()
        })
    }
}

/// 排除名单选择器里列不列这个应用（`AppInfo::user_apps`）。普通应用（Dock 里有图标）总是
/// 列出；菜单栏 / 后台应用（accessory）只列「用户自己装的顶层应用」——1Password、uTools、PixPin、Bartender 这类会
/// 复制内容或需要排除的工具都在这里，而 accessory 里大量的系统代理（聚焦、控制中心、WindowManager……）、WebKit 的
/// XPC 进程、嵌在别的应用里的辅助进程（Chrome Helper、Spark Desktop Helper）用户既认不出、也不会从里面复制：
/// - 包路径必须是 `.app` 结尾、且不嵌在另一个 `.app` / `.xpc` / `.framework` 里；
/// - 不在系统目录（`/System/`、`/Library/Apple/`、`/usr/`）下，bundle id 不是 `com.apple.` 开头；
/// - 不是独立安装的辅助进程：包文件名或 bundle id 的最后一段以 helper / agent / daemon / updater /
///   autoupdate / quicklook / crashreporter / loginitem 结尾（iStat Menus Helper `com.bjango.istatmenus.agent`、
///   Markdown QuickLook 的扩展宿主），或者是 Sparkle 更新框架自己的进程（`org.sparkle-project.*`，更新时从缓存目录
///   拉起的 Autoupdate / Installer 是不嵌套的顶层 `.app`）。
///
/// 已知的漏网项：名字看不出是辅助进程、又放在顶层的 accessory 应用（只承载 Quick Look / 共享扩展、但名字不带上述后缀的
/// 宿主应用）仍会列出——它们至少是用户自己装的、认得出的应用，选进排除名单也无害
fn is_user_facing(regular: bool, bundle_id: &str, path: Option<&str>) -> bool {
    if regular {
        return true;
    }
    let Some(path) = path.map(|p| p.trim_end_matches('/')) else {
        return false;
    };
    let id = bundle_id.to_ascii_lowercase();
    if id.starts_with("com.apple.") || id.starts_with("org.sparkle-project.") {
        return false;
    }
    if ["/System/", "/Library/Apple/", "/usr/"]
        .iter()
        .any(|p| path.starts_with(p))
    {
        return false;
    }
    let Some((parent, stem)) = path.strip_suffix(".app").and_then(|p| p.rsplit_once('/')) else {
        return false;
    };
    if [".app/", ".xpc/", ".framework/", ".bundle/"]
        .iter()
        .any(|nest| parent.contains(nest) || parent.ends_with(nest.trim_end_matches('/')))
    {
        return false;
    }
    !looks_like_helper(stem) && !looks_like_helper(id.rsplit('.').next().unwrap_or(""))
}

/// 辅助进程常见的名字后缀（比较前转小写、去掉空格 / 连字符 / 下划线）
const HELPER_SUFFIXES: [&str; 8] = [
    "helper",
    "agent",
    "daemon",
    "updater",
    "autoupdate",
    "quicklook",
    "crashreporter",
    "loginitem",
];

/// 名字（包文件名，或 bundle id 的最后一段）看起来是辅助进程
fn looks_like_helper(name: &str) -> bool {
    let norm: String = name
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-' && *c != '_')
        .flat_map(char::to_lowercase)
        .collect();
    HELPER_SUFFIXES.iter().any(|s| norm.ends_with(s))
}

#[cfg(test)]
mod tests {
    use super::is_user_facing;

    #[test]
    fn user_facing_keeps_regular_and_top_level_menu_bar_apps() {
        // 普通应用总是列出（系统自带的终端、文本编辑也算）
        assert!(is_user_facing(
            true,
            "com.apple.Terminal",
            Some("/System/Applications/Utilities/Terminal.app")
        ));
        assert!(is_user_facing(true, "com.google.Chrome", None));
        // 用户装的菜单栏应用
        for (id, path) in [
            ("com.1password.1password", "/Applications/1Password.app"),
            ("org.yuanli.utools", "/Applications/uTools.app"),
            ("com.surteesstudios.Bartender", "/Applications/Bartender 6.app/"),
            ("com.example.tool", "/Users/me/Applications/Tool.app"),
            (
                "com.bjango.istatmenus.status",
                "/Users/me/Library/Application Support/iStat Menus 7/iStat Menus Menubar.app",
            ),
            // 名为 Sparkle 的菜单栏应用（bundle id 就是 `sparkle.app`），不是 Sparkle 更新框架
            ("sparkle.app", "/Applications/Sparkle.app"),
            ("com.ejbills.DockDoorPro", "/Applications/DockDoor Pro.app"),
        ] {
            assert!(is_user_facing(false, id, Some(path)), "{id}");
        }
        // 系统代理、WebKit XPC、嵌套的辅助进程、没有包路径的
        for (id, path) in [
            (
                "com.apple.Spotlight",
                Some("/System/Library/CoreServices/Spotlight.app"),
            ),
            (
                "com.apple.WebKit.GPU",
                Some(
                    "/System/Volumes/Preboot/Cryptexes/OS/System/Library/Frameworks/WebKit.framework/Versions/A/XPCServices/com.apple.WebKit.GPU.xpc",
                ),
            ),
            (
                "com.google.Chrome.helper",
                Some(
                    "/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Framework.framework/Versions/1/Helpers/Google Chrome Helper.app",
                ),
            ),
            (
                "2BUA8C4S2C.com.1password.browser-helper",
                Some("/Applications/1Password.app/Contents/Library/LoginItems/1Password Browser Helper.app"),
            ),
            (
                "com.tencent.flue.WeChatAppEx",
                Some("/Applications/微信.app/Contents/MacOS/WeChatAppEx.app"),
            ),
            ("com.example.agent", Some("/Library/Apple/System/Agent.app")),
            // 独立安装的辅助进程：名字或 bundle id 末段是 helper / agent / quicklook / updater……
            (
                "com.bjango.istatmenus.agent",
                Some("/Users/me/Library/Application Support/iStat Menus 7/iStat Menus Helper.app"),
            ),
            (
                "com.devin.markdownquicklook",
                Some("/Applications/MarkdownQuickLook.app"),
            ),
            ("com.example.tool.agent", Some("/Applications/Tool Background.app")),
            ("com.example.sync", Some("/Applications/Sync Login-Item.app")),
            // Sparkle 更新框架更新时从缓存目录拉起的顶层 Autoupdate
            (
                "org.sparkle-project.Sparkle.Autoupdate",
                Some(
                    "/Users/me/Library/Caches/com.example.app/org.sparkle-project.Sparkle/Installation/x/Autoupdate.app",
                ),
            ),
            ("com.example.daemon", Some("/usr/local/bin/daemon")),
            ("com.example.nopath", None),
            ("com.apple.dock", Some("/Applications/Dock.app")),
        ] {
            assert!(!is_user_facing(false, id, path), "{id}");
        }
    }

    /// 测量用：本机的 `user_apps` 列表规模、枚举耗时与逐个取 64 px 图标的耗时。手动运行：
    /// `cargo test -p wisp-platform --release --lib macos::apps::tests::user_apps_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "测量用，手动运行（结果依赖本机正在运行的应用）"]
    #[allow(clippy::print_stderr)]
    fn user_apps_cost() {
        use std::sync::Arc;
        use std::time::Instant;

        use crate::{Platform, PlatformConfig};

        let p = Platform::create(PlatformConfig {
            own_pid: std::process::id(),
            main_thread: Arc::new(|f: Box<dyn FnOnce() + Send>| f()),
            self_hit: Arc::new(|_, _| false),
            keymaps: Arc::new(parking_lot::RwLock::new(Vec::new())),
            os_version: "test".to_owned(),
        });
        let t = Instant::now();
        let all = p.apps.running_apps();
        let t_all = t.elapsed();
        let t = Instant::now();
        let apps = p.apps.user_apps();
        let t_user = t.elapsed();
        let t = Instant::now();
        let icons = apps.iter().filter(|a| p.apps.icon_png(a, 64).is_some()).count();
        let t_icons = t.elapsed();
        eprintln!(
            "user_apps: running_apps={} ({:?}), user_apps={} ({:?}), icons {}/{} in {:?}",
            all.len(),
            t_all,
            apps.len(),
            t_user,
            icons,
            apps.len(),
            t_icons
        );
        for a in &apps {
            eprintln!("  {} | {}", a.id, a.name);
        }
    }
}
