//! 连接、首 token、空闲三个计时器。
//!
//! - 连接超时由 reqwest 的 `connect_timeout` 负责（按 Provider 的 `connect_ms` 取缓存的 Client）；
//! - 首 token：从收到响应头开始计时，到模型第一次产出内容（可见文本，或推理模型的思考内容）为止；
//!   ping 与注释行**不会**重置它，所以「只有心跳」的流会在 first_token_ms 时超时，而不是拖到 idle_ms；
//! - 空闲：两次收到字节之间的间隔，任何字节（包括 ping / 注释）都会重置；
//! - 结束宽限：看到结束标记后最多再等 2 s 的 `[DONE]` / usage / EOF。

use std::time::Duration;

use tokio::time::Instant;
use wisp_types::Timeouts;

/// 看到结束标记之后等待 `[DONE]` / usage / EOF 的上限
pub(crate) const FINISH_GRACE: Duration = Duration::from_secs(2);

/// 计时器到期的原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Expiry {
    FirstToken,
    Idle,
    /// 结束宽限期到：按正常结束处理
    Grace,
}

/// 流读取阶段的计时器
#[derive(Debug, Clone)]
pub(crate) struct StreamTimers {
    first_token: Duration,
    idle: Duration,
    headers_at: Instant,
    last_bytes: Instant,
    got_token: bool,
    grace_until: Option<Instant>,
}

impl StreamTimers {
    /// `now`：收到响应头的时刻
    pub fn new(t: &Timeouts, now: Instant) -> Self {
        Self {
            first_token: Duration::from_millis(u64::from(t.first_token_ms.max(1))),
            idle: Duration::from_millis(u64::from(t.idle_ms.max(1))),
            headers_at: now,
            last_bytes: now,
            got_token: false,
            grace_until: None,
        }
    }

    /// 收到任意字节
    pub fn on_bytes(&mut self, now: Instant) {
        self.last_bytes = now;
    }

    /// 模型产出了内容
    pub fn on_token(&mut self) {
        self.got_token = true;
    }

    /// 进入结束宽限期（只在第一次调用时生效）
    pub fn start_grace(&mut self, now: Instant) {
        self.grace_until.get_or_insert(now + FINISH_GRACE);
    }

    /// 下一个到期时刻
    pub fn deadline(&self) -> Instant {
        let mut d = self.last_bytes + self.idle;
        if !self.got_token {
            d = d.min(self.headers_at + self.first_token);
        }
        if let Some(g) = self.grace_until {
            d = d.min(g);
        }
        d
    }

    /// 到期时是哪一个计时器
    pub fn expiry(&self, now: Instant) -> Expiry {
        if self.grace_until.is_some_and(|g| now >= g) {
            Expiry::Grace
        } else if !self.got_token && now >= self.headers_at + self.first_token {
            Expiry::FirstToken
        } else {
            Expiry::Idle
        }
    }

    pub fn first_token_secs(&self) -> u64 {
        self.first_token.as_secs().max(1)
    }

    pub fn idle_secs(&self) -> u64 {
        self.idle.as_secs().max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timeouts() -> Timeouts {
        Timeouts {
            connect_ms: 1_000,
            first_token_ms: 30_000,
            idle_ms: 60_000,
        }
    }

    #[test]
    fn pings_do_not_extend_first_token() {
        let t0 = Instant::now();
        let mut t = StreamTimers::new(&timeouts(), t0);
        // 每 10 s 一个 ping：空闲计时被重置，但首 token 仍在 30 s 到期
        for i in 1..=5u64 {
            t.on_bytes(t0 + Duration::from_secs(i * 10));
        }
        assert_eq!(t.deadline(), t0 + Duration::from_secs(30));
        assert_eq!(t.expiry(t0 + Duration::from_secs(30)), Expiry::FirstToken);
    }

    #[test]
    fn idle_after_first_token() {
        let t0 = Instant::now();
        let mut t = StreamTimers::new(&timeouts(), t0);
        t.on_token();
        t.on_bytes(t0 + Duration::from_secs(5));
        assert_eq!(t.deadline(), t0 + Duration::from_secs(65));
        assert_eq!(t.expiry(t0 + Duration::from_secs(65)), Expiry::Idle);
    }

    #[test]
    fn grace_wins_when_earlier() {
        let t0 = Instant::now();
        let mut t = StreamTimers::new(&timeouts(), t0);
        t.on_token();
        t.start_grace(t0 + Duration::from_secs(1));
        t.start_grace(t0 + Duration::from_secs(9));
        assert_eq!(t.deadline(), t0 + Duration::from_secs(3));
        assert_eq!(t.expiry(t0 + Duration::from_secs(3)), Expiry::Grace);
    }
}
