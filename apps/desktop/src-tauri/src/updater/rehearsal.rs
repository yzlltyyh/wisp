//! 更新器本地演练的钩子（用法见 `scripts/release/updater-rehearsal.sh`）。
//!
//! **只在内嵌配置带 `plugins.wispUpdaterRehearsal.enabled = true` 的构建里生效**：演练脚本用 `tauri build --config` 叠加
//! 一份配置（本机端点、`dangerousInsecureTransportProtocol`、单独的 identifier 与这一项）。正式构建的 tauri.conf.json 没有
//! 这一项——这里不读任何环境变量、不做任何事。配置在编译时嵌入二进制，运行时改不了。
//!
//! **为什么端点不在这里覆盖**：tauri-plugin-updater 的 release 构建只接受 https 端点，除非配置打开
//! `dangerousInsecureTransportProtocol`，而这个开关只能来自编译时的配置；也就是说，运行时的端点覆盖（例如环境变量）在
//! release 构建里要么无从生效，要么得同时把正式构建的传输安全放开。演练构建的端点因此由叠加配置的
//! `plugins.updater.endpoints` 决定，正式构建不带任何覆盖的入口。
//!
//! **签名校验不受影响**：公钥只来自内嵌配置的 `plugins.updater.pubkey`，本模块与 `updater` 都不调用
//! `UpdaterBuilder::pubkey`；插件在 `Update::download` 里无条件验签。演练脚本一律用一次性测试密钥签名，叠加配置把 pubkey
//! 换成这把测试公钥（脚本核对 key id 与正式公钥不同；本机不产生正式签名的包）。脚本另用「篡改过的包 + 真签名」「别的密钥签的包」「latest.json 把旧的真包标成新版本」三种情形验证拒绝路径。
//!
//! 钩子（都在启动时、`schedule_startup_check` 里执行一次）：
//! - 记一行 `updater rehearsal build: version=… endpoints=… pubkey=…`（公钥本来就公开），脚本据此核对运行中的应用确实
//!   用着本机端点与正式公钥；
//! - `WISP_REHEARSAL_KEYCHAIN=write|read`：经应用自己的钥匙串句柄（与保存 Provider Key 同一条路径，条目的访问控制因此
//!   与真实的 Key 一致）写入 / 读取演练专用的条目（service `wisp`，account [`PROBE_ACCOUNT`]）。日志只记内容的指纹
//!   （blake3 前 8 字节），绝不记内容。旧版本写、新版本读：验证更新之后钥匙串条目仍然可读——ad-hoc 签名的 cdhash 每个
//!   版本都变，新版本第一次读取时系统会弹授权框，读取在阻塞线程池里等用户回答。

use std::collections::HashMap;

use serde_json::Value as JsonValue;
use tauri::Manager;

use crate::state::AppState;

/// 内嵌配置里打开演练钩子的插件配置项（`plugins.wispUpdaterRehearsal`）
pub const CONFIG_KEY: &str = "wispUpdaterRehearsal";
/// 钥匙串探针的环境变量（只在演练构建里读取）
pub const KEYCHAIN_ENV: &str = "WISP_REHEARSAL_KEYCHAIN";
/// 钥匙串探针条目的 account（service 与 Provider Key 相同，都是 `wisp`）。Provider id 是厂商模板 id（重名时加 `-<序号>`，
/// `wisp_llm::provider_from_template`），不会与它重名；演练脚本还把整个钥匙串放在隔离 HOME 里的一次性 keychain 文件中
pub const PROBE_ACCOUNT: &str = "wisp-updater-rehearsal-probe";

/// 钥匙串探针要做的事
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// 写入一个新的随机值
    Write,
    /// 读取并报告指纹
    Read,
}

/// 内嵌配置是否打开了演练钩子（纯函数）：`plugins.wispUpdaterRehearsal` 是带 `"enabled": true` 的对象。
/// 其他写法（缺失、`true`、字符串、`enabled` 不是布尔真）一律视为关闭
pub fn enabled_in(plugins: &HashMap<String, JsonValue>) -> bool {
    plugins
        .get(CONFIG_KEY)
        .and_then(|v| v.get("enabled"))
        .and_then(JsonValue::as_bool)
        .unwrap_or(false)
}

/// 解析 [`KEYCHAIN_ENV`]（纯函数；无法识别时为 None）
pub fn parse_probe(value: Option<&str>) -> Option<Probe> {
    match value.map(str::trim) {
        Some("write") => Some(Probe::Write),
        Some("read") => Some(Probe::Read),
        _ => None,
    }
}

