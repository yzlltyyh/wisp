//! 配置、数据、日志目录：基于 `dirs` **显式拼出 `wisp`**，不使用带 bundle id 的 `app_*_dir`。

use std::ffi::OsString;
use std::path::PathBuf;

use crate::error::IpcError;

/// 覆盖离线模型目录的环境变量（测试与真机验证用，见 [`Paths::models_dir`]）
pub const MODELS_DIR_ENV: &str = "WISP_MODELS_DIR";

/// 应用目录
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// `config_dir/wisp/`：settings.json、styles.json、glossary.json、state.json
    pub config_dir: PathBuf,
    /// `data_dir/wisp/`：clipboard.db 与 blobs/
    pub data_dir: PathBuf,
    /// `log_dir/wisp/`：wisp.log
    pub log_dir: PathBuf,
}

impl Paths {
    /// 按当前平台计算目录（不创建）
    pub fn resolve() -> Result<Self, IpcError> {
        let config = dirs::config_dir().ok_or_else(|| IpcError::internal("cannot resolve config dir"))?;
        let data = dirs::data_local_dir().ok_or_else(|| IpcError::internal("cannot resolve data dir"))?;
        Ok(Self {
            config_dir: config.join("wisp"),
            data_dir: data.join("wisp"),
            log_dir: log_dir_for(false),
        })
    }

    /// 以某个根目录为基准的三个目录（`<root>/config`、`<root>/data`、`<root>/logs`）：冒烟模式与测试用，
    /// 不碰用户真实的配置、剪贴板历史与钥匙串
    pub fn under(root: &std::path::Path) -> Self {
        Self {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("logs"),
        }
    }

    /// 冒烟模式（`WISP_SMOKE=1`）的目录：系统临时目录下按进程号隔离的 `wisp-smoke-<pid>/`（日志插件同样写到这里的
    /// `logs/`，见 [`log_dir`]）
    pub fn smoke() -> Self {
        Self::under(&std::env::temp_dir().join(format!("wisp-smoke-{}", std::process::id())))
    }

    /// window lab（`WISP_WINDOW_LAB=1`，只在 debug / fake-llm 构建里编译）的配置与数据目录：系统临时目录下按进程号隔离的
    /// `wisp-window-lab-<pid>/`。实验台会改写 `state.json`，剪贴板服务也会把运行期间的复制写进
    /// `clipboard.db` / `blobs/`，不能落在用户的真实目录里。日志照常写用户的日志目录（实验台的结论就在日志里）
    pub fn window_lab() -> Self {
        let root = std::env::temp_dir().join(format!("wisp-window-lab-{}", std::process::id()));
        Self {
            log_dir: log_dir_for(false),
            ..Self::under(&root)
        }
    }

    /// 创建配置与数据目录（日志目录由日志插件自己创建）
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.config_dir)?;
        std::fs::create_dir_all(&self.data_dir)
    }

    /// settings.json
    pub fn settings_file(&self) -> PathBuf {
        self.config_dir.join("settings.json")
    }

    /// styles.json
    pub fn styles_file(&self) -> PathBuf {
        self.config_dir.join("styles.json")
    }

    /// glossary.json
    pub fn glossary_file(&self) -> PathBuf {
        self.config_dir.join("glossary.json")
    }

    /// state.json
    pub fn state_file(&self) -> PathBuf {
        self.config_dir.join("state.json")
    }

    /// 离线模型与推理运行时：`data_dir/models/`（Windows 的 LOCALAPPDATA 不漫游，正适合放 GB 级文件）。
    /// 环境变量 `WISP_MODELS_DIR` 可以改到别处，但只在 debug 构建或冒烟模式下生效——发布版不让一个环境变量把下载写到别处，
    /// 测试也不会写进用户真实的目录
    pub fn models_dir(&self) -> PathBuf {
        let allow_override = cfg!(debug_assertions) || super::smoke::enabled();
        models_dir_for(&self.data_dir, allow_override, std::env::var_os(MODELS_DIR_ENV))
    }
}

/// [`Paths::models_dir`] 的实现（覆盖是否允许、覆盖值由调用方给出，便于测试）
fn models_dir_for(data_dir: &std::path::Path, allow_override: bool, value: Option<OsString>) -> PathBuf {
    match value.filter(|v| allow_override && !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => data_dir.join("models"),
    }
}

