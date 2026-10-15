//! 诊断导出：把日志、脱敏后的 settings.json、styles.json、state.json 与 `app_info` 打包成 zip，
//! 输出到用户在保存对话框里选择的位置。
//!
//! 诊断包**不含** Key（Key 只在钥匙串里，本来就读不到）、请求头的值、URL 的 query、正文（剪贴板、OCR、译文从不写日志，
//! 术语表不打包）。日志逐行再过一遍 `logging::redact`；日志总量上限 [`MAX_LOG_BYTES`]（取最新的）。
//! 打包在阻塞线程池里完成，不占 tokio worker。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tauri::Manager;
use wisp_types::Settings;

use crate::error::IpcError;
use crate::state::AppState;

/// 打包的日志总量上限（字节）
pub const MAX_LOG_BYTES: u64 = 12 * 1024 * 1024;
/// 被替换掉的值
const MASK: &str = "***";

/// 弹出保存对话框并导出；用户取消时返回 None
pub async fn export(app: &tauri::AppHandle) -> Result<Option<String>, IpcError> {
    let Some(state) = app.try_state::<AppState>() else {
        return Err(IpcError::new(crate::error::IpcErrorCode::NotReady, "app is starting"));
    };
    let lang = crate::tray::menu::current_lang(app);
    let t = |key| super::i18n::tr(lang, key);
    let date = crate::settings::state_store::utc_date(crate::settings::fsio::unix_now_ms());
    let Some(target) = super::files::save_dialog(
        app,
        t("dialog.diagnostics.title"),
        &format!("wisp-diagnostics-{date}.zip"),
        t("dialog.diagnostics.filter"),
        &["zip"],
    )
    .await?
    else {
        return Ok(None);
    };
    let bundle = Bundle {
        app_info: serde_json::to_value(json!({
            "meta": super::app_meta(app, &state),
            "runtime": super::runtime_info(app, &state),
            "startup_ms": state.perf.startup(),
            "arch": std::env::consts::ARCH,
            "debug_build": cfg!(debug_assertions),
        }))
        .unwrap_or(Value::Null),
        settings: redacted_settings(&state.settings.get()),
        styles: read_json(&state.settings.paths().styles_file()),
        state: read_json(&state.settings.paths().state_file()),
        selection_stats: state.perf.selection_stats(),
        log_dir: state.settings.paths().log_dir.clone(),
    };
    let path = target.clone();
    tauri::async_runtime::spawn_blocking(move || write_zip(&path, &bundle))
        .await
        .map_err(|e| IpcError::internal(format!("diagnostics task failed: {e}")))??;
    log::info!("diagnostics exported");
    Ok(Some(target.to_string_lossy().into_owned()))
}

/// 要打包的内容
struct Bundle {
    app_info: Value,
    settings: Value,
    styles: Value,
    state: Value,
    /// 按应用的取词 method 分布、结果分布与平均耗时（本次运行以来；不含正文）
    selection_stats: Value,
    log_dir: PathBuf,
}

fn read_json(path: &Path) -> Value {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}

/// 去掉请求头的值、URL 里的凭据（userinfo）与 query 的 settings
pub fn redacted_settings(settings: &Settings) -> Value {
    let mut v = serde_json::to_value(settings).unwrap_or(Value::Null);
    if let Some(providers) = v.get_mut("providers").and_then(Value::as_array_mut) {
        for p in providers {
            if let Some(headers) = p.get_mut("headers").and_then(Value::as_object_mut) {
                for value in headers.values_mut() {
                    *value = Value::String(MASK.to_owned());
                }
            }
            if let Some(Value::String(url)) = p.get_mut("base_url") {
                *url = redact_url(url);
            }
        }
    }
    v
}

