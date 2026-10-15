//! 邻居应用探测（冲突判定的第 4 层）：一次性列出正在运行的应用，与已知工具（Bob、Easydict、Pot、
//! Raycast、PopClip、Maccy、Paste、Snipaste、PixPin、uTools、Ditto、QQ…，数据在 `data/conflicts.json`）的默认键比对。
//!
//! 只在启动时、打开引导页时、打开快捷键页时各探测一次（`neighbor_apps` 命令），不轮询、不常驻；只提示不回退。
//! 只读取运行中的应用列表（macOS NSWorkspace / Windows 进程快照 / X11 窗口的 WM_CLASS），不读任何文件。

use std::time::{Duration, Instant};

use wisp_platform::common::combo::Combo;
use wisp_types::{AppRef, HotkeyStatus, HotkeyTarget, NeighborHit, OsKind};

use super::conflicts::{Neighbor, table};
use super::display::display;

/// 正在运行的一个已知邻居
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Running {
    pub name: String,
    /// 它的默认全局快捷键（规范串）
    pub combos: Vec<String>,
}

/// 某个运行中的应用是否就是这个邻居（bundle id / exe 名 / WM_CLASS 的 class 或 instance，不区分大小写）
fn matches(n: &Neighbor, app: &AppRef) -> bool {
    n.ids.iter().any(|id| {
        app.id.eq_ignore_ascii_case(id) || app.alt_id.as_deref().is_some_and(|alt| alt.eq_ignore_ascii_case(id))
    })
}

/// 在运行中的应用里找出已知邻居（纯函数）
pub fn find(os: OsKind, apps: &[AppRef]) -> Vec<Running> {
    let mut out: Vec<Running> = Vec::new();
    for n in table().neighbors.iter().filter(|n| n.os == os) {
        if apps.iter().any(|a| matches(n, a)) && !out.iter().any(|r| r.name == n.name) {
            out.push(Running {
                name: n.name.clone(),
                combos: n.combos.clone(),
            });
        }
    }
    out
}

/// 探测一次（阻塞：macOS 经 NSWorkspace，Windows 枚举进程快照；都在 20 ms 量级）
pub fn probe(platform: &wisp_platform::Platform) -> Vec<Running> {
    let started = Instant::now();
    let apps = platform.apps.running_apps();
    let found = find(OsKind::current(), &apps);
    log::debug!(
        "neighbor probe: {} apps, {} known neighbors in {} ms",
        apps.len(),
        found.len(),
        started.elapsed().as_millis()
    );
    found
}

/// 邻居的默认键与 Wisp 当前生效的动作快捷键重合的条目（只在对方的默认键与 Wisp 当前键重合时提示）
pub fn hits(os: OsKind, running: &[Running], statuses: &[HotkeyStatus]) -> Vec<NeighborHit> {
    let mut out = Vec::new();
    for r in running {
        for combo in &r.combos {
            let Some(theirs) = Combo::parse(combo) else { continue };
            for s in statuses {
                let HotkeyTarget::Action { action } = s.target else {
                    continue;
                };
                let Some(mine) = s.effective.as_deref().and_then(Combo::parse) else {
                    continue;
                };
                if mine == theirs {
                    out.push(NeighborHit {
                        app_name: r.name.clone(),
                        combo: combo.clone(),
                        display: display(combo, os),
                        action,
                    });
                }
            }
        }
    }
    out
}

/// 在 `at` 做的探测到 `now` 是否仍可复用（有效期 `ttl`）
pub fn fresh(at: Instant, now: Instant, ttl: Duration) -> bool {
    now.saturating_duration_since(at) < ttl
}

/// 是否有某个名字的邻居在运行（键盘建议里的 QQ 提示用）
pub fn is_running(running: &[Running], name: &str) -> bool {
    running.iter().any(|r| r.name == name)
}

#[cfg(test)]
mod tests {
    use wisp_types::{Binding, HotkeyAction, HotkeyState};

    use super::*;

    fn app(id: &str) -> AppRef {
        AppRef {
            id: id.to_owned(),
            alt_id: None,
            name: id.to_owned(),
            pid: None,
            path: None,
            window: None,
        }
    }

    fn status(action: HotkeyAction, effective: &str) -> HotkeyStatus {
        HotkeyStatus {
            target: HotkeyTarget::Action { action },
            binding: Binding::default_binding(),
            effective: Some(effective.to_owned()),
            display: None,
            state: HotkeyState::Registered,
            reason_key: None,
        }
    }

