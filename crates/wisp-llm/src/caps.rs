//! 能力缓存接口：desktop 实现持久化（写回 settings.json 的 models[].caps）。

use std::collections::HashMap;

use parking_lot::Mutex;
use wisp_types::{ModelCaps, Vision, VisionSource};

/// 能力缓存的持久化接口，由 desktop 实现（写回 settings.json 的 models[].caps）
pub trait CapsStore: Send + Sync {
    fn get(&self, provider_id: &str, model_id: &str) -> ModelCaps;
    fn update(&self, provider_id: &str, model_id: &str, f: &mut dyn FnMut(&mut ModelCaps));
    fn set_vision(&self, provider_id: &str, model_id: &str, vision: Vision, source: VisionSource);
}

type ModelKey = (String, String);

/// 内存版能力缓存（测试用）
#[derive(Default)]
pub struct MemoryCapsStore {
    caps: Mutex<HashMap<ModelKey, ModelCaps>>,
    vision: Mutex<HashMap<ModelKey, (Vision, VisionSource)>>,
}

impl MemoryCapsStore {
    /// 新建空缓存
    pub fn new() -> Self {
        Self::default()
    }

    /// 读取 set_vision 写入的结果（测试断言用）
    pub fn vision(&self, provider_id: &str, model_id: &str) -> Option<(Vision, VisionSource)> {
        self.vision
            .lock()
            .get(&(provider_id.to_owned(), model_id.to_owned()))
            .copied()
    }
}

impl CapsStore for MemoryCapsStore {
    fn get(&self, provider_id: &str, model_id: &str) -> ModelCaps {
        self.caps
            .lock()
            .get(&(provider_id.to_owned(), model_id.to_owned()))
            .cloned()
            .unwrap_or_default()
    }

    fn update(&self, provider_id: &str, model_id: &str, f: &mut dyn FnMut(&mut ModelCaps)) {
        let mut map = self.caps.lock();
        f(map.entry((provider_id.to_owned(), model_id.to_owned())).or_default());
    }

    fn set_vision(&self, provider_id: &str, model_id: &str, vision: Vision, source: VisionSource) {
        self.vision
            .lock()
            .insert((provider_id.to_owned(), model_id.to_owned()), (vision, source));
    }
}
