//! 16 ms 合并器：纯逻辑，由 desktop 的注册表驱动定时。

use std::time::{Duration, Instant};

use wisp_types::StreamEvent;

/// 16 ms 合并器：Delta 进缓冲；其余事件先把缓冲冲刷掉再原样放行
pub struct Coalescer {
    window: Duration,
    buf: String,
    deadline: Option<Instant>,
}

impl Coalescer {
    /// `window`：合并窗口（通常 16 ms）
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            buf: String::new(),
            deadline: None,
        }
    }

    /// Delta 进缓冲；其余事件先把缓冲冲刷掉再原样放行
    pub fn push(&mut self, ev: StreamEvent, now: Instant) -> Vec<StreamEvent> {
        match ev {
            StreamEvent::Delta { text } => {
                if self.buf.is_empty() {
                    self.deadline = Some(now + self.window);
                }
                self.buf.push_str(&text);
                Vec::new()
            }
            other => {
                let mut out = Vec::with_capacity(2);
                if let Some(flushed) = self.flush() {
                    out.push(flushed);
                }
                out.push(other);
                out
            }
        }
    }

    /// 到期时冲刷
    pub fn poll(&mut self, now: Instant) -> Option<StreamEvent> {
        match self.deadline {
            Some(d) if now >= d => self.flush(),
            _ => None,
        }
    }

    /// 下一次需要 poll 的时刻（缓冲为空时为 None）
    pub fn next_deadline(&self) -> Option<Instant> {
        self.deadline
    }

    fn flush(&mut self) -> Option<StreamEvent> {
        self.deadline = None;
        if self.buf.is_empty() {
            None
        } else {
            Some(StreamEvent::Delta {
                text: std::mem::take(&mut self.buf),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use wisp_types::StreamEvent;

    use super::Coalescer;

    fn delta(t: &str) -> StreamEvent {
        StreamEvent::Delta { text: t.to_owned() }
    }

    fn done() -> StreamEvent {
        StreamEvent::Done {
            elapsed_ms: 0,
            usage: None,
            truncated: None,
            cached: false,
            no_text: false,
        }
    }

    #[test]
    fn one_token_per_ms_yields_at_most_about_63_deltas_per_second() {
        let t0 = Instant::now();
        let mut c = Coalescer::new(Duration::from_millis(16));
        let mut out = Vec::new();
        let mut expected = String::new();
        for ms in 0..1000u64 {
            let now = t0 + Duration::from_millis(ms);
            // 驱动方在到期时 poll，然后再推入新的 token
            out.extend(c.poll(now));
            let token = format!("{}", ms % 10);
            expected.push_str(&token);
            assert!(c.push(delta(&token), now).is_empty(), "Delta 只进缓冲");
        }
        let in_first_second = out.len();
        assert!(in_first_second <= 63, "{in_first_second} deltas in 1 s");
        // Done 之前先把剩余内容冲刷出去，最后一段不丢
        let tail = c.push(done(), t0 + Duration::from_millis(1000));
        assert_eq!(tail.len(), 2);
        assert!(matches!(tail[1], StreamEvent::Done { .. }));
        out.extend(tail);
        let text: String = out
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Delta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, expected);
        assert!(c.next_deadline().is_none());
    }

    #[test]
    fn deadline_starts_with_the_first_buffered_delta() {
        let t0 = Instant::now();
        let mut c = Coalescer::new(Duration::from_millis(16));
        assert_eq!(c.next_deadline(), None);
        assert!(c.poll(t0).is_none());
        let _ = c.push(delta("a"), t0);
        let _ = c.push(delta("b"), t0 + Duration::from_millis(10));
        assert_eq!(c.next_deadline(), Some(t0 + Duration::from_millis(16)));
        assert!(c.poll(t0 + Duration::from_millis(15)).is_none());
        assert_eq!(c.poll(t0 + Duration::from_millis(16)), Some(delta("ab")));
        assert_eq!(c.next_deadline(), None);
    }

    #[test]
    fn non_delta_events_pass_through_after_flush() {
        let t0 = Instant::now();
        let mut c = Coalescer::new(Duration::from_millis(16));
        let notice = StreamEvent::Notice {
            kind: wisp_types::NoticeKind::Retried,
        };
        assert_eq!(c.push(notice.clone(), t0), vec![notice.clone()]);
        let _ = c.push(delta("x"), t0);
        assert_eq!(c.push(notice.clone(), t0), vec![delta("x"), notice]);
    }
}
