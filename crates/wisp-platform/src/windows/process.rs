//! 进程与窗口的身份信息：exe 路径、显示名（版本资源里的 FileDescription）、`AppRef` 构造、UWP 宿主解析、
//! 提权判定、shell 窗口识别与运行中应用列表。

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::OnceLock;

use ::windows::Win32::Foundation::{HANDLE, HWND, LPARAM};
use ::windows::Win32::Graphics::Dwm::{DWM_CLOAKED_SHELL, DWMWA_CLOAKED, DwmGetWindowAttribute};
use ::windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use ::windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use ::windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use ::windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use ::windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GA_ROOTOWNER, GW_OWNER, GWL_EXSTYLE, GetAncestor, GetClassNameW, GetWindow,
    GetWindowLongW, GetWindowThreadProcessId, IsWindowVisible, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};
use ::windows::core::{BOOL, PCWSTR, PWSTR};
use parking_lot::Mutex;
use wisp_types::AppRef;

use super::util::{OwnedHandle, RawHwnd, from_wide_buf, wide};

/// 窗口所属的线程与进程 id
pub(crate) fn window_thread_process(hwnd: HWND) -> (u32, u32) {
    let mut pid = 0u32;
    // SAFETY: 纯查询；输出参数指向局部变量
    let tid = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    (tid, pid)
}

/// 顶层所有者窗口（`GA_ROOTOWNER`）；取不到时返回原窗口
pub(crate) fn root_owner(hwnd: HWND) -> HWND {
    // SAFETY: 纯查询
    let root = unsafe { GetAncestor(hwnd, GA_ROOTOWNER) };
    if root.0.is_null() { hwnd } else { root }
}

/// 窗口类名
pub(crate) fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: 缓冲区长度由切片给出
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    let n = usize::try_from(n).unwrap_or(0).min(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// 进程的 exe 完整路径（`PROCESS_QUERY_LIMITED_INFORMATION` 对提权进程也有效，受保护进程除外）
pub(crate) fn exe_path(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    // SAFETY: 只申请受限查询权限；句柄由守卫关闭
    let handle = OwnedHandle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?);
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: 缓冲区与长度匹配；成功时 len 为写入的字符数（不含 NUL）
    unsafe { QueryFullProcessImageNameW(handle.0, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) }.ok()?;
    let len = (len as usize).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// 用 ToolHelp 快照按 pid 取 exe 文件名（打不开进程时的退路）
fn exe_name_by_snapshot(pid: u32) -> Option<String> {
    for_each_process(|entry_pid, name| (entry_pid == pid).then(|| name.to_owned()))
}

/// 遍历进程快照，`f` 返回 Some 时提前结束并返回该值
fn for_each_process<T>(mut f: impl FnMut(u32, &str) -> Option<T>) -> Option<T> {
    // SAFETY: 创建进程快照；句柄由守卫关闭
    let snap = OwnedHandle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?);
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: entry.dwSize 已设置；快照句柄有效
    let mut ok = unsafe { Process32FirstW(snap.0, &mut entry) }.is_ok();
    while ok {
        let name = from_wide_buf(&entry.szExeFile);
        if let Some(v) = f(entry.th32ProcessID, &name) {
            return Some(v);
        }
        // SAFETY: 同上
        ok = unsafe { Process32NextW(snap.0, &mut entry) }.is_ok();
    }
    None
}

/// 运行中的应用（按 exe 名去重；邻居探测用，一次性调用）
pub(crate) fn running_apps() -> Vec<AppRef> {
    let mut seen: HashMap<String, AppRef> = HashMap::new();
    let _: Option<()> = for_each_process(|pid, name| {
        if pid > 4 && !name.is_empty() {
            let id = name.to_lowercase();
            seen.entry(id.clone()).or_insert_with(|| AppRef {
                name: stem(name).to_owned(),
                id,
                alt_id: None,
                pid: Some(pid),
                path: None,
                window: None,
            });
        }
        None
    });
    let mut apps: Vec<AppRef> = seen.into_values().collect();
    apps.sort_by(|a, b| a.id.cmp(&b.id));
    apps
}

