//! 收藏的名称：规则与清理。名称存在 `meta_json.$.name`，只有收藏的条目才有（命名即收藏，取消收藏时一并去掉）。

/// 名称最多多少个字符（Unicode 标量值）
pub const MAX_NAME_CHARS: usize = 64;

/// 名称不合规的原因（不含名称本身：名称是用户内容，不进日志与错误信息）
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("favorite name longer than {MAX_NAME_CHARS} characters")]
    TooLong,
    /// 控制字符、行 / 段分隔符与双向文本控制符：名称显示在单行里，这些字符会打乱排版或伪装成别的文字
    #[error("favorite name contains control or bidi characters")]
    ForbiddenChar,
}

/// 清理用户输入的名称：去掉首尾空白；空串表示清除（`Ok(None)`）。IPC 在命令体第一步调用它校验参数，
/// `ClipStore::set_name` 落库前也会再调用一次
pub fn clean_name(raw: &str) -> Result<Option<String>, NameError> {
    let name = raw.trim();
    if name.is_empty() {
        return Ok(None);
    }
    if name.chars().any(forbidden) {
        return Err(NameError::ForbiddenChar);
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(NameError::TooLong);
    }
    Ok(Some(name.to_owned()))
}

fn forbidden(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{2028}' | '\u{2029}' | '\u{200E}' | '\u{200F}' | '\u{061C}')
        || ('\u{202A}'..='\u{202E}').contains(&c)
        || ('\u{2066}'..='\u{2069}').contains(&c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_trimmed_and_validated() {
        assert_eq!(
            clean_name("  Staging server \u{3000}"),
            Ok(Some("Staging server".to_owned()))
        );
        assert_eq!(clean_name(" \t "), Ok(None));
        assert_eq!(clean_name("服务器"), Ok(Some("服务器".to_owned())));
        assert_eq!(
            clean_name(&"名".repeat(MAX_NAME_CHARS)).map(|n| n.map(|n| n.chars().count())),
            Ok(Some(64))
        );
        assert_eq!(clean_name(&"名".repeat(MAX_NAME_CHARS + 1)), Err(NameError::TooLong));
        for bad in [
            "a\nb",
            "a\u{0}b",
            "a\u{2028}b",
            "\u{202E}gnp.exe",
            "a\u{2066}b",
            "tab\there",
        ] {
            assert_eq!(clean_name(bad), Err(NameError::ForbiddenChar), "{bad:?}");
        }
        // 首尾的换行只是空白，去掉即可
        assert_eq!(clean_name("\nname\n"), Ok(Some("name".to_owned())));
        // 错误信息不含名称
        assert!(!NameError::TooLong.to_string().contains('名'));
    }
}