/// 日志目录：macOS `~/Library/Logs/wisp`；Windows `%LOCALAPPDATA%\wisp\logs`；Linux `$XDG_STATE_HOME/wisp/logs`；
/// 冒烟模式下为 `Paths::smoke().log_dir`（冒烟日志不混进用户的 wisp.log，诊断导出与 `open_path(Log)` 也指向它）。
/// 日志插件在 Builder 之前构造，拿不到 `app.path()`，所以单独提供
pub fn log_dir() -> PathBuf {
    log_dir_for(super::smoke::enabled())
}

/// [`log_dir`] 的实现（`smoke` 由调用方给出，便于测试）
pub fn log_dir_for(smoke: bool) -> PathBuf {
    if smoke {
        return Paths::smoke().log_dir;
    }
    let fallback = || std::env::temp_dir().join("wisp").join("logs");
    match std::env::consts::OS {
        "macos" => dirs::home_dir()
            .map(|h| h.join("Library").join("Logs").join("wisp"))
            .unwrap_or_else(fallback),
        "windows" => dirs::data_local_dir()
            .map(|d| d.join("wisp").join("logs"))
            .unwrap_or_else(fallback),
        _ => dirs::state_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))
            .map(|d| d.join("wisp").join("logs"))
            .unwrap_or_else(fallback),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三端路径都显式拼出 `wisp`，不用带 bundle id 的 `app_*_dir`
    #[test]
    fn paths_follow_the_platform_table() {
        let p = Paths::resolve().expect("resolve");
        for dir in [&p.config_dir, &p.data_dir] {
            assert_eq!(
                dir.file_name().and_then(|n| n.to_str()),
                Some("wisp"),
                "{}",
                dir.display()
            );
        }
        let home = dirs::home_dir().expect("home");
        match std::env::consts::OS {
            "macos" => {
                let support = home.join("Library").join("Application Support").join("wisp");
                assert_eq!(p.config_dir, support);
                assert_eq!(p.data_dir, support);
                assert_eq!(p.log_dir, home.join("Library").join("Logs").join("wisp"));
            }
            _ => {
                // Windows：%APPDATA%\wisp、%LOCALAPPDATA%\wisp\logs；Linux：$XDG_CONFIG_HOME/wisp、$XDG_STATE_HOME/wisp/logs
                assert!(p.log_dir.ends_with(std::path::Path::new("wisp").join("logs")));
            }
        }
        assert!(
            p.settings_file()
                .ends_with(std::path::Path::new("wisp").join("settings.json"))
        );
    }

    /// 模型目录默认在 data_dir 下；覆盖只在允许时生效，空值不算
    #[test]
    fn models_dir_override_is_gated() {
        let data = std::path::Path::new("/data/wisp");
        assert_eq!(models_dir_for(data, false, None), data.join("models"));
        assert_eq!(
            models_dir_for(data, false, Some(OsString::from("/tmp/m"))),
            data.join("models"),
            "发布版不认环境变量"
        );
        assert_eq!(
            models_dir_for(data, true, Some(OsString::from("/tmp/m"))),
            PathBuf::from("/tmp/m")
        );
        assert_eq!(models_dir_for(data, true, Some(OsString::new())), data.join("models"));
    }

    #[test]
    fn window_lab_paths_are_isolated_but_keep_the_user_log() {
        let p = Paths::window_lab();
        assert!(p.config_dir.starts_with(std::env::temp_dir()));
        assert!(p.data_dir.starts_with(p.config_dir.parent().expect("root")));
        assert_ne!(p.config_dir, Paths::smoke().config_dir);
        assert_ne!(p.data_dir, Paths::resolve().expect("resolve").data_dir);
        assert_eq!(p.log_dir, log_dir_for(false));
    }

    #[test]
    fn smoke_paths_are_isolated_from_the_user_profile() {
        let p = Paths::smoke();
        assert!(p.config_dir.starts_with(std::env::temp_dir()));
        // 日志插件在冒烟模式下与 Paths::smoke() 写同一个目录，不碰用户的日志
        assert_eq!(log_dir_for(true), p.log_dir);
        assert_ne!(log_dir_for(true), log_dir_for(false));
        assert!(p.log_dir.starts_with(p.config_dir.parent().expect("root")));
        assert_ne!(p.config_dir, Paths::resolve().expect("resolve").config_dir);
        assert!(
            p.config_dir
                .to_string_lossy()
                .contains(&format!("wisp-smoke-{}", std::process::id()))
        );
    }
}