/// 窗口是否会出现在任务栏 / Alt-Tab 里（排除名单选择器只列这些进程，`AppInfo::user_apps`）：可见；没有被 DWM
/// 隐藏（UWP 挂起后残留的框架、应用自己隐藏的窗口；在别的虚拟桌面上的窗口也被隐藏，但原因是 `DWM_CLOAKED_SHELL`，
/// 照样算）；不是工具窗口 / 不激活窗口、没有所有者——除非显式带 `WS_EX_APPWINDOW`。全是纯查询，不向目标窗口发消息
fn is_taskbar_window(hwnd: HWND) -> bool {
    // SAFETY: 纯查询
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return false;
    }
    let mut cloaked = 0u32;
    // SAFETY: 输出缓冲区是 u32，长度如实传入；失败（老系统、窗口刚销毁）按没有隐藏处理
    let got = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut c_void,
            std::mem::size_of::<u32>() as u32,
        )
    };
    if got.is_ok() && cloaked != 0 && cloaked != DWM_CLOAKED_SHELL {
        return false;
    }
    // SAFETY: 纯查询
    let ex = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
    if ex & WS_EX_APPWINDOW.0 != 0 {
        return true;
    }
    // SAFETY: 纯查询
    let owned = unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|o| !o.0.is_null());
    !owned && ex & (WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) == 0
}

/// 系统目录（`%SystemRoot%`，通常是 `C:\Windows`；小写、带结尾的反斜杠）。其下的进程不作为「后台应用」列出
/// （ctfmon、sihost、RuntimeBroker、驱动商店里的音频 / 显卡托盘……）；有任务栏窗口的照样列出（[`user_apps`]）
fn system_root() -> &'static str {
    static ROOT: OnceLock<String> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = std::env::var("SystemRoot")
            .or_else(|_| std::env::var("windir"))
            .unwrap_or_else(|_| r"C:\Windows".to_owned());
        let mut root = root.trim_end_matches(['\\', '/']).to_lowercase();
        root.push('\\');
        root
    })
}

/// 路径是否在 `root`（小写、带结尾分隔符）之下：不区分大小写，`/` 与 `\` 等价
fn path_is_under(path: &str, root: &str) -> bool {
    let path = path.to_lowercase().replace('/', "\\");
    path.starts_with(root)
}

