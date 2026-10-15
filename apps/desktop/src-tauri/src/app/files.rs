//! 与文件系统 / 系统外壳打交道的应用级操作：原生文件对话框（回调式 API 转 async，不阻塞线程）、术语表导入导出、
//! 打开目录（诊断与数据目录「一键打开」）与外部链接。ipc 层只做校验后调用这里。

use std::path::{Path, PathBuf};

use tauri::Manager;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;
use wisp_types::{GlossaryFile, PathKind};

use crate::error::{IpcError, IpcErrorCode};
use crate::settings::glossary_store::{self, Format};
use crate::state::AppState;

/// 导入文件的大小上限（术语表按 1000 条设计，4 MB 足够宽裕）
pub const MAX_IMPORT_BYTES: u64 = 4 * 1024 * 1024;

fn state(app: &tauri::AppHandle) -> Result<tauri::State<'_, AppState>, IpcError> {
    app.try_state::<AppState>()
        .ok_or_else(|| IpcError::new(IpcErrorCode::NotReady, "app is starting"))
}

/// 原生「打开文件」对话框；用户取消时返回 None
pub async fn open_dialog(
    app: &tauri::AppHandle,
    title: &str,
    filter_name: &str,
    extensions: &[&str],
) -> Result<Option<PathBuf>, IpcError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title(title)
        .add_filter(filter_name, extensions)
        .pick_file(move |path| {
            let _ = tx.send(path);
        });
    let picked = rx
        .await
        .map_err(|_| IpcError::internal("file dialog closed unexpectedly"))?;
    picked
        .map(|p| {
            p.into_path()
                .map_err(|e| IpcError::invalid_argument(format!("unsupported path: {e}")))
        })
        .transpose()
}

/// 原生「保存文件」对话框；用户取消时返回 None
pub async fn save_dialog(
    app: &tauri::AppHandle,
    title: &str,
    file_name: &str,
    filter_name: &str,
    extensions: &[&str],
) -> Result<Option<PathBuf>, IpcError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title(title)
        .set_file_name(file_name)
        .add_filter(filter_name, extensions)
        .save_file(move |path| {
            let _ = tx.send(path);
        });
    let picked = rx
        .await
        .map_err(|_| IpcError::internal("save dialog closed unexpectedly"))?;
    picked
        .map(|p| {
            p.into_path()
                .map_err(|e| IpcError::invalid_argument(format!("unsupported path: {e}")))
        })
        .transpose()
}

/// 从 JSON / CSV / TSV 导入术语表（同 source 替换、新的追加），返回导入的条数；用户取消时为 0
pub async fn glossary_import(app: &tauri::AppHandle) -> Result<u32, IpcError> {
    let lang = crate::tray::menu::current_lang(app);
    let t = |key| super::i18n::tr(lang, key);
    let Some(path) = open_dialog(
        app,
        t("dialog.glossary.import"),
        t("dialog.glossary.filter"),
        &["json", "csv", "tsv", "txt"],
    )
    .await?
    else {
        return Ok(0);
    };
    let settings = std::sync::Arc::clone(&state(app)?.settings);
    tauri::async_runtime::spawn_blocking(move || {
        let entries = read_import(&path)?;
        let count = settings.glossary_import(entries)?;
        log::info!("glossary imported count={count}");
        Ok(count)
    })
    .await
    .map_err(|e| IpcError::internal(format!("import task failed: {e}")))?
}

/// 读取并解析导入文件（大小上限、UTF-8、格式按扩展名或内容判断）
pub fn read_import(path: &Path) -> Result<Vec<wisp_types::GlossaryEntry>, IpcError> {
    let size = std::fs::metadata(path)?.len();
    if size > MAX_IMPORT_BYTES {
        return Err(IpcError::invalid_argument(format!(
            "glossary file exceeds {} MB",
            MAX_IMPORT_BYTES / 1024 / 1024
        )));
    }
    let bytes = std::fs::read(path)?;
    let text = String::from_utf8(bytes).map_err(|_| IpcError::invalid_argument("glossary file is not UTF-8 text"))?;
    let entries = glossary_store::parse_import(&text, Format::detect(path, &text))?;
    crate::ipc::validate::validate_glossary(&entries)?;
    Ok(entries)
}

