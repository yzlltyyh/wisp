//! 流程状态机：翻译、输入、OCR、直接翻译四条流程（外加剪贴板面板的打开，只用来收尾埋点）的
//! 「状态 × 事件」转移表。
//!
//! 每次由 Rust 发起的流程（快捷键、托盘、浮标、`--trigger`、引导页「试一试」、剪贴板条目送翻译）对应一个 **run**：
//! 从触发开始，到它的面板会话结束为止。编排器在每个关键步骤调用 [`step`]（经 `Core::advance`），由返回值决定下一步：
//! - [`Transition::Next`]：合法转移，继续；
//! - [`Transition::Ignore`]：run 已经结束（被新的触发取代、被取消、面板已关），迟到的结果直接丢弃——这就是
//!   「取词进行中再次触发时以最新一次为准」与「迟到的旧事件按 id 丢弃」的落点；
//! - [`Transition::Illegal`]：代码路径与状态机不一致（实现错误）。debug 构建断言失败，发布构建记一条 error 并按 Ignore 处理。
//!
//! **「面板出现之前」以 `panel_prepare` 是否已把会话交给 run 为准，不看阶段**：NotReady / Confirming / Editing /
//! Browsing / Requesting（乃至缓存命中后的 Done）都是在 `panel_prepare` 返回之前进入的。新的触发到来时，编排器对
//! 所有还没拿到会话的 run 发 [`Event::Retrigger`]（一律作废），已经拿到会话的 run 不受影响（新会话替换面板时经
//! [`Event::Closed`] 收尾）。`panel_prepare` 本身在编排器的准备闸门里串行执行，并在同一个临界区里把会话记到 run 上，
//! 因此两个 run 不会竞争面板：被作废的 run 要么根本不 prepare，要么在新 run 之前 prepare（随即被新会话替换）。
//!
//! **输入流程**（专用快捷键）与**取词失败改开的输入视图**：Triggered →（Opened / SelectionMissed）→ Editing；用户提交
//! （前端 `translate_start`，该会话的第一个请求）时 Editing →（Requested）→ Requesting → Streaming → Done，此后与翻译流程
//! 相同。长文本确认同理：Confirming → 用户点「继续」（前端 `translate_start`）→ Requesting。输入视图提交的是长文本
//! 时（`TranslateReq.confirm_long = false` 且超过阈值）：Editing →（LongText）→
//! Confirming，「继续」时以 `confirm_long = true` 重发 → Requesting。
//!
//! 同一面板里其后由前端发起的请求（换风格、重试、润色、重新识别……）不再推进 run，它们在 `StreamRegistry` 里按
//! 「宿主 × 通道」管理（同一通道新请求取消旧请求）。

/// 流程种类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flow {
    /// 划词翻译 / 指定风格翻译 / 送来的文本直接翻译
    Translate,
    /// 输入模式（专用快捷键）
    Input,
    /// 截图 OCR
    Ocr,
    /// 截图直接翻译
    DirectTranslate,
    /// 剪贴板面板（流程本身由 clipboard_svc 建模，这里只跟踪面板会话以便收尾埋点）
    Clipboard,
}

/// 流程所处的阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    /// 刚触发，还没开始取词 / 截图
    Triggered,
    /// 工作线程上取词
    Selecting,
    /// 已拿到原文：未就绪检查、语言与方向、词典式判定、长文本判定、缓存
    Checking,
    /// 截图前隐藏窗口 → `capture_all` → 建 Overlay
    Capturing,
    /// Overlay 显示中，等用户框选
    Framing,
    /// 面板显示未就绪卡片（没有请求）
    NotReady,
    /// 长文本等用户确认（没有请求；确认后由前端发起）
    Confirming,
    /// 输入视图等用户提交（提交后由前端发起）
    Editing,
    /// 剪贴板面板
    Browsing,
    /// 请求已发出，还没有内容
    Requesting,
    /// 流式输出中
    Streaming,
    /// 请求正常结束（面板仍在）
    Done,
    /// 请求失败结束（面板仍在显示错误卡片），或面板没能显示
    Failed,
    /// 被取消（用户停止 / 关闭 / Overlay 取消 / 流式中被替换）
    Cancelled,
    /// 面板出现之前就被新的触发取代：结果丢弃
    Superseded,
    /// 面板会话已结束
    Closed,
}