/// base_url 脱敏：userinfo（`https://user:token@host`，没写 scheme 时同样识别）与 query 换成 `***`，其余再经
/// `logging::redact`（路径里形似 Key 的片段）
pub fn redact_url(url: &str) -> String {
    let authority_start = url.find("://").map_or(0, |i| i + 3);
    let rest = &url[authority_start..];
    let authority_len = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let mut out = String::with_capacity(url.len());
    out.push_str(&url[..authority_start]);
    match rest[..authority_len].rfind('@') {
        Some(at) if at > 0 => {
            out.push_str(MASK);
            out.push_str(&rest[at..]);
        }
        _ => out.push_str(rest),
    }
    if let Some(q) = out.find('?') {
        out.truncate(q + 1);
        out.push_str(MASK);
    }
    crate::app::logging::redact(&out).into_owned()
}

fn write_zip(path: &Path, bundle: &Bundle) -> Result<(), IpcError> {
    use zip::write::SimpleFileOptions;
    let tmp = crate::settings::fsio::tmp_path(path);
    let result = (|| -> Result<(), IpcError> {
        let file = std::fs::File::create(&tmp)?;
        let mut zip = zip::ZipWriter::new(std::io::BufWriter::new(file));
        let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        let zerr = |e: zip::result::ZipError| IpcError::new(crate::error::IpcErrorCode::Io, e.to_string());
        let mut add = |name: &str, bytes: &[u8]| -> Result<(), IpcError> {
            zip.start_file(name, options).map_err(zerr)?;
            zip.write_all(bytes)?;
            Ok(())
        };
        add("README.txt", README.as_bytes())?;
        add("app_info.json", &pretty(&bundle.app_info))?;
        add("settings.json", &pretty(&bundle.settings))?;
        add("styles.json", &pretty(&bundle.styles))?;
        add("state.json", &pretty(&bundle.state))?;
        add("selection-stats.json", &pretty(&bundle.selection_stats))?;
        for (name, bytes) in collect_logs(&bundle.log_dir) {
            add(&format!("logs/{name}"), &bytes)?;
        }
        let mut writer = zip.finish().map_err(zerr)?;
        writer.flush()?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            std::fs::rename(&tmp, path)?;
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn pretty(v: &Value) -> Vec<u8> {
    serde_json::to_vec_pretty(v).unwrap_or_default()
}

/// 日志文件（新的在前，总量不超过上限），逐行脱敏
fn collect_logs(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(super::logging::FILE_STEM) && n.ends_with(".log"))
        })
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    let mut budget = MAX_LOG_BYTES;
    let mut out = Vec::new();
    for (_, path) in files {
        let Ok(text) = std::fs::read(&path) else { continue };
        let text = String::from_utf8_lossy(&text);
        let mut redacted = String::with_capacity(text.len());
        for line in text.lines() {
            redacted.push_str(&super::logging::redact(line));
            redacted.push('\n');
        }
        let len = redacted.len() as u64;
        if len > budget {
            break;
        }
        budget -= len;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        out.push((name, redacted.into_bytes()));
    }
    out
}