/// 导出术语表（扩展名 `.csv` 为 CSV，其余为 JSON）；用户取消时返回 None
pub async fn glossary_export(app: &tauri::AppHandle) -> Result<Option<String>, IpcError> {
    let lang = crate::tray::menu::current_lang(app);
    let t = |key| super::i18n::tr(lang, key);
    let Some(path) = save_dialog(
        app,
        t("dialog.glossary.export"),
        "wisp-glossary.json",
        t("dialog.glossary.filter"),
        &["json", "csv"],
    )
    .await?
    else {
        return Ok(None);
    };
    let entries = state(app)?.settings.glossary_entries();
    let target = path.clone();
    tauri::async_runtime::spawn_blocking(move || write_export(&target, &entries))
        .await
        .map_err(|e| IpcError::internal(format!("export task failed: {e}")))??;
    log::info!("glossary exported");
    Ok(Some(path.to_string_lossy().into_owned()))
}

/// 按扩展名写出术语表（原子写入）
pub fn write_export(path: &Path, entries: &[wisp_types::GlossaryEntry]) -> Result<(), IpcError> {
    let is_csv = path
        .extension()
        .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case("csv"));
    let bytes = if is_csv {
        glossary_store::to_csv(entries).into_bytes()
    } else {
        glossary_store::encode(&GlossaryFile {
            schema_version: crate::settings::migrate::DOC_SCHEMA_VERSION,
            entries: entries.to_vec(),
        })?
    };
    crate::settings::fsio::write_atomic(path, &bytes)?;
    Ok(())
}

/// 在文件管理器中打开配置 / 数据 / 日志目录（不存在时先创建）
pub fn open_path(app: &tauri::AppHandle, kind: PathKind) -> Result<(), IpcError> {
    let st = state(app)?;
    let paths = st.settings.paths();
    let dir = match kind {
        PathKind::Config => &paths.config_dir,
        PathKind::Data => &paths.data_dir,
        PathKind::Log => &paths.log_dir,
    };
    std::fs::create_dir_all(dir)?;
    app.opener()
        .open_path(dir.to_string_lossy(), None::<&str>)
        .map_err(|e| IpcError::new(IpcErrorCode::Platform, format!("cannot open folder: {e}")))
}

/// 外部链接是否可以交给系统浏览器：只允许 http / https，且不含空白与控制字符
pub fn is_safe_url(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    let rest = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://"));
    rest.is_some_and(|r| !r.is_empty() && !r.starts_with('/'))
        && !url.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// 用默认浏览器打开链接（只允许 http / https）
pub fn open_url(app: &tauri::AppHandle, url: &str) -> Result<(), IpcError> {
    if !is_safe_url(url) {
        return Err(IpcError::invalid_argument("url must be an http(s) link"));
    }
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| IpcError::new(IpcErrorCode::Platform, format!("cannot open link: {e}")))
}

#[cfg(test)]
mod tests {
    use wisp_types::GlossaryEntry;

    use super::*;

    #[test]
    fn only_web_links_are_allowed() {
        for ok in [
            "https://github.com/yzlltyyh/wisp",
            "http://localhost:1420/x?y=1",
            "HTTPS://Example.com",
        ] {
            assert!(is_safe_url(ok), "{ok}");
        }
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://",
            "https:///path",
            "https://exa mple.com",
            "https://example.com/\n",
            "smb://server/share",
            "",
        ] {
            assert!(!is_safe_url(bad), "{bad:?}");
        }
    }

    #[test]
    fn export_then_import_round_trips_in_both_formats() {
        let dir = tempfile::tempdir().expect("tempdir");
        let entries = vec![
            GlossaryEntry {
                source: "token".to_owned(),
                target: "令牌".to_owned(),
                note: Some("LLM, \"tokens\"".to_owned()),
            },
            GlossaryEntry {
                source: "GPU".to_owned(),
                target: "GPU".to_owned(),
                note: None,
            },
        ];
        for name in ["g.json", "g.csv"] {
            let path = dir.path().join(name);
            write_export(&path, &entries).expect("export");
            let back = read_import(&path).expect("import");
            assert_eq!(back.len(), 2, "{name}");
            assert_eq!(back[0].target, "令牌", "{name}");
            assert_eq!(back[1].source, "GPU", "{name}");
        }
    }

    #[test]
    fn oversized_or_binary_imports_are_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let big = dir.path().join("big.json");
        let file = std::fs::File::create(&big).expect("create");
        file.set_len(MAX_IMPORT_BYTES + 1).expect("len");
        assert_eq!(read_import(&big).expect_err("big").code, IpcErrorCode::InvalidArgument);
        let bin = dir.path().join("bin.csv");
        std::fs::write(&bin, [0xff, 0xfe, 0x00, 0x81]).expect("write");
        assert_eq!(
            read_import(&bin).expect_err("binary").code,
            IpcErrorCode::InvalidArgument
        );
    }
}