/// 只为别的应用服务、用户不会把它当成一个应用去排除的进程：WebView2 / 显卡容器这类运行时，以及名字看得出是辅助进程的
/// （exe 名去掉扩展名与结尾的位数后以 helper / agent / daemon / updater / update / handler / crashreporter / service /
/// broker 结尾——与 macOS `user_apps` 的辅助进程口径一致）。只用于「后台应用」；有任务栏窗口
/// 的进程不经这道过滤
fn is_helper_exe(exe: &str) -> bool {
    const RUNTIMES: &[&str] = &["msedgewebview2", "applicationframehost", "nvcontainer"];
    const SUFFIXES: &[&str] = &[
        "helper",
        "agent",
        "daemon",
        "updater",
        "update",
        "handler",
        "crashreporter",
        "service",
        "broker",
    ];
    let lower = stem(exe).to_lowercase();
    // RtkAudUService64、Foo_x64、Bar-64 之类：位数 / 架构后缀不算名字
    let mut name = lower.trim_end_matches(|c: char| c.is_ascii_digit());
    for arch in ["_x", "-x", " x", "x"] {
        if lower.len() > name.len()
            && let Some(rest) = name.strip_suffix(arch)
        {
            name = rest;
            break;
        }
    }
    let name = name.trim_end_matches(['_', '-', ' ', '.']);
    RUNTIMES.contains(&lower.as_str()) || RUNTIMES.contains(&name) || SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// 「后台应用」：在当前桌面上有顶层窗口（隐藏的也算——最小化到托盘的应用把主窗口隐藏起来，托盘图标本身也要一个窗口来
/// 接收消息）、却没有任务栏窗口的进程，只保留可执行文件不在系统目录下、名字不是辅助进程 / 运行时的（[`is_helper_exe`]）
fn is_background_app(path: &str) -> bool {
    !path_is_under(path, system_root()) && !is_helper_exe(file_name(path))
}

/// 排除名单选择器用的应用列表（`AppInfo::user_apps`）。按 Z 序枚举当前桌面的
/// 顶层窗口（含隐藏的），分两批：
/// 1. 有任务栏窗口（[`is_taskbar_window`]）的应用：不是 shell 自身的那些，UWP 换成真正的应用进程（`app_for_window`）；
///    系统目录下的进程只要有任务栏窗口（记事本、远程桌面、任务管理器）照样列出——它们确实是用户在用的应用；
/// 2. 没有任务栏窗口、只有隐藏 / 工具窗口的「后台应用」（[`is_background_app`]）：最小化到托盘的 KeePass / Bitwarden、
///    托盘常驻的 Ditto / Snipaste / PixPin——正是用户最想**提前**加进排除名单的一类（「最近来源」只有复制过之后才有）。它们没有可激活的窗口，`AppRef.window` 为 None。
///
/// 按 exe 名去重（任务栏那一批在前）。全是纯查询，不向目标窗口发消息
pub(crate) fn user_apps(names: &NameCache, own_pid: u32) -> Vec<AppRef> {
    unsafe extern "system" fn visit(hwnd: HWND, data: LPARAM) -> BOOL {
        // SAFETY: data 指向下面的局部 Vec，枚举同步进行
        let found = unsafe { &mut *(data.0 as *mut Vec<(RawHwnd, bool)>) };
        found.push((RawHwnd::new(hwnd), is_taskbar_window(hwnd)));
        BOOL(1)
    }
    let mut windows: Vec<(RawHwnd, bool)> = Vec::new();
    // SAFETY: 回调只在本调用期间同步执行；LPARAM 指向局部变量
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(&mut windows as *mut Vec<(RawHwnd, bool)> as isize));
    }
    let mut out: Vec<AppRef> = Vec::new();
    // 第 1 批：任务栏窗口
    let mut listed_pids: Vec<u32> = Vec::new();
    for (raw, _) in windows.iter().filter(|(_, taskbar)| *taskbar) {
        let hwnd = raw.hwnd();
        let (_, pid) = window_thread_process(hwnd);
        if pid == 0 || pid == own_pid || is_shell_window(hwnd, pid) {
            continue;
        }
        let Some(app) = names.app_for_window(hwnd) else {
            continue;
        };
        listed_pids.push(pid);
        // UWP 框架里的应用已挂起、找不到真正的进程时，app 仍是宿主本身：它不是一个可排除的应用
        if app.pid == Some(own_pid)
            || app.id.eq_ignore_ascii_case("ApplicationFrameHost.exe")
            || out.iter().any(|a| a.id == app.id)
        {
            continue;
        }
        out.push(app);
    }
    // 第 2 批：只有隐藏 / 工具窗口的后台应用（每个进程只看一次）
    let mut seen_pids = listed_pids;
    for (raw, _) in &windows {
        let (_, pid) = window_thread_process(raw.hwnd());
        if pid <= 4 || pid == own_pid || seen_pids.contains(&pid) {
            continue;
        }
        seen_pids.push(pid);
        // 打不开的进程（受保护进程、别的会话）不列：它们不是用户的应用
        let Some(path) = exe_path(pid) else {
            continue;
        };
        if !is_background_app(&path) {
            continue;
        }
        let Some(app) = names.app_for_pid(pid, Some(path)) else {
            continue;
        };
        if !out.iter().any(|a| a.id == app.id) {
            out.push(app);
        }
    }
    out
}