    /// 运行 Easydict 时提示 ⌥D 重合，不运行时没有
    #[test]
    fn easydict_overlaps_on_macos() {
        let statuses = [
            status(HotkeyAction::TranslateSelection, "Alt+KeyD"),
            status(HotkeyAction::Ocr, "Alt+KeyS"),
            status(HotkeyAction::Clipboard, "Alt+KeyV"),
        ];
        let running = find(OsKind::Macos, &[app("com.apple.Safari"), app("com.izual.Easydict")]);
        assert_eq!(running.len(), 1);
        let h = hits(OsKind::Macos, &running, &statuses);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].app_name, "Easydict");
        assert_eq!(h[0].action, HotkeyAction::TranslateSelection);
        assert_eq!(h[0].display, "⌥D");
        assert!(
            hits(
                OsKind::Macos,
                &find(OsKind::Macos, &[app("com.apple.Safari")]),
                &statuses
            )
            .is_empty()
        );
    }

    /// 探测结果只在短时间内复用：启动后才打开的应用（例如 QQ）在下一次检查 / 键盘建议时就能被发现
    #[test]
    fn cached_probe_expires() {
        let t0 = Instant::now();
        let ttl = Duration::from_secs(2);
        assert!(fresh(t0, t0, ttl));
        assert!(fresh(t0, t0 + Duration::from_millis(1999), ttl));
        assert!(!fresh(t0, t0 + ttl, ttl));
    }

    /// 核实过的 id：应用包里的 Info.plist 与公开来源给出的真实 bundle id / 进程名都能命中
    #[test]
    fn verified_ids_match() {
        let mac = find(
            OsKind::Macos,
            &[
                app("com.depthpicture.PixPin"),
                app("com.tencent.qq"),
                app("org.yuanli.utools"),
                app("com.hezongyidev.Bob"),
                app("com.Snipaste"),
                app("com.wiheads.paste-direct"),
            ],
        );
        let names: Vec<&str> = mac.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["Bob", "Paste", "Snipaste", "PixPin", "uTools", "QQ"]);
        // PixPin 的默认键是 ⌃1 / ⌃2（不是 F1 / F3）
        let pixpin = mac.iter().find(|r| r.name == "PixPin").expect("PixPin");
        assert_eq!(pixpin.combos, ["Control+Digit1", "Control+Digit2"]);
        // 旧的候选值不再命中
        assert!(find(OsKind::Macos, &[app("com.pixpin.pixpin"), app("com.snipaste.snipaste")]).is_empty());
        // Windows 进程快照的 id 是小写的 exe 名
        let win = find(
            OsKind::Windows,
            &[app("pixpin.exe"), app("powertoys.exe"), app("ditto.exe")],
        );
        assert_eq!(win.len(), 3);
    }

    /// 真机探测（手动运行：`cargo test -p wisp-desktop --lib hotkeys::neighbors::tests::live_probe -- --ignored
    /// --nocapture`）：列出本机正在运行的已知邻居与耗时（目标：探测 < 20 ms 且不常驻）
    #[test]
    #[ignore = "读取本机正在运行的应用列表"]
    #[allow(clippy::print_stderr)] // 真机手工用例：打印探测到的邻居与耗时
    fn live_probe() {
        let platform = wisp_platform::Platform::create(wisp_platform::PlatformConfig {
            own_pid: std::process::id(),
            main_thread: std::sync::Arc::new(|f: Box<dyn FnOnce() + Send>| f()),
            self_hit: std::sync::Arc::new(|_, _| false),
            keymaps: std::sync::Arc::new(parking_lot::RwLock::new(Vec::new())),
            os_version: String::new(),
        });
        // 第一次调用含 NSWorkspace 的冷启动；取第二次的耗时
        let _ = probe(&platform);
        let started = Instant::now();
        let found = probe(&platform);
        let took = started.elapsed();
        eprintln!("live neighbor probe: {found:?} in {took:?}");
        assert!(took < Duration::from_millis(20), "探测耗时 {took:?}");
    }

    /// 平台过滤、大小写、X11 的 instance 也算；同名邻居只出现一次
    #[test]
    fn matching_rules() {
        assert!(find(OsKind::Windows, &[app("com.izual.easydict")]).is_empty());
        let qq = find(OsKind::Windows, &[app("QQ.exe")]);
        assert!(is_running(&qq, "QQ"));
        let mut x = app("Qq-Class");
        x.alt_id = Some("qq".to_owned());
        assert!(is_running(&find(OsKind::Linux, &[x]), "QQ"));
        let wechat = find(OsKind::Windows, &[app("wechat.exe"), app("weixin.exe")]);
        assert_eq!(wechat.len(), 1);
        // QQ 的 Ctrl+Alt+A 与 Windows 输入翻译的备选键重合（检测到 QQ 时提示）
        let h = hits(
            OsKind::Windows,
            &qq,
            &[status(HotkeyAction::TranslateInput, "Control+Alt+KeyA")],
        );
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].display, "Ctrl+Alt+A");
    }
}
