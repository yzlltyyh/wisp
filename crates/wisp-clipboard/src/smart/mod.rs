//! 智能识别与文本变换：剪贴板条目、识别结果等纯文本的「片段」（芯片条）与「粘贴为…」都从这里来。
//!
//! - [`recognize`]：识别器注册表，按设置启用的种类找出片段（不落库，按需计算）；
//! - [`transform`]：变换注册表，判断哪些变换适用并执行；
//! - [`otp`]：验证码检测，记录时据此给条目定过期时间，芯片条也用同一个检测器。
//!
//! 放在 wisp-clipboard 而不是单独的 crate：记录管线要在写入前识别验证码，而 crate 之间只允许横向依赖 wisp-types。
//! 这里的输入输出都是用户内容，任何结果都不写日志（最多记数量）。

pub mod otp;
pub mod recognize;
pub mod transform;

use wisp_types::{ClipInspect, ClipKind};

pub use recognize::{MAX_ENTITIES, RecognizeOpts, Recognizer, SCAN_BYTES, Span, recognize};
pub use transform::{Transform, TransformError, applicable, apply};

/// 一段内容的检视：片段（芯片条）与适用的变换（「粘贴为…」）。图片与文件没有正文，结果为空
pub fn inspect(kind: ClipKind, text: &str, opts: &RecognizeOpts) -> ClipInspect {
    if matches!(kind, ClipKind::Image | ClipKind::Files) {
        return ClipInspect::default();
    }
    ClipInspect {
        entities: recognize(text, opts),
        transforms: applicable(kind, text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wisp_types::{EntityKind, TransformId};

    #[test]
    fn inspect_combines_entities_and_transforms() {
        let text = "Your verification code is 482915\nDo not share it.";
        let r = inspect(ClipKind::Text, text, &RecognizeOpts::default());
        assert_eq!(r.entities.iter().map(|e| e.kind).collect::<Vec<_>>(), [EntityKind::Otp]);
        assert_eq!(r.entities[0].value, "482915");
        assert!(r.transforms.contains(&TransformId::JoinLines));
        assert_eq!(
            inspect(ClipKind::Image, text, &RecognizeOpts::default()),
            ClipInspect::default()
        );
        assert_eq!(
            inspect(ClipKind::Files, text, &RecognizeOpts::default()),
            ClipInspect::default()
        );
    }
}
