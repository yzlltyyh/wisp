//! 远端目录布局与加密时使用的逻辑名。
//!
//! 所有路径都相对于同步根目录（用户填写的地址之下的 `wisp/`，见 [`crate::remote::ROOT_DIR`]），
//! 不带开头与结尾的 `/`。路径只由校验过的 id（十六进制）与固定片段拼成，不会出现需要转义的字符。
//!
//! ```text
//! vault.json                                明文：格式版本、vault_id、KDF 参数、口令校验
//! <vault_id>.<device_id>.head               每台设备一个（密文）
//! <vault_id>/log/<device_id>/<w>.seg        日志段（密文，不可变；w 为 10 位十进制）
//! <vault_id>/blobs/<device_id>/<name>.blob  大对象（密文，不可变）
//! ```
//!
//! 逻辑名进入 AEAD 的附加数据，把密文绑定到它应在的位置：把一个文件挪到别的名字下会解密失败。

/// vault.json 的路径
pub const VAULT_FILE: &str = "vault.json";

/// 写入编号（w）的上限：10 位十进制
pub const MAX_W: u64 = 9_999_999_999;

const HEAD_SUFFIX: &str = ".head";

/// vault_id 与 device_id：16 位小写十六进制
pub fn is_valid_id(s: &str) -> bool {
    is_lower_hex(s, 16)
}

/// blob 名：32 位小写十六进制
pub fn is_valid_blob_name(s: &str) -> bool {
    is_lower_hex(s, 32)
}

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `<vault_id>.<device_id>.head`
pub fn head_path(vault_id: &str, device_id: &str) -> String {
    format!("{vault_id}.{device_id}{HEAD_SUFFIX}")
}

/// 从根目录列表里的文件名解析出属于该 vault 的 head 的 device_id；其他文件（旧 vault 的残留等）返回 `None`
pub fn parse_head_name<'a>(vault_id: &str, name: &'a str) -> Option<&'a str> {
    let device = name
        .strip_prefix(vault_id)?
        .strip_prefix('.')?
        .strip_suffix(HEAD_SUFFIX)?;
    is_valid_id(device).then_some(device)
}

/// `<vault_id>`：本 vault 的日志与 blob 都在这个目录下（清除服务器数据时整体删除）
pub fn vault_dir(vault_id: &str) -> String {
    vault_id.to_string()
}

/// `<vault_id>/log/<device_id>`
pub fn log_dir(vault_id: &str, device_id: &str) -> String {
    format!("{vault_id}/log/{device_id}")
}

/// `<vault_id>/log/<device_id>/<w:010>.seg`
pub fn segment_path(vault_id: &str, device_id: &str, w: u64) -> String {
    format!("{vault_id}/log/{device_id}/{w:010}.seg")
}

/// `<vault_id>/blobs/<device_id>`
pub fn blob_dir(vault_id: &str, device_id: &str) -> String {
    format!("{vault_id}/blobs/{device_id}")
}

/// `<vault_id>/blobs/<device_id>/<name>.blob`
pub fn blob_path(vault_id: &str, device_id: &str, name: &str) -> String {
    format!("{vault_id}/blobs/{device_id}/{name}.blob")
}

/// head 的逻辑名：`head/<device_id>`
pub fn head_aad_name(device_id: &str) -> String {
    format!("head/{device_id}")
}

/// 日志段的逻辑名：`seg/<device_id>/<w:010>`
pub fn segment_aad_name(device_id: &str, w: u64) -> String {
    format!("seg/{device_id}/{w:010}")
}

/// blob 的逻辑名：`blobs/<device_id>/<name>`
pub fn blob_aad_name(device_id: &str, name: &str) -> String {
    format!("blobs/{device_id}/{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const V: &str = "0123456789abcdef";
    const D: &str = "fedcba9876543210";

    #[test]
    fn paths() {
        assert_eq!(head_path(V, D), "0123456789abcdef.fedcba9876543210.head");
        assert_eq!(
            segment_path(V, D, 42),
            "0123456789abcdef/log/fedcba9876543210/0000000042.seg"
        );
        assert_eq!(blob_path(V, D, "ab"), "0123456789abcdef/blobs/fedcba9876543210/ab.blob");
        assert_eq!(segment_aad_name(D, 7), "seg/fedcba9876543210/0000000007");
        assert_eq!(head_aad_name(D), "head/fedcba9876543210");
        assert_eq!(blob_aad_name(D, "ab"), "blobs/fedcba9876543210/ab");
    }

    #[test]
    fn head_names_are_filtered_by_vault() {
        assert_eq!(parse_head_name(V, &head_path(V, D)), Some(D));
        assert_eq!(parse_head_name(V, &head_path("1111111111111111", D)), None);
        assert_eq!(parse_head_name(V, "0123456789abcdef.FEDCBA9876543210.head"), None);
        assert_eq!(parse_head_name(V, "0123456789abcdef.fedcba9876543210.head.tmp"), None);
        assert_eq!(parse_head_name(V, VAULT_FILE), None);
        assert_eq!(parse_head_name(V, V), None);
    }

    #[test]
    fn id_validation() {
        assert!(is_valid_id(V));
        assert!(!is_valid_id("0123456789ABCDEF"));
        assert!(!is_valid_id("../../etc/passwd"));
        assert!(!is_valid_id("0123456789abcde"));
        assert!(is_valid_blob_name(&"a".repeat(32)));
        assert!(!is_valid_blob_name(&"g".repeat(32)));
    }
}