impl Stage {
    /// 终止阶段：之后的事件一律忽略（Done / Failed 还要等面板关闭，不算终止）
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Cancelled | Self::Superseded | Self::Closed)
    }

    /// 还在做准备、没有可显示结果的阶段（取消、失败一律直接结束）
    pub const fn is_pre_panel(self) -> bool {
        matches!(
            self,
            Self::Triggered | Self::Selecting | Self::Checking | Self::Capturing | Self::Framing
        )
    }

    /// 是否有进行中的请求
    pub const fn is_requesting(self) -> bool {
        matches!(self, Self::Requesting | Self::Streaming)
    }
}

/// 驱动状态机的事件
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Event {
    /// 开始取词（翻译）/ 开始截图（OCR）
    Begin,
    /// 拿到非空原文（取词成功，或触发本身带着文本）
    Selected,
    /// 取词为空 / 失败：改以输入视图打开
    SelectionMissed,
    /// 未就绪（没有 Provider / 默认模型 / 视觉模型、钥匙串不可用、缺权限、截图失败）
    NotReady,
    /// 超过长文本阈值，等用户确认
    LongText,
    /// 请求已发出（含缓存命中，缓存命中随即 Finished）
    Requested,
    /// 无请求的视图（输入 / 剪贴板）已打开
    Opened,
    /// 截图完成、Overlay 已建好
    Captured,
    /// 截图失败（有卡片）
    CaptureFailed,
    /// 截图被用户取消（门户），静默结束
    CaptureCancelled,
    /// Overlay 提交了选区
    Committed,
    /// 收到第一个 Delta
    FirstDelta,
    /// 请求正常结束
    Finished,
    /// 请求失败，或面板没能显示
    Failed,
    /// 取消（用户停止、Overlay Esc、应用退出）
    Cancel,
    /// 来了新的触发，而这个 run 的面板还没出现（`panel_prepare` 还没把会话交给它）：不论处于哪个阶段，一律作废
    /// （Overlay 显示中算取消）。已经拿到会话的 run 不会收到这个事件（新会话替换面板时，旧会话结束，经 Closed 收尾）
    Retrigger,
    /// 面板会话已结束（`panel_hidden` + `panel_session_live` 核对为 false）：被新会话替换、用户关闭、截图前隐藏、
    /// 页面重载……流式中再次触发（重入）也走这里：旧会话被替换的那一刻结束，进行中的请求取消
    Closed,
}

/// 一次转移的结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    Next(Stage),
    Ignore,
    Illegal,
}

/// 各流程的初始阶段
pub const fn initial(_flow: Flow) -> Stage {
    Stage::Triggered
}

