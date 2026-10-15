//! 能力缓存的持久化实现：写在 settings.json 的 `providers[].models[].caps` 与 `vision_source` 中，
//! 经 `SettingsService::update` 写入，写入合并去抖 1 s。
//!
//! - `get` 先看尚未落盘的修改，再看 settings.json 里的值，因此 `update` 之后立即可见（第二次请求就按缓存构造参数）；
//! - 修改先进入内存里的待写表，1 s 后合并成一次 `SettingsService::update`（一次性定时，空闲时不存在定时器）；退出时
//!   `flush` 写掉剩余修改；
//! - Provider 不在 settings.json 里（草稿 Provider 的「测试连接」）时结论只留在内存（本次会话有效），不写盘；
//! - 模型不在 `models` 列表里时追加一个条目（启用）以便持久化——它确实被用过；
//! - 视觉能力按来源优先级覆盖：user > runtime > capability > keyword，低优先级的结论不覆盖高优先级的。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, Weak};
use std::time::Duration;

use parking_lot::Mutex;
use wisp_types::{ModelCaps, ModelEntry, Settings, Vision, VisionSource};

use crate::error::IpcError;
use crate::settings::SettingsService;

/// 合并写入的延迟
pub const FLUSH_DELAY: Duration = Duration::from_secs(1);

type Key = (String, String);

/// 一个模型尚未落盘的结论
#[derive(Debug, Clone, Default, PartialEq)]
struct Pending {
    caps: Option<ModelCaps>,
    vision: Option<(Vision, VisionSource)>,
}

impl Pending {
    fn merge(&mut self, newer: Self) {
        if newer.caps.is_some() {
            self.caps = newer.caps;
        }
        if newer.vision.is_some() {
            self.vision = newer.vision;
        }
    }
}

/// 内存里的三张表。**同一把锁**保护：「取当前值 → 修改 → 写回」在锁内完成，并发学到的两条结论
/// （例如翻译与 OCR 同时降级，一个学到 temperature、一个学到 max_tokens_field）不会互相覆盖
#[derive(Debug, Default)]
struct Queues {
    /// 待写入 settings.json 的修改
    pending: HashMap<Key, Pending>,
    /// 正在写入 settings.json 的一批：写盘期间仍然可见，`get` / `update` 不会退回到旧的磁盘值
    inflight: HashMap<Key, Pending>,
    /// 无法持久化（Provider 不在设置里）的结论，只在本次会话有效
    ephemeral: HashMap<Key, Pending>,
}

impl Queues {
    /// 内存里最新的结论（ephemeral < inflight < pending）
    fn overlay(&self, key: &Key) -> Pending {
        let mut out = self.ephemeral.get(key).cloned().unwrap_or_default();
        for layer in [&self.inflight, &self.pending] {
            if let Some(p) = layer.get(key) {
                out.merge(p.clone());
            }
        }
        out
    }

    fn enqueue(&mut self, key: Key, change: Pending) {
        self.pending.entry(key).or_default().merge(change);
    }
}

/// 以 settings.json 为后端的能力缓存
#[derive(Debug, Default)]
pub struct SettingsCapsStore {
    service: OnceLock<Weak<SettingsService>>,
    queues: Mutex<Queues>,
    /// 串行化 flush（定时写入与退出时的写入不交错：后到的一次等前一次写完，退出时不会漏写）
    flushing: Mutex<()>,
    /// 已安排了一次合并写入
    scheduled: AtomicBool,
}

impl SettingsCapsStore {
    /// 新建
    pub fn new() -> Self {
        Self::default()
    }

    /// 绑定所属的设置服务（`SettingsService::load` 构造完成后调用一次）
    pub fn bind(&self, service: Weak<SettingsService>) {
        let _ = self.service.set(service);
    }

    fn service(&self) -> Option<std::sync::Arc<SettingsService>> {
        self.service.get().and_then(Weak::upgrade)
    }