/// 钥匙串内容的指纹（blake3 前 8 字节的十六进制）：只用于比对两次运行读写的是同一个值，不能反推内容
pub fn fingerprint(secret: &str) -> String {
    blake3::hash(secret.as_bytes()).as_bytes()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 内嵌配置里更新器的端点与公钥（只用于日志；公钥本来就公开）
fn updater_summary(plugins: &HashMap<String, JsonValue>) -> (String, String) {
    let updater = plugins.get("updater");
    let endpoints = updater
        .and_then(|u| u.get("endpoints"))
        .and_then(JsonValue::as_array)
        .map(|list| list.iter().filter_map(JsonValue::as_str).collect::<Vec<_>>().join(","))
        .unwrap_or_default();
    let pubkey = updater
        .and_then(|u| u.get("pubkey"))
        .and_then(JsonValue::as_str)
        .unwrap_or_default()
        .to_owned();
    (endpoints, pubkey)
}

/// 启动时（`schedule_startup_check`）调用：不是演练构建时只做一次哈希表查找
pub fn on_startup(app: &tauri::AppHandle) {
    let plugins = &app.config().plugins.0;
    if !enabled_in(plugins) {
        return;
    }
    let (endpoints, pubkey) = updater_summary(plugins);
    log::warn!(
        "updater rehearsal build: version={} endpoints={endpoints} pubkey={pubkey} (not a release build)",
        app.package_info().version
    );
    let raw = std::env::var(KEYCHAIN_ENV).ok();
    let Some(probe) = parse_probe(raw.as_deref()) else {
        if let Some(raw) = raw {
            log::warn!("updater rehearsal: ignoring {KEYCHAIN_ENV}={raw:?} (expected write or read)");
        }
        return;
    };
    let handle = app.clone();
    // 钥匙串调用是阻塞的（新版本第一次读取会等用户回答授权框）
    tauri::async_runtime::spawn_blocking(move || run_probe(&handle, probe));
}

fn run_probe(app: &tauri::AppHandle, probe: Probe) {
    let Some(state) = app.try_state::<AppState>() else {
        log::warn!("updater rehearsal: keychain probe skipped (app state is not ready)");
        return;
    };
    let keychain = state.settings.keychain();
    match probe {
        Probe::Write => {
            let value = uuid::Uuid::new_v4().simple().to_string();
            match keychain.set(PROBE_ACCOUNT, &value) {
                Ok(()) => log::info!(
                    "updater rehearsal: keychain probe written fingerprint={}",
                    fingerprint(&value)
                ),
                Err(e) => log::warn!("updater rehearsal: keychain probe write failed: {}", e.message),
            }
        }
        Probe::Read => {
            log::info!("updater rehearsal: keychain probe read started");
            match keychain.get(PROBE_ACCOUNT) {
                Ok(Some(secret)) => log::info!(
                    "updater rehearsal: keychain probe readable fingerprint={}",
                    fingerprint(secret.expose())
                ),
                Ok(None) => log::warn!("updater rehearsal: keychain probe missing"),
                Err(e) => log::warn!("updater rehearsal: keychain probe read failed: {}", e.message),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plugins(entries: &[(&str, JsonValue)]) -> HashMap<String, JsonValue> {
        entries.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect()
    }

    /// 正式构建的配置（只有 updater）不打开钩子；只有带 `enabled: true` 的对象才打开
    #[test]
    fn only_an_explicit_rehearsal_config_enables_the_hooks() {
        let release = plugins(&[(
            "updater",
            json!({ "pubkey": "x", "endpoints": ["https://example.invalid/latest.json"] }),
        )]);
        assert!(!enabled_in(&release));
        assert!(!enabled_in(&HashMap::new()));
        for loose in [
            json!(true),
            json!("enabled"),
            json!({}),
            json!({ "enabled": "true" }),
            json!({ "enabled": 1 }),
            json!({ "enabled": false }),
        ] {
            assert!(!enabled_in(&plugins(&[(CONFIG_KEY, loose.clone())])), "{loose}");
        }
        assert!(enabled_in(&plugins(&[(CONFIG_KEY, json!({ "enabled": true }))])));
    }

    /// 正式构建的 tauri.conf.json 里没有演练配置项（钩子在正式构建里永远不读环境变量）
    #[test]
    fn the_shipped_config_has_no_rehearsal_entry() {
        let conf: JsonValue =
            serde_json::from_str(include_str!("../../tauri.conf.json")).expect("tauri.conf.json is valid JSON");
        let plugins: HashMap<String, JsonValue> =
            serde_json::from_value(conf["plugins"].clone()).expect("plugins is an object");
        assert!(!enabled_in(&plugins));
        assert!(!plugins.contains_key(CONFIG_KEY));
        let (endpoints, pubkey) = updater_summary(&plugins);
        assert!(endpoints.starts_with("https://"), "正式端点是 https：{endpoints}");
        assert!(!pubkey.is_empty());
        assert_ne!(
            conf["plugins"]["updater"]["dangerousInsecureTransportProtocol"],
            json!(true),
            "正式配置不能放开 http 端点"
        );
    }

    #[test]
    fn probe_actions_parse_strictly() {
        assert_eq!(parse_probe(Some("write")), Some(Probe::Write));
        assert_eq!(parse_probe(Some(" read\n")), Some(Probe::Read));
        for junk in [None, Some(""), Some("READ"), Some("delete"), Some("1")] {
            assert_eq!(parse_probe(junk), None, "{junk:?}");
        }
    }

    /// 探针条目与 Provider Key 共用 service `wisp`：account 不能是任何厂商模板 id（Provider id 由模板 id 派生）
    #[test]
    fn the_probe_account_never_collides_with_a_provider_id() {
        assert!(wisp_llm::provider_from_template(PROBE_ACCOUNT, &[]).is_none());
        assert!(!PROBE_ACCOUNT.trim().is_empty());
    }

    #[test]
    fn fingerprints_are_short_and_stable() {
        let a = fingerprint("0123456789abcdef0123456789abcdef");
        assert_eq!(a.len(), 16);
        assert_eq!(a, fingerprint("0123456789abcdef0123456789abcdef"));
        assert_ne!(a, fingerprint("0123456789abcdef0123456789abcdee"));
    }

    #[test]
    fn the_summary_reads_endpoints_and_pubkey() {
        let p = plugins(&[(
            "updater",
            json!({ "pubkey": "cHVi", "endpoints": ["http://127.0.0.1:1/latest.json", "https://b"] }),
        )]);
        assert_eq!(
            updater_summary(&p),
            ("http://127.0.0.1:1/latest.json,https://b".to_owned(), "cHVi".to_owned())
        );
        assert_eq!(updater_summary(&HashMap::new()), (String::new(), String::new()));
    }
}