/// 转移表。规则：
/// - 终止阶段吸收一切事件（Ignore）；
/// - `Retrigger`（只发给还没拿到面板会话的 run）：Framing → Cancelled（拆掉 Overlay），其余 → Superseded；
/// - 准备阶段：`Cancel` → Cancelled，`Failed` → Failed；
/// - `Closed` 让进行中的请求 → Cancelled，其余 → Closed；
/// - 其余组合都是非法转移。
pub fn step(flow: Flow, stage: Stage, event: Event) -> Transition {
    use Event as E;
    use Stage as S;
    use Transition::{Ignore, Illegal, Next};

    if stage.is_terminal() {
        return Ignore;
    }
    // 各阶段共有的转移
    match (stage, event) {
        (s, E::Retrigger) => {
            // Overlay 显示中被新的触发取代：Overlay 拆掉，算作取消
            return Next(if s == S::Framing { S::Cancelled } else { S::Superseded });
        }
        (s, E::Cancel) if s.is_pre_panel() || s.is_requesting() => return Next(S::Cancelled),
        (S::Done | S::Failed | S::NotReady | S::Confirming | S::Editing | S::Browsing, E::Cancel) => return Ignore,
        (s, E::Closed) if s.is_requesting() => return Next(S::Cancelled),
        (_, E::Closed) => return Next(S::Closed),
        (s, E::Failed) if s.is_pre_panel() || s.is_requesting() => return Next(S::Failed),
        _ => {}
    }

    match flow {
        Flow::Translate => match (stage, event) {
            (S::Triggered, E::Begin) => Next(S::Selecting),
            (S::Triggered | S::Selecting, E::Selected) => Next(S::Checking),
            (S::Triggered | S::Selecting, E::SelectionMissed) => Next(S::Editing),
            (S::Triggered | S::Checking, E::NotReady) => Next(S::NotReady),
            // 长文本：Rust 发起时在规划之后；取词失败改开的输入视图提交了长文本时在 Editing（
            // `TranslateReq.confirm_long`）
            (S::Checking | S::Editing, E::LongText) => Next(S::Confirming),
            // 前端提交：取词失败改开的输入视图提交、长文本确认后「继续」
            (S::Checking | S::Editing | S::Confirming, E::Requested) => Next(S::Requesting),
            _ => requesting(stage, event),
        },
        Flow::Input => match (stage, event) {
            (S::Triggered, E::Opened) => Next(S::Editing),
            // 提交的是长文本：先确认（StartAck.long_text），「继续」时再提交
            (S::Editing, E::LongText) => Next(S::Confirming),
            // 用户提交（前端 translate_start，该会话的第一个请求）；长文本确认后「继续」
            (S::Editing | S::Confirming, E::Requested) => Next(S::Requesting),
            _ => requesting(stage, event),
        },
        Flow::Clipboard => match (stage, event) {
            (S::Triggered, E::Opened) => Next(S::Browsing),
            _ => Illegal,
        },
        Flow::Ocr | Flow::DirectTranslate => match (stage, event) {
            (S::Triggered, E::Begin) => Next(S::Capturing),
            (S::Triggered | S::Framing, E::NotReady) | (S::Capturing, E::CaptureFailed) => Next(S::NotReady),
            (S::Capturing, E::CaptureCancelled) => Next(S::Cancelled),
            (S::Capturing, E::Captured) => Next(S::Framing),
            (S::Framing, E::Committed) => Next(S::Requesting),
            _ => requesting(stage, event),
        },
    }
}