    fn key(provider_id: &str, model_id: &str) -> Key {
        (provider_id.to_owned(), model_id.to_owned())
    }

    /// settings.json 里的模型条目
    fn stored(&self, provider_id: &str, model_id: &str) -> Option<ModelEntry> {
        let svc = self.service()?;
        let settings = svc.get();
        settings
            .provider(provider_id)?
            .models
            .iter()
            .find(|m| m.id == model_id)
            .cloned()
    }

    /// 安排一次 [`FLUSH_DELAY`] 后的合并写入（一次性；写盘含 fsync，放到阻塞线程池，不占 tokio worker）
    fn schedule(&self) {
        if self.scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(service) = self.service.get().cloned() else {
            // 没有绑定服务（单测里直接 new）：不定时，等显式 flush
            self.scheduled.store(false, Ordering::Release);
            return;
        };
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(FLUSH_DELAY).await;
            let flushed = tauri::async_runtime::spawn_blocking(move || {
                if let Some(svc) = service.upgrade() {
                    svc.caps_flush();
                }
            })
            .await;
            if let Err(e) = flushed {
                log::warn!("model caps flush task failed: {e}");
            }
        });
    }

    /// 把待写的修改合并成一次 `SettingsService::update`（定时器到期与退出时调用；**阻塞**）
    pub fn flush(&self) {
        let _flushing = self.flushing.lock();
        self.scheduled.store(false, Ordering::Release);
        {
            let mut q = self.queues.lock();
            if q.pending.is_empty() {
                return;
            }
            let batch = std::mem::take(&mut q.pending);
            for (key, change) in batch {
                q.inflight.entry(key).or_default().merge(change);
            }
        }
        let Some(svc) = self.service() else {
            let mut q = self.queues.lock();
            let batch = std::mem::take(&mut q.inflight);
            for (key, change) in batch {
                q.ephemeral.entry(key).or_default().merge(change);
            }
            return;
        };
        let mut orphans: Vec<(Key, Pending)> = Vec::new();
        // 闭包在设置的写锁内执行：此时才读 inflight，期间被 `reset` 清掉的条目不会再写回去
        let result = svc.update(&mut |s: &mut Settings| {
            orphans.clear();
            let q = self.queues.lock();
            for (key, change) in &q.inflight {
                if !apply(s, key, change) {
                    orphans.push((key.clone(), change.clone()));
                }
            }
        });
        let mut q = self.queues.lock();
        let written = std::mem::take(&mut q.inflight);
        match result {
            Ok(_) => {
                for (key, change) in orphans {
                    q.ephemeral.entry(key).or_default().merge(change);
                }
            }
            Err(e) => {
                // 写盘失败：结论仍然有效，放回待写表（期间新到的修改优先），下次修改时再试
                log::warn!("model caps flush failed: {}", e.message);
                for (key, old) in written {
                    let slot = q.pending.entry(key).or_default();
                    let newer = std::mem::take(slot);
                    *slot = old;
                    slot.merge(newer);
                }
            }
        }
    }

    /// 清除某个模型学到的能力（`model_caps_reset`）：caps 恢复默认；运行时得出的视觉结论一并清除（用户设置的保留）
    pub fn reset(&self, provider_id: &str, model_id: &str) -> Result<(), IpcError> {
        let key = Self::key(provider_id, model_id);
        let had_ephemeral = {
            let mut q = self.queues.lock();
            q.pending.remove(&key);
            q.inflight.remove(&key);
            q.ephemeral.remove(&key).is_some()
        };
        let Some(svc) = self.service() else {
            return Ok(());
        };
        let exists = svc
            .get()
            .provider(provider_id)
            .is_some_and(|p| p.models.iter().any(|m| m.id == model_id));
        if !exists {
            return if had_ephemeral {
                Ok(())
            } else {
                Err(IpcError::not_found("model"))
            };
        }
        svc.update(&mut |s: &mut Settings| {
            if let Some(m) = model_mut(s, provider_id, model_id) {
                m.caps = ModelCaps::default();
                if m.vision_source == Some(VisionSource::Runtime) {
                    m.vision = Vision::Unknown;
                    m.vision_source = None;
                }
            }
        })?;
        log::info!("model caps reset provider={provider_id} model={model_id}");
        Ok(())
    }

    /// 删除某个 Provider 的全部待写 / 内存结论（删除 Provider 时）
    pub fn forget_provider(&self, provider_id: &str) {
        let mut q = self.queues.lock();
        q.pending.retain(|(p, _), _| p != provider_id);
        q.inflight.retain(|(p, _), _| p != provider_id);
        q.ephemeral.retain(|(p, _), _| p != provider_id);
    }
}

