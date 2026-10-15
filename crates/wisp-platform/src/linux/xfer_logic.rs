//! 选区读取的纯状态机（只依赖 std）。
//!
//! 一次读取 = ConvertSelection → 等 owner 的 SelectionNotify → 读属性；数据大时 owner 改用 INCR 分块：先写一个
//! 类型为 INCR 的属性（值是总大小的下限），之后每当请求方删除属性就写下一块，长度为 0 的块表示结束。
//! 这里只做「读到了什么 → 下一步是什么」的判定，X 往返由 `x11/transfer.rs` 完成，因此这些转移规则在 macOS 开发机上
//! 也能单测（`tests/smoke_linux.rs` 用 `#[path]` 引入本文件）。

/// X11 原子（与 x11rb 的 `Atom` 相同，都是 u32）
pub type Atom = u32;
/// 预定义原子 None
pub const ATOM_NONE: Atom = 0;
/// 预定义原子 STRING（Latin-1 文本）
pub const ATOM_STRING: Atom = 31;

/// 单个传输的结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    Data {
        ty: Atom,
        format: u8,
        bytes: Vec<u8>,
        /// 按「截断」模式读取时超过上限被截断
        truncated: bool,
    },
    /// 拒绝模式下超过上限（附已知大小）
    TooLarge(usize),
    /// owner 拒绝转换（property = None）或数据类型无效
    Refused,
    /// 超时
    Timeout,
}

/// 超过上限时的处理
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitMode {
    /// 保留前 N 字节并标记截断（文本）
    Truncate,
    /// 整体放弃（HTML / RTF / 图片 / 快照）
    Reject,
}

/// 一个传输的状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// 已发出 ConvertSelection，等 owner 应答
    WaitNotify,
    /// INCR 接收中：类型与格式取自第一块
    Incr {
        ty: Atom,
        format: u8,
        data: Vec<u8>,
    },
    Done(Fetched),
}

/// 收到 SelectionNotify、看过属性的类型与长度之后要做什么
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyPlan {
    /// owner 拒绝或属性不存在
    Refused,
    /// 属性类型是 INCR：读取并删除它（表示开始接收），再按提示大小决定是否继续
    StartIncr,
    /// 拒绝模式下超过上限：删除属性后放弃
    TooLarge(usize),
    /// 读取前 `want` 字节；`truncated` 时读完要手动删除属性（剩余数据不会被自动删除）
    Read { want: usize, truncated: bool },
}

/// SelectionNotify 之后的决定。`property_none`：通知里的 property 为 None（owner 拒绝）；`head_ty` / `total`：
/// 长度为 0 的 GetProperty 返回的类型与剩余字节数
pub fn plan_notify(
    property_none: bool,
    head_ty: Atom,
    total: usize,
    incr_atom: Atom,
    limit: usize,
    mode: LimitMode,
) -> NotifyPlan {
    if property_none || head_ty == ATOM_NONE {
        return NotifyPlan::Refused;
    }
    if head_ty == incr_atom {
        return NotifyPlan::StartIncr;
    }
    if total > limit {
        return match mode {
            LimitMode::Reject => NotifyPlan::TooLarge(total),
            LimitMode::Truncate => NotifyPlan::Read {
                want: limit,
                truncated: true,
            },
        };
    }
    NotifyPlan::Read {
        want: total,
        truncated: false,
    }
}

/// INCR 开始：`hint` 是 owner 声明的总大小下限。拒绝模式下声明就超限的直接放弃（不浪费带宽）
pub fn start_incr(hint: usize, limit: usize, mode: LimitMode) -> State {
    if hint > limit && mode == LimitMode::Reject {
        return State::Done(Fetched::TooLarge(hint));
    }
    State::Incr {
        ty: ATOM_NONE,
        format: 8,
        // 预分配不超过 4 MB：提示值只是下限，也可能是对端随意给的
        data: Vec::with_capacity(hint.min(limit).min(4 * 1024 * 1024)),
    }
}

/// 普通（非 INCR）读取完成
pub fn finish_read(ty: Atom, format: u8, mut bytes: Vec<u8>, want: usize, truncated: bool) -> State {
    bytes.truncate(want);
    State::Done(Fetched::Data {
        ty,
        format,
        bytes,
        truncated,
    })
}

