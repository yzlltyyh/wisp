//! 电源状态（纯逻辑 + 读 sysfs）：`/sys/class/power_supply/*` 下每个电源的 `type`、`online`、`status`、`scope`。
//!
//! 接着交流电源（`Mains`，或在线的 USB 供电）就不算用电池；只有电池、并且在放电时才算。`scope = Device` 的是鼠标、
//! 键盘之类外设的电池，不算本机电源。省电模式在 Linux 上没有统一的查询接口（各桌面环境、power-profiles-daemon 各有
//! 一套），返回 None。只依赖 std，`tests/smoke_linux.rs` 在任何系统上都能跑它的单测。

use std::path::Path;

/// 一个电源的几个属性（缺的为 None）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Supply {
    pub kind: String,
    pub online: Option<bool>,
    pub status: Option<String>,
    pub device_scope: bool,
}

/// 是否在用电池；看不出来（没有电源信息，例如容器里）时为 None
pub(crate) fn on_battery(supplies: &[Supply]) -> Option<bool> {
    let system: Vec<&Supply> = supplies.iter().filter(|s| !s.device_scope).collect();
    let external = |s: &&&Supply| s.kind == "Mains" || s.kind.starts_with("USB");
    if system.iter().filter(external).any(|s| s.online == Some(true)) {
        return Some(false);
    }
    let batteries: Vec<&&Supply> = system.iter().filter(|s| s.kind == "Battery").collect();
    if batteries.is_empty() {
        // 台式机：没有电池，有交流电源的记录就算接着电源
        return system.iter().any(|s| s.kind == "Mains").then_some(false);
    }
    if batteries.iter().any(|b| b.status.as_deref() == Some("Discharging")) {
        return Some(true);
    }
    // 有电池、交流电源明确离线
    system
        .iter()
        .filter(external)
        .any(|s| s.online == Some(false))
        .then_some(true)
}

/// 读 `dir`（一般是 `/sys/class/power_supply`）下的全部电源；目录不存在时为空
pub(crate) fn read_supplies(dir: &Path) -> Vec<Supply> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let field = |base: &Path, name: &str| -> Option<String> {
        std::fs::read_to_string(base.join(name))
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    entries
        .flatten()
        .map(|e| {
            let base = e.path();
            Supply {
                kind: field(&base, "type").unwrap_or_default(),
                online: field(&base, "online").map(|v| v == "1"),
                status: field(&base, "status"),
                device_scope: field(&base, "scope").is_some_and(|v| v.eq_ignore_ascii_case("device")),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supply(kind: &str, online: Option<bool>, status: Option<&str>) -> Supply {
        Supply {
            kind: kind.into(),
            online,
            status: status.map(Into::into),
            device_scope: false,
        }
    }

    #[test]
    fn laptop_on_ac_and_on_battery() {
        let ac = supply("Mains", Some(true), None);
        let bat = supply("Battery", None, Some("Charging"));
        assert_eq!(on_battery(&[ac.clone(), bat]), Some(false));
        let unplugged = supply("Mains", Some(false), None);
        let discharging = supply("Battery", None, Some("Discharging"));
        assert_eq!(on_battery(&[unplugged.clone(), discharging.clone()]), Some(true));
        // 只有电池信息
        assert_eq!(on_battery(&[discharging]), Some(true));
        assert_eq!(
            on_battery(&[supply("Battery", None, Some("Full")), unplugged]),
            Some(true)
        );
        // USB-C 供电
        assert_eq!(
            on_battery(&[
                supply("USB", Some(true), None),
                supply("Battery", None, Some("Discharging"))
            ]),
            Some(false)
        );
    }

    #[test]
    fn desktops_containers_and_peripherals() {
        assert_eq!(on_battery(&[supply("Mains", Some(true), None)]), Some(false));
        assert_eq!(on_battery(&[]), None);
        // 无线鼠标的电池不算本机电源
        let mouse = Supply {
            device_scope: true,
            ..supply("Battery", None, Some("Discharging"))
        };
        assert_eq!(on_battery(std::slice::from_ref(&mouse)), None);
        assert_eq!(on_battery(&[supply("Mains", Some(true), None), mouse]), Some(false));
    }

    #[test]
    fn reads_the_sysfs_layout() {
        let dir = std::env::temp_dir().join(format!("wisp-power-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (name, files) in [
            ("AC", vec![("type", "Mains\n"), ("online", "0\n")]),
            ("BAT0", vec![("type", "Battery\n"), ("status", "Discharging\n")]),
            ("hidpp_battery_0", vec![("type", "Battery\n"), ("scope", "Device\n")]),
        ] {
            let d = dir.join(name);
            std::fs::create_dir_all(&d).expect("mkdir");
            for (f, v) in files {
                std::fs::write(d.join(f), v).expect("write");
            }
        }
        let supplies = read_supplies(&dir);
        assert_eq!(supplies.len(), 3);
        assert_eq!(on_battery(&supplies), Some(true));
        assert!(read_supplies(&dir.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