/// 视觉结论来源的优先级：user > runtime > capability > keyword
fn rank(s: VisionSource) -> u8 {
    match s {
        VisionSource::User => 3,
        VisionSource::Runtime => 2,
        VisionSource::Capability => 1,
        VisionSource::Keyword => 0,
    }
}

fn model_mut<'a>(s: &'a mut Settings, provider_id: &str, model_id: &str) -> Option<&'a mut ModelEntry> {
    s.providers
        .iter_mut()
        .find(|p| p.id == provider_id)?
        .models
        .iter_mut()
        .find(|m| m.id == model_id)
}

/// 把一个模型的修改写进设置；Provider 不存在时返回 false
fn apply(s: &mut Settings, (provider_id, model_id): &Key, change: &Pending) -> bool {
    let Some(provider) = s.providers.iter_mut().find(|p| &p.id == provider_id) else {
        return false;
    };
    let idx = match provider.models.iter().position(|m| &m.id == model_id) {
        Some(i) => i,
        None => {
            provider.models.push(ModelEntry::new(model_id.clone()));
            provider.models.len() - 1
        }
    };
    let model = &mut provider.models[idx];
    if let Some(caps) = &change.caps {
        model.caps = caps.clone();
    }
    if let Some((vision, source)) = change.vision {
        model.vision = vision;
        model.vision_source = Some(source);
    }
    true
}

impl wisp_llm::CapsStore for SettingsCapsStore {
    fn get(&self, provider_id: &str, model_id: &str) -> ModelCaps {
        let key = Self::key(provider_id, model_id);
        if let Some(caps) = self.queues.lock().overlay(&key).caps {
            return caps;
        }
        self.stored(provider_id, model_id).map(|m| m.caps).unwrap_or_default()
    }

    fn update(&self, provider_id: &str, model_id: &str, f: &mut dyn FnMut(&mut ModelCaps)) {
        let key = Self::key(provider_id, model_id);
        let changed = {
            // 读当前值、修改、入队都在同一把锁内：并发的两次学习各自基于对方的结果，不会整份互相覆盖
            let mut q = self.queues.lock();
            let before = q
                .overlay(&key)
                .caps
                .unwrap_or_else(|| self.stored(provider_id, model_id).map(|m| m.caps).unwrap_or_default());
            let mut caps = before.clone();
            f(&mut caps);
            let changed = caps != before;
            if changed {
                q.enqueue(
                    key,
                    Pending {
                        caps: Some(caps),
                        vision: None,
                    },
                );
            }
            changed
        };
        if changed {
            log::debug!("model caps learned provider={provider_id} model={model_id}");
            self.schedule();
        }
    }