/// 文件名（不含目录）
pub(crate) fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// 去掉扩展名
pub(crate) fn stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

/// 某进程是否以更高的完整性级别运行，使本进程的 `SendInput` 会被 UIPI 拦截。
/// 本进程自己已提权时恒为 false；打不开目标进程时按已提权处理
pub(crate) fn is_elevated(pid: u32) -> bool {
    if own_elevated() {
        return false;
    }
    if pid == 0 {
        return false;
    }
    // SAFETY: 只申请受限查询权限；句柄由守卫关闭
    match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(h) => {
            let h = OwnedHandle(h);
            token_elevated(h.0).unwrap_or(true)
        }
        Err(_) => true,
    }
}

fn own_elevated() -> bool {
    static OWN: OnceLock<bool> = OnceLock::new();
    // SAFETY: 伪句柄，无需关闭
    *OWN.get_or_init(|| token_elevated(unsafe { GetCurrentProcess() }).unwrap_or(false))
}

fn token_elevated(process: HANDLE) -> Option<bool> {
    let mut token = HANDLE::default();
    // SAFETY: 输出参数指向局部变量；成功后由守卫关闭
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.ok()?;
    let token = OwnedHandle(token);
    let mut elevation = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    // SAFETY: 输出缓冲区是 TOKEN_ELEVATION，长度如实传入
    unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    }
    .ok()?;
    Some(elevation.TokenIsElevated != 0)
}

/// shell 自身的窗口（任务栏、桌面、开始菜单、搜索、Alt-Tab 等）：它们成为前台时不算「用户的应用」
pub(crate) fn is_shell_window(hwnd: HWND, pid: u32) -> bool {
    const SHELL_CLASSES: &[&str] = &[
        "Shell_TrayWnd",
        "Shell_SecondaryTrayWnd",
        "Progman",
        "WorkerW",
        "NotifyIconOverflowWindow",
        "TopLevelWindowForOverflowXamlIsland",
        "XamlExplorerHostIslandWindow",
        "MultitaskingViewFrame",
        "ForegroundStaging",
        "TaskSwitcherWnd",
        "TaskListThumbnailWnd",
        "Shell_InputSwitchTopLevelWindow",
        "Shell_Flyout",
        "SysShadow",
    ];
    const SHELL_EXES: &[&str] = &[
        "shellexperiencehost.exe",
        "startmenuexperiencehost.exe",
        "searchhost.exe",
        "searchapp.exe",
        "searchui.exe",
        "textinputhost.exe",
        "lockapp.exe",
        "shellhost.exe",
    ];
    let class = class_name(hwnd);
    if SHELL_CLASSES.contains(&class.as_str()) {
        return true;
    }
    if class.starts_with("Windows.UI.")
        && let Some(path) = exe_path(pid)
    {
        let exe = file_name(&path).to_lowercase();
        return SHELL_EXES.contains(&exe.as_str());
    }
    false
}

