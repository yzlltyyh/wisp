//! 监听回调 → 工作任务。
//!
//! 平台回调运行在平台线程上（macOS 的 DispatchSource、Windows 的剪贴板窗口线程、Linux 的 `wisp-x11-cb`），只做
//! [`WatchSignal::post`]：记下最新序号并唤醒工作任务，不阻塞、不读取、不写剪贴板（平台回调里不做阻塞工作）。
//!
//! 工作任务平时停在 `Notify` 上（零唤醒）；收到信号后在阻塞线程池里读取并记录（`Engine::capture`）。连续多次变更
//! 来不及处理时只处理最新的那一次——读到的本来就是当前剪贴板。读取失败（数据提供方超时、Windows 剪贴板被占用）时
//! 按 [`RETRY_DELAYS`] 退避重试，最多 3 次；等待期间有了更新的变更就放弃这一次，交给下一轮。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Notify;

use super::engine::Engine;

/// 读取失败后的退避（最多 3 次）。一次性、有界，只在读取失败后出现（不是常驻定时器）
pub(crate) const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(120),
    Duration::from_millis(360),
    Duration::from_millis(1000),
];

/// 一次读取的结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Capture {
    /// 处理完毕（已记录、被跳过，或遇到不可重试的错误）
    Done,
    /// 读取失败，可以稍后重试
    Retry,
}

/// 平台回调与工作任务之间的转交点
#[derive(Default)]
pub(crate) struct WatchSignal {
    latest: AtomicU64,
    posted: AtomicBool,
    notify: Notify,
}

impl WatchSignal {
    /// 平台回调里调用：只记序号并唤醒（不阻塞、不分配）
    pub(crate) fn post(&self, change_id: u64) {
        self.latest.store(change_id, Ordering::Release);
        self.posted.store(true, Ordering::Release);
        self.notify.notify_one();
    }

    /// 最近一次回调的序号
    pub(crate) fn latest(&self) -> Option<u64> {
        self.posted
            .load(Ordering::Acquire)
            .then(|| self.latest.load(Ordering::Acquire))
    }

    /// 等下一次回调，返回届时最新的序号
    pub(crate) async fn next(&self) -> u64 {
        loop {
            self.notify.notified().await;
            if let Some(id) = self.latest() {
                return id;
            }
        }
    }
}

/// 工作任务：常驻，平时停在 [`WatchSignal::next`] 上
pub(crate) async fn run(engine: Arc<Engine>) {
    loop {
        let change = engine.signal().next().await;
        process(&engine, change).await;
    }
}

/// 处理一次变更：重复的序号直接忽略；读取失败时按 `delays` 退避重试
pub(crate) async fn process_with(engine: &Arc<Engine>, change: u64, delays: &[Duration]) {
    if !engine.begin_change(change) {
        return;
    }
    let mut attempt = 0usize;
    loop {
        let e = Arc::clone(engine);
        let step = match tauri::async_runtime::spawn_blocking(move || e.capture(change)).await {
            Ok(step) => step,
            Err(err) => {
                log::warn!("clipboard capture task failed: {err}");
                Capture::Done
            }
        };
        if step == Capture::Done {
            return;
        }
        let Some(delay) = delays.get(attempt) else {
            log::info!("clipboard read failed after {attempt} retries; change skipped");
            return;
        };
        attempt += 1;
        tokio::time::sleep(*delay).await;
        if engine.signal().latest() != Some(change) {
            // 等待期间又有新的复制：读到的会是新内容，交给下一轮
            return;
        }
    }
}

async fn process(engine: &Arc<Engine>, change: u64) {
    process_with(engine, change, &RETRY_DELAYS).await;
}