/// 请求阶段的公共转移（翻译与 OCR 相同）
fn requesting(stage: Stage, event: Event) -> Transition {
    use Event as E;
    use Stage as S;
    match (stage, event) {
        (S::Requesting, E::FirstDelta) => Transition::Next(S::Streaming),
        // 流式中重复的 FirstDelta（同一阶段只记第一次）
        (S::Streaming, E::FirstDelta) => Transition::Ignore,
        (S::Requesting | S::Streaming, E::Finished) => Transition::Next(S::Done),
        _ => Transition::Illegal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOWS: [Flow; 5] = [
        Flow::Translate,
        Flow::Input,
        Flow::Ocr,
        Flow::DirectTranslate,
        Flow::Clipboard,
    ];
    const STAGES: [Stage; 16] = [
        Stage::Triggered,
        Stage::Selecting,
        Stage::Checking,
        Stage::Capturing,
        Stage::Framing,
        Stage::NotReady,
        Stage::Confirming,
        Stage::Editing,
        Stage::Browsing,
        Stage::Requesting,
        Stage::Streaming,
        Stage::Done,
        Stage::Failed,
        Stage::Cancelled,
        Stage::Superseded,
        Stage::Closed,
    ];
    const EVENTS: [Event; 17] = [
        Event::Begin,
        Event::Selected,
        Event::SelectionMissed,
        Event::NotReady,
        Event::LongText,
        Event::Requested,
        Event::Opened,
        Event::Captured,
        Event::CaptureFailed,
        Event::CaptureCancelled,
        Event::Committed,
        Event::FirstDelta,
        Event::Finished,
        Event::Failed,
        Event::Cancel,
        Event::Retrigger,
        Event::Closed,
    ];

    /// 沿着一串事件走，返回最终阶段（遇到非 Next 时 panic，便于定位）
    fn walk(flow: Flow, events: &[Event]) -> Stage {
        events.iter().fold(initial(flow), |s, &e| match step(flow, s, e) {
            Transition::Next(n) => n,
            other => panic!("{flow:?}: {s:?} --{e:?}--> {other:?}"),
        })
    }

    #[test]
    fn happy_paths_of_every_flow() {
        use Event as E;
        assert_eq!(
            walk(
                Flow::Translate,
                &[
                    E::Begin,
                    E::Selected,
                    E::Requested,
                    E::FirstDelta,
                    E::Finished,
                    E::Closed
                ]
            ),
            Stage::Closed
        );
        // 送来的文本：跳过取词
        assert_eq!(
            walk(Flow::Translate, &[E::Selected, E::Requested, E::Finished]),
            Stage::Done
        );
        assert_eq!(
            walk(Flow::Translate, &[E::Begin, E::SelectionMissed, E::Closed]),
            Stage::Closed
        );
        assert_eq!(
            walk(Flow::Translate, &[E::Begin, E::Selected, E::LongText, E::Closed]),
            Stage::Closed
        );
        assert_eq!(walk(Flow::Input, &[E::Opened, E::Closed]), Stage::Closed);
        // 输入提交：Editing → Requesting → Streaming → Done（前端发起的第一个请求推进 run）
        assert_eq!(
            walk(
                Flow::Input,
                &[E::Opened, E::Requested, E::FirstDelta, E::Finished, E::Closed]
            ),
            Stage::Closed
        );
        // 取词失败改开的输入视图提交、长文本确认后继续
        assert_eq!(
            walk(
                Flow::Translate,
                &[E::Begin, E::SelectionMissed, E::Requested, E::Finished]
            ),
            Stage::Done
        );
        assert_eq!(
            walk(
                Flow::Translate,
                &[E::Selected, E::LongText, E::Requested, E::FirstDelta]
            ),
            Stage::Streaming
        );
        // 输入视图提交了长文本：先确认，「继续」后才请求
        assert_eq!(
            walk(
                Flow::Input,
                &[E::Opened, E::LongText, E::Requested, E::FirstDelta, E::Finished]
            ),
            Stage::Done
        );
        assert_eq!(
            walk(
                Flow::Translate,
                &[E::Begin, E::SelectionMissed, E::LongText, E::Requested]
            ),
            Stage::Requesting
        );
        assert_eq!(walk(Flow::Clipboard, &[E::Opened, E::Closed]), Stage::Closed);
        for flow in [Flow::Ocr, Flow::DirectTranslate] {
            assert_eq!(
                walk(
                    flow,
                    &[
                        E::Begin,
                        E::Captured,
                        E::Committed,
                        E::FirstDelta,
                        E::Finished,
                        E::Closed
                    ]
                ),
                Stage::Closed
            );
            assert_eq!(walk(flow, &[E::Begin, E::CaptureFailed]), Stage::NotReady);
            assert_eq!(walk(flow, &[E::Begin, E::CaptureCancelled]), Stage::Cancelled);
            assert_eq!(walk(flow, &[E::Begin, E::Captured, E::Cancel]), Stage::Cancelled);
            assert_eq!(walk(flow, &[E::NotReady, E::Closed]), Stage::Closed);
        }
    }

    #[test]
    fn cancel_reentry_and_failure_have_explicit_transitions() {
        for flow in FLOWS {
            for stage in STAGES {
                // 重入（只发给还没拿到面板会话的 run）：任何未终止的阶段都作废（Overlay 显示中算取消）
                let re = step(flow, stage, Event::Retrigger);
                if stage.is_terminal() {
                    assert_eq!(re, Transition::Ignore, "{flow:?} {stage:?}");
                } else if stage == Stage::Framing {
                    assert_eq!(re, Transition::Next(Stage::Cancelled));
                } else {
                    assert_eq!(re, Transition::Next(Stage::Superseded), "{flow:?} {stage:?}");
                }
                // 取消：进行中的一律 Cancelled；已经只剩面板的忽略
                let cancel = step(flow, stage, Event::Cancel);
                if stage.is_pre_panel() || stage.is_requesting() {
                    assert_eq!(cancel, Transition::Next(Stage::Cancelled), "{flow:?} {stage:?}");
                } else {
                    assert_eq!(cancel, Transition::Ignore, "{flow:?} {stage:?}");
                }
                // 流式中被替换 / 面板关闭：请求取消；其余收尾为 Closed
                let closed = step(flow, stage, Event::Closed);
                if stage.is_terminal() {
                    assert_eq!(closed, Transition::Ignore);
                } else if stage.is_requesting() {
                    assert_eq!(closed, Transition::Next(Stage::Cancelled), "{flow:?} {stage:?}");
                } else {
                    assert_eq!(closed, Transition::Next(Stage::Closed), "{flow:?} {stage:?}");
                }
                // 失败
                let failed = step(flow, stage, Event::Failed);
                if stage.is_pre_panel() || stage.is_requesting() {
                    assert_eq!(failed, Transition::Next(Stage::Failed), "{flow:?} {stage:?}");
                }
            }
        }
    }

    #[test]
    fn terminal_stages_absorb_everything() {
        for flow in FLOWS {
            for stage in STAGES.iter().copied().filter(|s| s.is_terminal()) {
                for event in EVENTS {
                    assert_eq!(
                        step(flow, stage, event),
                        Transition::Ignore,
                        "{flow:?} {stage:?} {event:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn transition_table_snapshot() {
        // 「状态 × 事件」全表：只统计 Next 的条数与几个关键的非法组合，防止表被无意改动
        let mut next = 0;
        for flow in FLOWS {
            for stage in STAGES {
                for event in EVENTS {
                    if matches!(step(flow, stage, event), Transition::Next(_)) {
                        next += 1;
                    }
                }
            }
        }
        // 每个流程共有 40 条（重入 13、取消 7、关闭 13、失败 7）× 5 + 各流程专有 40 条（翻译 14、输入 5、剪贴板 1、
        // OCR 10、直接翻译 10）
        assert_eq!(next, 243, "转移表的合法转移条数变了：确认是有意的修改后更新这里");
        // 非法：输入流程不取词、不截图；翻译流程不提交选区；请求结束后不会再有 FirstDelta；剪贴板面板不发请求
        assert_eq!(step(Flow::Input, Stage::Triggered, Event::Begin), Transition::Illegal);
        assert_eq!(
            step(Flow::Clipboard, Stage::Browsing, Event::Requested),
            Transition::Illegal
        );
        assert_eq!(
            step(Flow::Translate, Stage::Framing, Event::Committed),
            Transition::Illegal
        );
        assert_eq!(step(Flow::Ocr, Stage::Selecting, Event::Selected), Transition::Illegal);
        assert_eq!(
            step(Flow::Translate, Stage::Done, Event::FirstDelta),
            Transition::Illegal
        );
        assert_eq!(step(Flow::Ocr, Stage::Done, Event::Finished), Transition::Illegal);
    }
}