/// UWP 应用的顶层窗口属于 ApplicationFrameHost.exe，真正的应用在其子窗口（CoreWindow）的进程里
fn uwp_child_pid(frame: HWND, frame_pid: u32) -> Option<u32> {
    struct Ctx {
        frame_pid: u32,
        found: Option<u32>,
    }
    unsafe extern "system" fn visit(child: HWND, data: LPARAM) -> BOOL {
        // SAFETY: data 指向下面的局部 Ctx，枚举同步进行
        let ctx = unsafe { &mut *(data.0 as *mut Ctx) };
        let (_, pid) = window_thread_process(child);
        if pid != 0 && pid != ctx.frame_pid {
            ctx.found = Some(pid);
            return BOOL(0);
        }
        BOOL(1)
    }
    let mut ctx = Ctx { frame_pid, found: None };
    // SAFETY: 回调只在本调用期间同步执行；LPARAM 指向局部变量
    unsafe {
        let _ = EnumChildWindows(Some(frame), Some(visit), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    ctx.found
}

/// 显示名缓存（按 exe 路径；版本资源读取涉及磁盘 IO，只做一次）
pub(crate) struct NameCache {
    names: Mutex<HashMap<String, String>>,
}

impl NameCache {
    pub(crate) fn new() -> Self {
        Self {
            names: Mutex::new(HashMap::new()),
        }
    }

    /// exe 的显示名：FileDescription → ProductName → 文件名去扩展名
    pub(crate) fn display_name(&self, path: &str) -> String {
        let key = path.to_lowercase();
        if let Some(n) = self.names.lock().get(&key) {
            return n.clone();
        }
        let name = version_string(path, "FileDescription")
            .or_else(|| version_string(path, "ProductName"))
            .unwrap_or_else(|| stem(file_name(path)).to_owned());
        let mut guard = self.names.lock();
        if guard.len() >= 256 {
            guard.clear();
        }
        guard.insert(key, name.clone());
        name
    }

    /// 为窗口构造 `AppRef`（`window` 保留顶层窗口以便激活；UWP 应用的 pid / 路径取真正的应用进程）
    pub(crate) fn app_for_window(&self, hwnd: HWND) -> Option<AppRef> {
        let root = root_owner(hwnd);
        let (_, mut pid) = window_thread_process(root);
        if pid == 0 {
            return None;
        }
        let mut path = exe_path(pid);
        if path
            .as_deref()
            .is_some_and(|p| file_name(p).eq_ignore_ascii_case("ApplicationFrameHost.exe"))
            && let Some(child) = uwp_child_pid(root, pid)
        {
            pid = child;
            path = exe_path(pid);
        }
        let mut app = self.app_for_pid(pid, path)?;
        app.window = Some(RawHwnd::new(root).as_u64());
        Some(app)
    }

    /// 为进程构造 `AppRef`（`path` 可以预先给出，省一次查询）
    pub(crate) fn app_for_pid(&self, pid: u32, path: Option<String>) -> Option<AppRef> {
        let path = path.or_else(|| exe_path(pid));
        let (id, name) = match &path {
            Some(p) => (file_name(p).to_lowercase(), self.display_name(p)),
            None => {
                let exe = exe_name_by_snapshot(pid)?;
                (exe.to_lowercase(), stem(&exe).to_owned())
            }
        };
        Some(AppRef {
            id,
            alt_id: None,
            name,
            pid: Some(pid),
            path,
            window: None,
        })
    }
}

/// 读取 exe 版本资源中的字符串（先按资源自带的语言 / 代码页，再试 en-US Unicode / en-US 1252）
fn version_string(path: &str, key: &str) -> Option<String> {
    let wpath = wide(path);
    // SAFETY: 路径以 NUL 结尾
    let size = unsafe { GetFileVersionInfoSizeW(PCWSTR(wpath.as_ptr()), None) };
    if size == 0 {
        return None;
    }
    let mut block = vec![0u8; size as usize];
    // SAFETY: 缓冲区大小与 size 一致
    unsafe { GetFileVersionInfoW(PCWSTR(wpath.as_ptr()), None, size, block.as_mut_ptr() as *mut c_void) }.ok()?;

    let mut langs: Vec<(u16, u16)> = Vec::new();
    if let Some(raw) = query_value(&block, r"\VarFileInfo\Translation") {
        langs.extend(
            raw.chunks_exact(4)
                .map(|c| (u16::from_le_bytes([c[0], c[1]]), u16::from_le_bytes([c[2], c[3]]))),
        );
    }
    langs.extend([(0x0409, 0x04B0), (0x0409, 0x04E4)]);
    for (lang, cp) in langs {
        let sub = format!(r"\StringFileInfo\{lang:04x}{cp:04x}\{key}");
        if let Some(raw) = query_value(&block, &sub) {
            let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            let s = from_wide_buf(&units);
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_owned());
            }
        }
    }
    None
}

