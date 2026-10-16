//! Wisp 的共享数据类型。
//!
//! 这里定义所有跨 crate、跨进程（IPC）以及持久化用到的数据类型、它们的 `Default` 实现
//! 和少量纯函数。序列化统一使用 snake_case：结构体字段名本身就是 snake_case，
//! 枚举值逐个显式 rename；带数据的联合类型一律使用内部标签。
//!
//! `Option` 字段序列化为 `null`（不加 `skip_serializing_if`），TS 一侧统一写成 `T | null`；
//! 只有配置文档里的可选字段加 `#[serde(default)]`，以容忍手工编辑时的缺失。
//!
//! 任何字段的增删改都必须同步 `apps/desktop/src/shared/ipc/types.ts`，并重新生成黄金样例
//! （`UPDATE_GOLDEN=1 cargo test -p wisp-types`）。
//!
//! ## Rust 下发的 i18n key
//!
//! 以 `_key` 结尾的字段与 `LlmError.hint` 都是**前端** i18n key，第一段必须是前端 locales 的命名空间
//! （`apps/desktop/src/locales/<lang>/<ns>.json`），文案由该命名空间的拥有者维护：
//!
//! | 字段 | key 形状 | 命名空间 | 产出方 |
//! | --- | --- | --- | --- |
//! | `Capability.reason_key` | `errors.platform.<reason>` | errors.json | `wisp-platform` |
//! | `NotReady::SelectionUnavailable.reason_key` / `IpcError{platform}.message` | `errors.platform.<reason>`（`wisp_platform::reason`） | errors.json | `wisp-platform`，经编排器 / 剪贴板服务 |
//! | `NotReady::CaptureFailed.message` | `errors.capture.<name>` 或 `errors.platform.wayland_capture` | errors.json | 编排器（`orchestrator::not_ready::capture_not_ready`） |
//! | `LlmError.hint` | `errors.hint.<name>` | errors.json | `wisp-llm` |
//! | `ProviderTemplate.name_key` / `RegionVariant.label_key` | `settings.providers.template.<id>` / `settings.providers.region.<name>` | settings.json | `wisp-llm` |
//! | `HotkeyStatus.reason_key` / `ExtTriggerStatus.reason_key` / `Conflict.message_key` | `hotkeys.reason.*` / `hotkeys.ext.*` / `hotkeys.conflict.*`，或 `errors.platform.*` | hotkeys.json / errors.json | desktop 的 `hotkeys` |
//!
//! 新增 key 时要在对应命名空间里同时补齐 zh-CN / en-US 文案；`scripts/check-i18n.mjs`
//! 扫描 Rust 源码里这些前缀的字符串字面量，校验两种语言的 locales 都有对应条目。前端拿到的是运行期字符串，
//! 用 `t(key as TKey)` 之前应先判断 key 是否存在（缺失时显示通用文案，不显示 key 本身）。
//! 托盘、原生对话框、通知等由 Rust 自己显示的文案（`tray.*` 等）只在 desktop 的 `app/i18n.rs` 静态表里，不进前端 locales。

pub mod app;
pub mod clip;
pub mod defaults;
pub mod geom;
pub mod hotkey;
pub mod ipc;
pub mod lang;
pub mod lenient;
pub mod llm;
pub mod local;
mod macros;
pub mod perm;
pub mod settings;
pub mod style;
pub mod sync;

pub use app::*;
pub use clip::*;
pub use geom::*;
pub use hotkey::*;
pub use ipc::*;
pub use lang::*;
pub use lenient::{ConfigIssue, from_value_lenient, parse_lenient};
pub use llm::*;
pub use local::*;
pub use perm::*;
pub use settings::*;
pub use style::*;
pub use sync::*;
