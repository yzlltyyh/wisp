//! 本机在同步里的身份与几个小工具：设备 id、设备名、配置影子的指纹、模型服务地址的 origin。

use wisp_types::OsKind;

use crate::error::IpcError;
use crate::settings::migrate::DOC_SCHEMA_VERSION;
use crate::settings::state_store::StateStore;

/// 设备名的长度上限（字符）。设备名会写进服务器上的设备信息，过长的主机名截断即可
const MAX_NAME_CHARS: usize = 64;

/// 本机设备 id：state.json 里有就沿用（断开后重连也沿用，其他设备上的设备列表不会多出一台），没有就生成并立即写盘。
/// **阻塞**（写 state.json）
pub fn ensure_device_id(state: &StateStore) -> Result<String, IpcError> {
    if let Some(id) = state.sync_device_id() {
        return Ok(id);
    }
    let id = wisp_sync::crypto::random_id().map_err(|_| IpcError::internal("cannot generate a device id"))?;
    state.set_sync_device_id(Some(&id))?;
    Ok(id)
}

/// 设备名：系统主机名（macOS 的主机名多带一个 `.local` 后缀，去掉）；取不到时用系统名称
pub fn device_name() -> String {
    clean_name(&tauri_plugin_os::hostname(), OsKind::current())
}

fn clean_name(raw: &str, os: OsKind) -> String {
    let trimmed = raw.trim();
    let name = trimmed
        .len()
        .checked_sub(".local".len())
        .filter(|&at| trimmed.is_char_boundary(at) && trimmed[at..].eq_ignore_ascii_case(".local"))
        .map_or(trimmed, |at| &trimmed[..at])
        .trim();
    let name: String = name.chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect();
    if name.is_empty() {
        match os {
            OsKind::Macos => "Mac",
            OsKind::Windows => "Windows",
            OsKind::Linux => "Linux",
        }
        .to_owned()
    } else {
        name
    }
}

/// 配置影子的指纹：应用版本 + 三个文件的 schema 版本。与上次不同说明升级或降级过，旧影子不能拿来判断手改
pub fn fingerprint(app_version: &str) -> String {
    format!(
        "{app_version};{};{DOC_SCHEMA_VERSION};{DOC_SCHEMA_VERSION}",
        wisp_types::SETTINGS_SCHEMA_VERSION
    )
}

/// 同步记录里写的各文件 schema 版本
pub fn schema_versions() -> wisp_sync::SchemaVersions {
    wisp_sync::SchemaVersions {
        settings: wisp_types::SETTINGS_SCHEMA_VERSION,
        styles: DOC_SCHEMA_VERSION,
        glossary: DOC_SCHEMA_VERSION,
    }
}

/// 模型服务地址的 origin（`scheme://host:port`，端口总是写出来）。同一个 Key 只能发往设置它时的 origin：别处把服务地址
/// 改到另一台主机时，本机的 Key 不能跟着发过去
pub fn provider_origin(base_url: &str) -> Option<String> {
    let url = url::Url::parse(base_url.trim()).ok()?;
    let host = url.host_str()?;
    let port = url.port_or_known_default()?;
    Some(format!("{}://{}:{port}", url.scheme(), host.to_ascii_lowercase()))
}

/// Unix 毫秒
pub fn now_ms() -> i64 {
    i64::try_from(wisp_sync::hlc::wall_now_ms()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_names_drop_the_local_suffix() {
        assert_eq!(clean_name("Studio-Mac.local", OsKind::Macos), "Studio-Mac");
        assert_eq!(clean_name("  desk.LOCAL ", OsKind::Linux), "desk");
        assert_eq!(clean_name("DESKTOP-1", OsKind::Windows), "DESKTOP-1");
        assert_eq!(clean_name("", OsKind::Windows), "Windows");
        assert_eq!(clean_name(".local", OsKind::Macos), "Mac");
        assert_eq!(
            clean_name(&"x".repeat(200), OsKind::Linux).chars().count(),
            MAX_NAME_CHARS
        );
    }

    #[test]
    fn origins_always_carry_the_port() {
        assert_eq!(
            provider_origin("https://API.example.com/v1").as_deref(),
            Some("https://api.example.com:443")
        );
        assert_eq!(
            provider_origin("http://127.0.0.1:11434/v1").as_deref(),
            Some("http://127.0.0.1:11434")
        );
        assert_eq!(provider_origin("not a url"), None);
        assert_ne!(
            provider_origin("https://a.example.com/v1"),
            provider_origin("https://b.example.com/v1")
        );
    }

    #[test]
    fn device_ids_are_generated_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateStore::load(dir.path().join("state.json"));
        let id = ensure_device_id(&state).expect("id");
        assert!(crate::settings::state_store::is_sync_device_id(&id));
        assert_eq!(ensure_device_id(&state).expect("id"), id);
    }
}