    fn set_vision(&self, provider_id: &str, model_id: &str, vision: Vision, source: VisionSource) {
        let key = Self::key(provider_id, model_id);
        let changed = {
            let mut q = self.queues.lock();
            let (cur_vision, cur_source) = match q.overlay(&key).vision {
                Some((v, s)) => (v, Some(s)),
                None => self
                    .stored(provider_id, model_id)
                    .map_or((Vision::Unknown, None), |m| (m.vision, m.vision_source)),
            };
            if let Some(cur) = cur_source
                && rank(source) < rank(cur)
            {
                log::debug!(
                    "vision {source:?} ignored for provider={provider_id} model={model_id}: {cur:?} has priority"
                );
                return;
            }
            let changed = cur_vision != vision || cur_source != Some(source);
            if changed {
                q.enqueue(
                    key,
                    Pending {
                        caps: None,
                        vision: Some((vision, source)),
                    },
                );
            }
            changed
        };
        if changed {
            self.schedule();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use wisp_types::MaxTokensField;

    use super::*;
    use crate::settings::service::tests::{provider, test_service};

    #[test]
    fn update_is_visible_immediately_and_persisted_on_flush() {
        let (_dir, svc) = test_service();
        svc.update(&mut |s| s.providers.push(provider("p", &["m"])))
            .expect("seed");
        let caps = svc.caps_store();
        caps.update("p", "m", &mut |c| c.temperature = Some(false));
        assert_eq!(caps.get("p", "m").temperature, Some(false), "未落盘也立即可见");
        assert_eq!(
            svc.get().providers[0].models[0].caps.temperature,
            None,
            "合并写入之前 settings 不变"
        );
        svc.caps_flush();
        assert_eq!(svc.get().providers[0].models[0].caps.temperature, Some(false));
        // 「重启」：重新从磁盘加载
        let reloaded = SettingsService::load_for_tests(svc.paths());
        assert_eq!(reloaded.caps_store().get("p", "m").temperature, Some(false));
    }

    #[test]
    fn several_updates_coalesce_into_one_write() {
        let (_dir, svc) = test_service();
        svc.update(&mut |s| s.providers.push(provider("p", &["m"])))
            .expect("seed");
        let mut rx = svc.changes();
        rx.borrow_and_update();
        let caps = svc.caps_store();
        caps.update("p", "m", &mut |c| c.temperature = Some(false));
        caps.update("p", "m", &mut |c| c.max_tokens_field = Some(MaxTokensField::MaxTokens));
        caps.set_vision("p", "m", Vision::No, VisionSource::Runtime);
        svc.caps_flush();
        assert!(rx.has_changed().expect("open"));
        let m = &svc.get().providers[0].models[0];
        assert_eq!(m.caps.temperature, Some(false));
        assert_eq!(m.caps.max_tokens_field, Some(MaxTokensField::MaxTokens));
        assert_eq!((m.vision, m.vision_source), (Vision::No, Some(VisionSource::Runtime)));
    }

    #[test]
    fn unknown_model_is_appended_and_draft_provider_stays_in_memory() {
        let (_dir, svc) = test_service();
        svc.update(&mut |s| s.providers.push(provider("p", &[]))).expect("seed");
        let caps = svc.caps_store();
        caps.update("p", "new-model", &mut |c| c.stream_options = Some(false));
        caps.update("draft", "m", &mut |c| c.stream_options = Some(false));
        svc.caps_flush();
        let p = &svc.get().providers[0];
        assert_eq!(p.models.len(), 1);
        assert_eq!(p.models[0].id, "new-model");
        assert!(p.models[0].enabled);
        assert!(svc.get().provider("draft").is_none(), "草稿 Provider 不写进设置");
        assert_eq!(caps.get("draft", "m").stream_options, Some(false), "但本次会话仍然有效");
    }

    #[test]
    fn vision_respects_source_priority() {
        let (_dir, svc) = test_service();
        let mut p = provider("p", &["m"]);
        p.models[0].vision = Vision::Yes;
        p.models[0].vision_source = Some(VisionSource::User);
        svc.update(&mut |s| s.providers.push(p.clone())).expect("seed");
        let caps = svc.caps_store();
        caps.set_vision("p", "m", Vision::No, VisionSource::Runtime);
        svc.caps_flush();
        let m = &svc.get().providers[0].models[0];
        assert_eq!((m.vision, m.vision_source), (Vision::Yes, Some(VisionSource::User)));
        // 关键词推断不能覆盖运行时结论
        caps.set_vision("p", "x", Vision::No, VisionSource::Runtime);
        caps.set_vision("p", "x", Vision::Yes, VisionSource::Keyword);
        svc.caps_flush();
        let x = svc.get().providers[0]
            .models
            .iter()
            .find(|m| m.id == "x")
            .cloned()
            .expect("x");
        assert_eq!((x.vision, x.vision_source), (Vision::No, Some(VisionSource::Runtime)));
    }

    #[test]
    fn concurrent_learning_keeps_every_conclusion() {
        let (_dir, svc) = test_service();
        svc.update(&mut |s| s.providers.push(provider("p", &["m"])))
            .expect("seed");
        let caps = svc.caps_store();
        // 两个线程同时学到同一个模型的不同结论（翻译与 OCR 同时降级），中间还穿插着写盘
        let threads: Vec<_> = (0..2)
            .map(|i| {
                let caps = Arc::clone(&caps);
                let svc = Arc::clone(&svc);
                std::thread::spawn(move || {
                    for round in 0..50 {
                        if i == 0 {
                            caps.update("p", "m", &mut |c| c.temperature = Some(round % 2 == 0));
                        } else {
                            caps.update("p", "m", &mut |c| {
                                c.max_tokens_field = Some(if round % 2 == 0 {
                                    MaxTokensField::MaxTokens
                                } else {
                                    MaxTokensField::MaxCompletionTokens
                                });
                            });
                        }
                        if round % 10 == 0 {
                            svc.caps_flush();
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().expect("join");
        }
        // 两条结论都在（最后一轮 round = 49：temperature = false、max_completion_tokens）
        let learned = caps.get("p", "m");
        assert_eq!(learned.temperature, Some(false));
        assert_eq!(learned.max_tokens_field, Some(MaxTokensField::MaxCompletionTokens));
        svc.caps_flush();
        let m = &svc.get().providers[0].models[0];
        assert_eq!(m.caps.temperature, Some(false));
        assert_eq!(m.caps.max_tokens_field, Some(MaxTokensField::MaxCompletionTokens));
    }

    #[test]
    fn conclusions_stay_visible_while_being_written() {
        let (_dir, svc) = test_service();
        svc.update(&mut |s| s.providers.push(provider("p", &["m"])))
            .expect("seed");
        let caps = svc.caps_store();
        caps.update("p", "m", &mut |c| c.temperature = Some(false));
        // 写盘进行中（持有设置的写锁）：结论在 inflight 里，读取与新的学习都基于它
        let guard = svc.write_lock_for_tests();
        let flusher = {
            let svc = Arc::clone(&svc);
            std::thread::spawn(move || svc.caps_flush())
        };
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert_eq!(caps.get("p", "m").temperature, Some(false));
        caps.update("p", "m", &mut |c| c.stream_options = Some(false));
        drop(guard);
        flusher.join().expect("join");
        svc.caps_flush();
        let m = &svc.get().providers[0].models[0];
        assert_eq!(m.caps.temperature, Some(false));
        assert_eq!(m.caps.stream_options, Some(false));
    }

    #[test]
    fn reset_clears_learned_caps_and_runtime_vision() {
        let (_dir, svc) = test_service();
        svc.update(&mut |s| s.providers.push(provider("p", &["m"])))
            .expect("seed");
        let caps = svc.caps_store();
        caps.update("p", "m", &mut |c| c.temperature = Some(false));
        caps.set_vision("p", "m", Vision::No, VisionSource::Runtime);
        svc.caps_flush();
        svc.caps_reset("p", "m").expect("reset");
        let m = &svc.get().providers[0].models[0];
        assert_eq!(m.caps, ModelCaps::default());
        assert_eq!((m.vision, m.vision_source), (Vision::Unknown, None));
        assert_eq!(
            svc.caps_reset("p", "nope").expect_err("missing").code,
            crate::error::IpcErrorCode::NotFound
        );
    }
}