/// INCR 的一块。`chunk` 为 None 表示属性当前不存在（类型为 None）——那是迟到 / 重复的 NewValue 通知，
/// 忽略并继续等待；只有「类型有效、长度为 0」的块才表示传输结束
pub fn incr_chunk(state: State, chunk: Option<(Atom, u8, Vec<u8>)>, limit: usize, mode: LimitMode) -> State {
    let State::Incr {
        mut ty,
        mut format,
        mut data,
    } = state
    else {
        return state;
    };
    let Some((chunk_ty, chunk_format, bytes)) = chunk else {
        return State::Incr { ty, format, data };
    };
    if bytes.is_empty() {
        return State::Done(Fetched::Data {
            ty: if ty == ATOM_NONE { chunk_ty } else { ty },
            format,
            bytes: data,
            truncated: false,
        });
    }
    if ty == ATOM_NONE {
        ty = chunk_ty;
        format = chunk_format;
    }
    if data.len() + bytes.len() > limit {
        return match mode {
            LimitMode::Reject => State::Done(Fetched::TooLarge(data.len() + bytes.len())),
            LimitMode::Truncate => {
                let room = limit - data.len();
                data.extend_from_slice(&bytes[..room]);
                State::Done(Fetched::Data {
                    ty,
                    format,
                    bytes: data,
                    truncated: true,
                })
            }
        };
    }
    data.extend_from_slice(&bytes);
    State::Incr { ty, format, data }
}

/// 这次 SelectionNotify 的处理是否已经完成了 INCR 握手。`StartIncr` 会读取并**删除** INCR 属性，owner 随即开始
/// 写第一块——即使紧接着因为声明大小超限而放弃，对端也会往这个窗口写，必须计入 [`may_still_write`] 的
/// `entered_incr`（否则窗口被放回池中，旧 owner 的数据块会混进下一次读取）
pub fn handshake_started(plan: NotifyPlan) -> bool {
    plan == NotifyPlan::StartIncr
}

/// 传输结束时对端是否可能还在往这个属性里写（超时、INCR 中途放弃）：这样的请求方窗口不能复用。
/// `entered_incr`：完成过 INCR 握手（见 [`handshake_started`]）
pub fn may_still_write(state: &State, entered_incr: bool) -> bool {
    match state {
        State::WaitNotify | State::Incr { .. } | State::Done(Fetched::Timeout) => true,
        // INCR 中途因超限放弃：对端还在等这边删除属性
        State::Done(Fetched::TooLarge(_)) => entered_incr,
        State::Done(Fetched::Data { truncated, .. }) => entered_incr && *truncated,
        State::Done(Fetched::Refused) => false,
    }
}

/// 作为选区 owner 时，是否接受时间为 `time` 的转换请求或清除通知（ICCCM：早于 Wisp 成为 owner 的请求属于上一任
/// owner）。`owned_at` 是成为 owner 的服务器时间，取不到时为 None——此时不做时间校验（宁可多响应，也不能让
/// `write` 返回 Ok 之后的粘贴全部被拒绝）。`time` 为 CurrentTime（0）时总是接受。服务器时间是 32 位毫秒计数，
/// 约 49.7 天回绕一次，按回绕比较
pub fn owner_time_ok(owned_at: Option<u32>, time: u32) -> bool {
    time == 0 || owned_at.is_none_or(|t| time.wrapping_sub(t) < 0x8000_0000)
}

/// Latin-1 → String
pub fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// 字节解码为文本：STRING 按 Latin-1；其余（UTF8_STRING、text/plain;charset=utf-8、text/plain）按 UTF-8。
/// 末尾被截断的多字节字符直接丢弃，其余非法字节替换为 U+FFFD
pub fn decode_text(ty: Atom, bytes: Vec<u8>) -> String {
    if ty == ATOM_STRING {
        return latin1(&bytes);
    }
    match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => {
            let mut bytes = e.into_bytes();
            let cut = incomplete_tail(&bytes);
            bytes.truncate(bytes.len() - cut);
            String::from_utf8_lossy(&bytes).into_owned()
        }
    }
}

