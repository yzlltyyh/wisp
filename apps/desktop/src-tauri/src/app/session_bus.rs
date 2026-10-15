//! Linux 会话总线（D-Bus）上的名字查询：托盘宿主探测（`tray::host`，StatusNotifierWatcher）与钥匙串初始化前的 Secret
//! Service 探测（`settings::keychain`，`org.freedesktop.secrets`）共用。全是可移植代码、不写 `cfg(target_os)`，
//! 只在 Linux 上被调用。
//!
//! 依次尝试 `busctl` / `gdbus` / `dbus-send`（各限时 [`PROBE_TIMEOUT`]），三者都不可用时返回 None，调用方按「不知道」处理、
//! 不误报。**阻塞**（起子进程），调用方放到阻塞线程池。
//!
//! 为什么不直接用 zbus：desktop 没有直接依赖 zbus（它只是 keyring 的间接依赖），引入需要改 desktop 的 Cargo.toml 与
//! Cargo.lock，并在应用层写 `cfg(target_os = "linux")`，所以先用外部命令探测。

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 单个查询命令的超时
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// 会话总线上 `name` 当前有没有拥有者（`NameHasOwner`）；查询工具都不可用时为 None。**阻塞**，最多约 3 × [`PROBE_TIMEOUT`]
pub fn name_has_owner(name: &str) -> Option<bool> {
    query("NameHasOwner", Some(name)).as_deref().and_then(parse_bool_reply)
}

/// `name` 能否按需激活（`ListActivatableNames` 里有它，例如 gnome-keyring 的 `org.freedesktop.secrets.service`）；
/// 查询工具都不可用时为 None。**阻塞**
pub fn name_activatable(name: &str) -> Option<bool> {
    query("ListActivatableNames", None).map(|out| list_contains(&out, name))
}

/// `name` 已在总线上或可以按需激活；查不到时为 None。**阻塞**
pub fn name_reachable(name: &str) -> Option<bool> {
    match name_has_owner(name)? {
        true => Some(true),
        false => name_activatable(name),
    }
}

/// 调用 `org.freedesktop.DBus` 的方法（`arg` 为可选的字符串参数），返回第一个成功的工具的输出
fn query(method: &str, arg: Option<&str>) -> Option<String> {
    let full_method = format!("org.freedesktop.DBus.{method}");
    let dbus_send_arg = arg.map(|a| format!("string:{a}"));
    let mut busctl = vec![
        "--user",
        "--no-pager",
        "call",
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        method,
    ];
    let mut gdbus = vec![
        "call",
        "--session",
        "--dest",
        "org.freedesktop.DBus",
        "--object-path",
        "/org/freedesktop/DBus",
        "--method",
        &full_method,
    ];
    let mut dbus_send = vec![
        "--session",
        "--print-reply=literal",
        "--dest=org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        &full_method,
    ];
    if let Some(a) = arg {
        busctl.extend(["s", a]);
        gdbus.push(a);
    }
    if let Some(a) = dbus_send_arg.as_deref() {
        dbus_send.push(a);
    }
    [("busctl", busctl), ("gdbus", gdbus), ("dbus-send", dbus_send)]
        .iter()
        .find_map(|(program, args)| run_with_timeout(program, args, PROBE_TIMEOUT))
}

/// 三种工具的布尔回复：busctl `b true`、gdbus `(true,)`、dbus-send `   boolean true`
pub fn parse_bool_reply(output: &str) -> Option<bool> {
    output
        .split(|c: char| !c.is_ascii_alphanumeric())
        .find_map(|word| match word {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        })
}

/// 三种工具的名字列表回复里有没有 `name`：busctl `as 2 "a.b" "c.d"`、gdbus `(['a.b', 'c.d'],)`、dbus-send 每行一个
pub fn list_contains(output: &str, name: &str) -> bool {
    output
        .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':')))
        .any(|token| token == name)
}

/// 运行命令并在 `timeout` 内取得 stdout：程序不存在、退出码非 0、超时（会结束子进程）都返回 None
pub fn run_with_timeout(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return Some(out);
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bool_replies_of_all_tools_are_parsed() {
        assert_eq!(parse_bool_reply("b true\n"), Some(true));
        assert_eq!(parse_bool_reply("(false,)\n"), Some(false));
        assert_eq!(parse_bool_reply("   boolean true\n"), Some(true));
        assert_eq!(parse_bool_reply("Error: no bus"), None);
        assert_eq!(parse_bool_reply(""), None);
    }

    #[test]
    fn name_lists_of_all_tools_are_parsed() {
        let name = "org.freedesktop.secrets";
        let busctl = "as 3 \"org.freedesktop.DBus\" \"org.freedesktop.secrets\" \"org.gnome.Shell\"\n";
        let gdbus = "(['org.freedesktop.DBus', 'org.freedesktop.secrets'],)\n";
        let dbus_send = "   array [\n      org.freedesktop.DBus\n      org.freedesktop.secrets\n   ]\n";
        for out in [busctl, gdbus, dbus_send] {
            assert!(list_contains(out, name), "{out}");
        }
        // 只认完整的名字（前缀、更长的名字都不算）
        assert!(!list_contains("as 1 \"org.freedesktop.secrets.extra\"", name));
        assert!(!list_contains("(['org.freedesktop'],)", name));
        assert!(!list_contains("", name));
    }

    #[test]
    fn missing_probe_tools_yield_none() {
        assert_eq!(
            run_with_timeout("wisp-no-such-tool-7c1e", &[], Duration::from_millis(200)),
            None
        );
    }
}