/// `VerQueryValueW` 的结果（复制成字节）。字符串值的长度以字符计，其余以字节计；这里统一按「最多到块末尾」截取
fn query_value(block: &[u8], sub: &str) -> Option<Vec<u8>> {
    let wsub = wide(sub);
    let mut ptr: *mut c_void = std::ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: block 是 GetFileVersionInfoW 填好的版本块；返回的指针指向块内部
    let ok = unsafe {
        VerQueryValueW(
            block.as_ptr() as *const c_void,
            PCWSTR(wsub.as_ptr()),
            &mut ptr,
            &mut len,
        )
    };
    if !ok.as_bool() || ptr.is_null() || len == 0 {
        return None;
    }
    let start = (ptr as usize).checked_sub(block.as_ptr() as usize)?;
    if start >= block.len() {
        return None;
    }
    let is_string = sub.starts_with(r"\StringFileInfo");
    let want = if is_string { len as usize * 2 } else { len as usize };
    let end = start.saturating_add(want).min(block.len());
    Some(block[start..end].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_helpers() {
        assert_eq!(file_name(r"C:\Program Files\App\App.exe"), "App.exe");
        assert_eq!(file_name("App.exe"), "App.exe");
        assert_eq!(stem("App.exe"), "App");
        assert_eq!(stem(".hidden"), ".hidden");
        assert_eq!(stem("noext"), "noext");
    }

    #[test]
    fn background_apps_skip_helpers_and_the_system_directory() {
        for helper in [
            "msedgewebview2.exe",
            "steamwebhelper.exe",
            "GoogleCrashHandler64.exe",
            "crashpad_handler.exe",
            "OneDriveUpdater.exe",
            "RtkAudUService64.exe",
            "WidgetService.exe",
            "Foo Helper_x64.exe",
            "NVContainer.exe",
        ] {
            assert!(is_helper_exe(helper), "{helper}");
        }
        for app in [
            "KeePass.exe",
            "Bitwarden.exe",
            "Ditto.exe",
            "Snipaste.exe",
            "PixPin.exe",
            "OneDrive.exe",
            "1Password.exe",
            "Everything64.exe",
            "x64dbg.exe",
        ] {
            assert!(!is_helper_exe(app), "{app}");
        }
        let root = r"c:\windows\";
        assert!(path_is_under(r"C:\Windows\System32\ctfmon.exe", root));
        assert!(path_is_under("C:/WINDOWS/explorer.exe", root));
        assert!(
            !path_is_under(r"C:\WindowsApps\Foo.exe", root),
            "前缀相同的别的目录不算"
        );
        assert!(!path_is_under(
            r"C:\Program Files\KeePass Password Safe 2\KeePass.exe",
            root
        ));
        assert!(system_root().ends_with('\\') && system_root() == system_root().to_lowercase());
    }

    #[test]
    fn own_process_is_identified() {
        let cache = NameCache::new();
        let app = cache.app_for_pid(std::process::id(), None).expect("own process");
        assert!(app.id.ends_with(".exe"));
        assert!(!app.name.is_empty());
        assert!(!is_elevated(std::process::id()) || own_elevated());
    }

    #[test]
    fn running_apps_contains_self() {
        let apps = running_apps();
        let exe = std::env::current_exe().expect("current exe");
        let id = exe
            .file_name()
            .and_then(|n| n.to_str())
            .expect("exe name")
            .to_lowercase();
        let me = apps.iter().find(|a| a.id == id).expect("own exe in running apps");
        let file = exe.file_name().and_then(|n| n.to_str()).expect("exe name");
        assert!(me.name.eq_ignore_ascii_case(stem(file)), "{} vs {file}", me.name);
        // 按 exe 名去重、排序
        assert!(apps.windows(2).all(|w| w[0].id < w[1].id));
    }
}
