//! XSETTINGS 解析（纯逻辑，只依赖 std）。
//!
//! X11 下由 XSETTINGS 管理器（gsd-xsettings、xfsettingsd、xsettingsd、KDE 的 kde-gtk-config…）把 GTK 主题、
//! 图标主题、动画开关、缩放倍数等写在 `_XSETTINGS_S<screen>` 选区 owner 窗口的 `_XSETTINGS_SETTINGS` 属性里。
//! 这里只解析该属性的二进制格式；监听属性变化由 `x11` 模块负责（PropertyNotify，事件驱动，零轮询）。

use std::collections::HashMap;

/// 单个设置的取值
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XSetting {
    Int(i32),
    Str(String),
    /// (red, green, blue, alpha)，各 16 位
    Color(u16, u16, u16, u16),
}

/// 解析后的设置表
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XSettings {
    pub serial: u32,
    pub values: HashMap<String, XSetting>,
}

impl XSettings {
    /// 取字符串设置
    pub fn str(&self, name: &str) -> Option<&str> {
        match self.values.get(name) {
            Some(XSetting::Str(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    /// 取整数设置
    pub fn int(&self, name: &str) -> Option<i32> {
        match self.values.get(name) {
            Some(XSetting::Int(v)) => Some(*v),
            _ => None,
        }
    }

    /// GTK 主题名（`Net/ThemeName`）
    pub fn theme_name(&self) -> Option<&str> {
        self.str("Net/ThemeName")
    }

    /// 图标主题名（`Net/IconThemeName`）
    pub fn icon_theme_name(&self) -> Option<&str> {
        self.str("Net/IconThemeName")
    }

    /// 是否开启动画（`Gtk/EnableAnimations`，GNOME 的 enable-animations 会映射到这里）
    pub fn enable_animations(&self) -> Option<bool> {
        self.int("Gtk/EnableAnimations").map(|v| v != 0)
    }

    /// GDK 的整数窗口缩放（`Gdk/WindowScalingFactor`）
    pub fn window_scaling_factor(&self) -> Option<u32> {
        self.int("Gdk/WindowScalingFactor")
            .and_then(|v| u32::try_from(v).ok())
            .filter(|v| (1..=8).contains(v))
    }
}

/// 解析 `_XSETTINGS_SETTINGS` 属性的原始字节；格式错误时返回 None（不 panic）
pub fn parse(data: &[u8]) -> Option<XSettings> {
    let mut r = Reader {
        data,
        pos: 0,
        big_endian: false,
    };
    let order = r.u8()?;
    r.big_endian = match order {
        0 => false,
        1 => true,
        _ => return None,
    };
    r.skip(3)?;
    let serial = r.u32()?;
    let count = r.u32()?;
    // 防御：每条设置至少 12 字节，数量明显超过数据长度时视为损坏
    if count as usize > data.len() / 8 {
        return None;
    }
    let mut values = HashMap::with_capacity(count as usize);
    for _ in 0..count {
        let kind = r.u8()?;
        r.skip(1)?;
        let name_len = r.u16()? as usize;
        let name = String::from_utf8_lossy(r.bytes(name_len)?).into_owned();
        r.align4()?;
        let _last_change = r.u32()?;
        let value = match kind {
            0 => XSetting::Int(r.u32()? as i32),
            1 => {
                let len = r.u32()? as usize;
                let s = String::from_utf8_lossy(r.bytes(len)?).into_owned();
                r.align4()?;
                XSetting::Str(s)
            }
            2 => {
                // 协议中的顺序是 red, blue, green, alpha
                let red = r.u16()?;
                let blue = r.u16()?;
                let green = r.u16()?;
                let alpha = r.u16()?;
                XSetting::Color(red, green, blue, alpha)
            }
            _ => return None,
        };
        values.insert(name, value);
    }
    Some(XSettings { serial, values })
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    big_endian: bool,
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let out = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(out)
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        self.bytes(n).map(|_| ())
    }

    fn align4(&mut self) -> Option<()> {
        let pad = (4 - self.pos % 4) % 4;
        self.skip(pad)
    }

    fn u8(&mut self) -> Option<u8> {
        self.bytes(1).map(|b| b[0])
    }

    fn u16(&mut self) -> Option<u16> {
        let b: [u8; 2] = self.bytes(2)?.try_into().ok()?;
        Some(if self.big_endian {
            u16::from_be_bytes(b)
        } else {
            u16::from_le_bytes(b)
        })
    }

    fn u32(&mut self) -> Option<u32> {
        let b: [u8; 4] = self.bytes(4)?.try_into().ok()?;
        Some(if self.big_endian {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按协议格式构造一段 XSETTINGS 数据（小端或大端）
    fn build(big: bool, entries: &[(&str, XSetting)]) -> Vec<u8> {
        let u16b = |v: u16| if big { v.to_be_bytes() } else { v.to_le_bytes() };
        let u32b = |v: u32| if big { v.to_be_bytes() } else { v.to_le_bytes() };
        let pad = |out: &mut Vec<u8>| {
            while !out.len().is_multiple_of(4) {
                out.push(0);
            }
        };
        let mut out = vec![u8::from(big), 0, 0, 0];
        out.extend(u32b(7));
        out.extend(u32b(entries.len() as u32));
        for (name, value) in entries {
            let kind = match value {
                XSetting::Int(_) => 0,
                XSetting::Str(_) => 1,
                XSetting::Color(..) => 2,
            };
            out.push(kind);
            out.push(0);
            out.extend(u16b(name.len() as u16));
            out.extend(name.as_bytes());
            pad(&mut out);
            out.extend(u32b(3));
            match value {
                XSetting::Int(v) => out.extend(u32b(*v as u32)),
                XSetting::Str(s) => {
                    out.extend(u32b(s.len() as u32));
                    out.extend(s.as_bytes());
                    pad(&mut out);
                }
                XSetting::Color(r, g, b, a) => {
                    out.extend(u16b(*r));
                    out.extend(u16b(*b));
                    out.extend(u16b(*g));
                    out.extend(u16b(*a));
                }
            }
        }
        out
    }

    #[test]
    fn parses_little_and_big_endian() {
        for big in [false, true] {
            let data = build(
                big,
                &[
                    ("Net/ThemeName", XSetting::Str("Adwaita-dark".to_owned())),
                    ("Gtk/EnableAnimations", XSetting::Int(0)),
                    ("Gdk/WindowScalingFactor", XSetting::Int(2)),
                    ("Net/IconThemeName", XSetting::Str("Yaru".to_owned())),
                    ("Gtk/Color", XSetting::Color(1, 2, 3, 4)),
                    ("Xft/DPI", XSetting::Int(-1)),
                ],
            );
            let s = parse(&data).expect("可以解析");
            assert_eq!(s.serial, 7);
            assert_eq!(s.theme_name(), Some("Adwaita-dark"));
            assert_eq!(s.icon_theme_name(), Some("Yaru"));
            assert_eq!(s.enable_animations(), Some(false));
            assert_eq!(s.window_scaling_factor(), Some(2));
            assert_eq!(s.values.get("Gtk/Color"), Some(&XSetting::Color(1, 2, 3, 4)));
            assert_eq!(s.int("Xft/DPI"), Some(-1));
        }
    }

    #[test]
    fn rejects_garbage_without_panicking() {
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&[9, 0, 0, 0]), None);
        let mut data = build(false, &[("Net/ThemeName", XSetting::Str("x".to_owned()))]);
        data.truncate(data.len() - 3);
        assert_eq!(parse(&data), None);
        // 数量字段被篡改成巨大值
        let mut data = build(false, &[]);
        data[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse(&data), None);
    }

    #[test]
    fn invalid_scaling_factor_is_ignored() {
        let data = build(false, &[("Gdk/WindowScalingFactor", XSetting::Int(0))]);
        assert_eq!(parse(&data).and_then(|s| s.window_scaling_factor()), None);
    }
}