const README: &str = "Wisp diagnostics bundle\n\
\n\
app_info.json  version, platform, runtime state and startup timings\n\
settings.json  settings with request header values, URL credentials and URL queries replaced by ***\n\
styles.json    translation styles\n\
state.json     runtime state (window positions, onboarding, update checks)\n\
selection-stats.json  text selection per app since launch: method counts, outcomes, average time (no text)\n\
logs/          recent logs (no source text, translations, clipboard content or keys)\n\
\n\
API keys are stored only in the system keychain and are never part of this bundle.\n";

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use wisp_types::{ModelEntry, Protocol, Provider, Quirks, Timeouts};

    use super::*;

    fn provider() -> Provider {
        Provider {
            id: "p".to_owned(),
            name: "P".to_owned(),
            protocol: Protocol::OpenAiChat,
            base_url: "https://api.example.com/v1?api-key=SECRET".to_owned(),
            headers: BTreeMap::from([("X-Api-Key".to_owned(), "SECRET-HEADER".to_owned())]),
            quirks: Quirks::default(),
            models: vec![ModelEntry::new("m")],
            timeouts: Timeouts::default(),
            template_id: None,
        }
    }

    #[test]
    fn settings_are_redacted() {
        let mut s = Settings::default();
        s.providers.push(provider());
        let v = redacted_settings(&s);
        let text = v.to_string();
        assert!(!text.contains("SECRET"), "{text}");
        assert_eq!(v["providers"][0]["headers"]["X-Api-Key"], "***");
        assert_eq!(v["providers"][0]["base_url"], "https://api.example.com/v1?***");
        assert_eq!(v["providers"][0]["models"][0]["id"], "m", "其余内容保留");
    }

    #[test]
    fn base_url_credentials_are_redacted() {
        assert_eq!(
            redact_url("https://user:TOKEN@gw.example.com/v1?key=SECRET"),
            "https://***@gw.example.com/v1?***"
        );
        assert_eq!(redact_url("user:TOKEN@gw.example.com/v1"), "***@gw.example.com/v1");
        assert_eq!(redact_url("https://api.example.com/v1"), "https://api.example.com/v1");
        // 路径里形似 Key 的片段（token 字符含 `/`，后面的路径段一并遮住）
        assert_eq!(
            redact_url("https://api.example.com/keys/sk-abcdefghijklmnopqrstuv/v1"),
            "https://api.example.com/keys/***"
        );
        // 路径里的 @ 不是 userinfo
        assert_eq!(redact_url("https://host/@team/v1"), "https://host/@team/v1");
        let mut s = Settings::default();
        let mut p = provider();
        p.base_url = "https://gwuser42:hunter2@proxy.local/v1".to_owned();
        s.providers.push(p);
        let text = redacted_settings(&s).to_string();
        assert!(!text.contains("hunter2") && !text.contains("gwuser42"), "{text}");
    }

    #[test]
    fn zip_contains_redacted_logs_and_documents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).expect("mkdir");
        std::fs::write(logs.join("wisp.log"), "ok line\nkey sk-abcdefghijklmnopqrstu leaked\n").expect("log");
        std::fs::write(logs.join("other.txt"), "ignored").expect("other");
        let mut s = Settings::default();
        s.providers.push(provider());
        let bundle = Bundle {
            app_info: json!({"version": "0.1.0"}),
            settings: redacted_settings(&s),
            styles: Value::Null,
            state: json!({"onboarding_done": true}),
            selection_stats: json!({"com.apple.TextEdit": {"count": 2, "methods": {"ax": 2}, "results": {"ok": 2}, "total_ms": 20, "avg_ms": 10}}),
            log_dir: logs,
        };
        let out = dir.path().join("diag.zip");
        write_zip(&out, &bundle).expect("zip");
        let mut archive = zip::ZipArchive::new(std::fs::File::open(&out).expect("open")).expect("archive");
        let names: Vec<String> = archive
            .file_names()
            .filter_map(Result::ok)
            .map(|n| n.into_owned())
            .collect();
        for expected in [
            "README.txt",
            "app_info.json",
            "selection-stats.json",
            "settings.json",
            "state.json",
            "logs/wisp.log",
        ] {
            assert!(names.iter().any(|n| n == expected), "{names:?}");
        }
        assert!(!names.iter().any(|n| n.contains("other")));
        let mut log = String::new();
        std::io::Read::read_to_string(&mut archive.by_name("logs/wisp.log").expect("log"), &mut log).expect("read");
        assert!(log.contains("ok line"));
        assert!(!log.contains("sk-abcdefghijklmnopqrstu"));
        let mut settings = String::new();
        std::io::Read::read_to_string(&mut archive.by_name("settings.json").expect("settings"), &mut settings)
            .expect("read");
        assert!(!settings.contains("SECRET"));
        assert!(!crate::settings::fsio::tmp_path(&out).exists());
    }
}
