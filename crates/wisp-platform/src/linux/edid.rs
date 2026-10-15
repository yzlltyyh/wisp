//! 从 EDID 取显示器的可读名称（纯逻辑，只依赖 std）。RandR 输出名（`DP-1`、`eDP-1`）只适合作 id，
//! 设置页与日志更需要「DELL U2720Q」这样的名称。

const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];

/// 显示器名称：优先取「显示器名称」描述符（0xFC），没有时用「厂商代码 + 产品码」；EDID 无效时返回 None
pub fn monitor_name(edid: &[u8]) -> Option<String> {
    if edid.len() < 128 || edid[..8] != HEADER {
        return None;
    }
    for start in [54usize, 72, 90, 108] {
        let d = &edid[start..start + 18];
        if d[0] == 0 && d[1] == 0 && d[3] == 0xFC {
            let text: String = d[5..18]
                .iter()
                .take_while(|&&b| b != 0x0A && b != 0)
                .map(|&b| {
                    if b.is_ascii_graphic() || b == b' ' {
                        b as char
                    } else {
                        ' '
                    }
                })
                .collect();
            let text = text.trim();
            if !text.is_empty() {
                return Some(text.to_owned());
            }
        }
    }
    let vendor = vendor_id(edid)?;
    let product = u16::from_le_bytes([edid[10], edid[11]]);
    Some(format!("{vendor} {product:04X}"))
}

/// 三字母厂商代码（PNP ID）
fn vendor_id(edid: &[u8]) -> Option<String> {
    let raw = u16::from_be_bytes([edid[8], edid[9]]);
    let letters = [(raw >> 10) & 0x1F, (raw >> 5) & 0x1F, raw & 0x1F];
    letters
        .iter()
        .map(|&v| (1..=26).contains(&v).then(|| (b'A' + v as u8 - 1) as char))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edid_with(descriptor_name: Option<&str>) -> Vec<u8> {
        let mut e = vec![0u8; 128];
        e[..8].copy_from_slice(&HEADER);
        // "DEL"：D=4, E=5, L=12 → 00100 00101 01100
        let raw: u16 = (4 << 10) | (5 << 5) | 12;
        e[8..10].copy_from_slice(&raw.to_be_bytes());
        e[10..12].copy_from_slice(&0xA0C2u16.to_le_bytes());
        // 第一个描述符：序列号，第二个：名称
        e[54 + 3] = 0xFF;
        if let Some(name) = descriptor_name {
            let d = &mut e[72..90];
            d[3] = 0xFC;
            let mut text = name.as_bytes().to_vec();
            text.push(0x0A);
            text.resize(13, 0x20);
            d[5..18].copy_from_slice(&text[..13]);
        }
        e
    }

    #[test]
    fn reads_name_descriptor() {
        assert_eq!(
            monitor_name(&edid_with(Some("DELL U2720Q"))).as_deref(),
            Some("DELL U2720Q")
        );
    }

    #[test]
    fn falls_back_to_vendor_and_product() {
        assert_eq!(monitor_name(&edid_with(None)).as_deref(), Some("DEL A0C2"));
    }

    #[test]
    fn rejects_invalid() {
        assert_eq!(monitor_name(&[0u8; 64]), None);
        let mut e = edid_with(Some("X"));
        e[0] = 1;
        assert_eq!(monitor_name(&e), None);
    }
}