/// 末尾「合法但不完整」的 UTF-8 序列的字节数（0 表示末尾没有被截断的字符）
fn incomplete_tail(bytes: &[u8]) -> usize {
    let len = bytes.len();
    for back in 1..=len.min(3) {
        let b = bytes[len - back];
        if b & 0xC0 == 0x80 {
            // 延续字节：继续往前找首字节
            continue;
        }
        let need = match b {
            0xF0.. => 4,
            0xE0.. => 3,
            0xC0.. => 2,
            _ => 1,
        };
        // 首字节要求的长度超过剩余字节，且这几个字节是某个合法字符的前缀（「意外结束」而不是非法字节）
        let prefix = need > back
            && std::str::from_utf8(&bytes[len - back..])
                .is_err_and(|e| e.valid_up_to() == 0 && e.error_len().is_none());
        return if prefix { back } else { 0 };
    }
    0
}

/// 解析 TARGETS 的回复（ATOM 列表，按本机字节序）
pub fn parse_atoms(bytes: &[u8], format: u8) -> Vec<Atom> {
    if format != 32 {
        return Vec::new();
    }
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const INCR: Atom = 300;
    const UTF8: Atom = 301;

    fn incr(limit: usize, mode: LimitMode, chunks: &[Option<&[u8]>]) -> State {
        let mut st = start_incr(0, limit, mode);
        for c in chunks {
            st = incr_chunk(st, c.map(|b| (UTF8, 8, b.to_vec())), limit, mode);
        }
        st
    }

    #[test]
    fn notify_plans() {
        assert_eq!(
            plan_notify(true, UTF8, 10, INCR, 100, LimitMode::Reject),
            NotifyPlan::Refused
        );
        assert_eq!(
            plan_notify(false, ATOM_NONE, 0, INCR, 100, LimitMode::Reject),
            NotifyPlan::Refused
        );
        assert_eq!(
            plan_notify(false, INCR, 4, INCR, 100, LimitMode::Reject),
            NotifyPlan::StartIncr
        );
        assert_eq!(
            plan_notify(false, UTF8, 500, INCR, 100, LimitMode::Reject),
            NotifyPlan::TooLarge(500)
        );
        assert_eq!(
            plan_notify(false, UTF8, 500, INCR, 100, LimitMode::Truncate),
            NotifyPlan::Read {
                want: 100,
                truncated: true
            }
        );
        assert_eq!(
            plan_notify(false, UTF8, 50, INCR, 100, LimitMode::Reject),
            NotifyPlan::Read {
                want: 50,
                truncated: false
            }
        );
    }

    #[test]
    fn incr_accumulates_until_zero_length_chunk() {
        let st = incr(100, LimitMode::Reject, &[Some(b"abc"), Some(b"def"), Some(b"")]);
        assert_eq!(
            st,
            State::Done(Fetched::Data {
                ty: UTF8,
                format: 8,
                bytes: b"abcdef".to_vec(),
                truncated: false
            })
        );
    }

    #[test]
    fn incr_ignores_missing_property_notifications() {
        // 迟到 / 重复的 NewValue：属性不存在时不能当成结束块
        let st = incr(100, LimitMode::Reject, &[Some(b"abc"), None, None, Some(b"def")]);
        assert_eq!(
            st,
            State::Incr {
                ty: UTF8,
                format: 8,
                data: b"abcdef".to_vec()
            }
        );
        assert!(may_still_write(&st, true));
    }

    #[test]
    fn incr_limits() {
        let st = incr(4, LimitMode::Truncate, &[Some(b"abc"), Some(b"def")]);
        assert_eq!(
            st,
            State::Done(Fetched::Data {
                ty: UTF8,
                format: 8,
                bytes: b"abcd".to_vec(),
                truncated: true
            })
        );
        assert!(may_still_write(&st, true), "中途截断：对端还在等这边删除属性");
        let st = incr(4, LimitMode::Reject, &[Some(b"abc"), Some(b"def")]);
        assert_eq!(st, State::Done(Fetched::TooLarge(6)));
        assert_eq!(start_incr(10, 4, LimitMode::Reject), State::Done(Fetched::TooLarge(10)));
        assert!(matches!(start_incr(10, 4, LimitMode::Truncate), State::Incr { .. }));
    }

    #[test]
    fn finished_reads_and_reuse() {
        let st = finish_read(UTF8, 8, b"hello".to_vec(), 4, true);
        assert_eq!(
            st,
            State::Done(Fetched::Data {
                ty: UTF8,
                format: 8,
                bytes: b"hell".to_vec(),
                truncated: true
            })
        );
        assert!(!may_still_write(&st, false), "普通读取的截断已手动删除属性");
        assert!(may_still_write(&State::Done(Fetched::Timeout), false));
        assert!(may_still_write(&State::WaitNotify, false));
        assert!(!may_still_write(&State::Done(Fetched::Refused), false));
        assert!(!may_still_write(&State::Done(Fetched::TooLarge(9)), false));
    }

    #[test]
    fn declared_too_large_incr_taints_the_window() {
        // 拒绝模式下 INCR 声明的大小就超限：INCR 属性已被删除（握手完成），owner 仍会写第一块 → 窗口不可复用
        let plan = plan_notify(false, INCR, 4, INCR, 100, LimitMode::Reject);
        assert!(handshake_started(plan));
        let st = start_incr(10 * 1024 * 1024, 100, LimitMode::Reject);
        assert_eq!(st, State::Done(Fetched::TooLarge(10 * 1024 * 1024)));
        assert!(may_still_write(&st, handshake_started(plan)));
        // 普通属性超限：属性被这边删掉、owner 不会再写 → 窗口可以复用
        let plan = plan_notify(false, UTF8, 500, INCR, 100, LimitMode::Reject);
        assert!(!handshake_started(plan));
        assert!(!may_still_write(
            &State::Done(Fetched::TooLarge(500)),
            handshake_started(plan)
        ));
    }

    #[test]
    fn owner_time_checks() {
        // CurrentTime 总是接受
        assert!(owner_time_ok(Some(5000), 0));
        assert!(owner_time_ok(Some(5000), 5000));
        assert!(owner_time_ok(Some(5000), 5001));
        assert!(
            !owner_time_ok(Some(5000), 4999),
            "早于成为 owner 的请求属于上一任 owner"
        );
        // 服务器运行超过 24.8 天（时间 ≥ 2^31）也照常比较；回绕之后仍视为「之后」
        assert!(owner_time_ok(Some(0x8000_1000), 0x8000_2000));
        assert!(owner_time_ok(Some(0xFFFF_FF00), 0x0000_0100));
        // 成为 owner 的时间未知：不做时间校验（否则带真实时间戳的粘贴请求会全部被拒绝）
        assert!(owner_time_ok(None, 0x8000_2000));
        assert!(owner_time_ok(None, 1));
    }

    #[test]
    fn text_decoding() {
        assert_eq!(decode_text(ATOM_STRING, vec![0x63, 0x61, 0x66, 0xE9]), "café");
        assert_eq!(decode_text(UTF8, "中文".as_bytes().to_vec()), "中文");
        // 末尾被截断的多字节字符丢弃
        let mut cut = "中文".as_bytes().to_vec();
        cut.pop();
        assert_eq!(decode_text(UTF8, cut), "中");
        assert_eq!(decode_text(UTF8, vec![b'a', 0xFF, b'b']), "a\u{FFFD}b");
        // 中间的非法字节替换、末尾的半个字符丢弃
        let mut mixed = vec![b'x', 0xFF];
        mixed.extend_from_slice(&"文".as_bytes()[..2]);
        assert_eq!(decode_text(UTF8, mixed), "x\u{FFFD}");
        // 末尾是孤立的非法字节（不是被截断的字符）：替换而不是丢弃
        assert_eq!(decode_text(UTF8, vec![b'a', 0xC3, b'b']), "a\u{FFFD}b");
        assert_eq!(decode_text(UTF8, vec![b'a', 0x80]), "a\u{FFFD}");
        let atoms: Vec<u8> = [1u32, 2, 3].iter().flat_map(|a| a.to_ne_bytes()).collect();
        assert_eq!(parse_atoms(&atoms, 32), vec![1, 2, 3]);
        assert!(parse_atoms(&atoms, 8).is_empty());
    }
}
