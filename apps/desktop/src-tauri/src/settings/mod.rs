//! 设置服务：settings.json（用户可手改，JSON Merge Patch 更新，notify 热加载）、styles.json、glossary.json、
//! state.json（运行态），钥匙串与能力缓存。所有 JSON 文件原子写入（先写 *.tmp 再 rename）。

pub mod caps_store;
pub mod fsio;
pub mod glossary_store;
pub mod keychain;
pub mod migrate;
pub mod patch;
pub mod service;
pub mod state_store;
pub mod styles_store;
pub mod validate;
pub mod watcher;

pub use service::SettingsService;
