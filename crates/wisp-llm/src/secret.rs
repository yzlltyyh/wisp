//! API Key 包装：Debug / Display 一律输出 "***"，Drop 时清零。

use std::fmt;

/// Key 包装：Debug/Display 一律输出 "***"，Drop 时清零
pub struct Secret(String);

impl Secret {
    /// 包装一个 Key
    pub fn new(s: String) -> Self {
        Self(s)
    }

    /// 取出明文（只在拼请求头时调用，禁止写日志）
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// 生成 `prefix + Key` 的请求头值（Key 去掉首尾空白，标记为 sensitive）；临时缓冲用完即清零。
    /// Key 含有不能出现在请求头里的字符时返回 None
    pub(crate) fn header_value(&self, prefix: &str) -> Option<reqwest::header::HeaderValue> {
        let key = self.0.trim();
        let mut buf = Vec::with_capacity(prefix.len() + key.len());
        buf.extend_from_slice(prefix.as_bytes());
        buf.extend_from_slice(key.as_bytes());
        let value = reqwest::header::HeaderValue::from_bytes(&buf).ok().map(|mut v| {
            v.set_sensitive(true);
            v
        });
        buf.fill(0);
        std::hint::black_box(&buf);
        value
    }

    /// 复制一份（测试连接要用同一个 Key 构造探针请求）
    pub(crate) fn duplicate(&self) -> Self {
        Self(self.0.clone())
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.0).into_bytes();
        bytes.fill(0);
        // 防止编译器把清零当作无用写入优化掉
        std::hint::black_box(&bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::Secret;

    #[test]
    fn never_prints_plaintext() {
        let s = Secret::new("sk-test".to_owned());
        assert_eq!(format!("{s:?} {s}"), "*** ***");
        assert_eq!(s.expose(), "sk-test");
        assert_eq!(format!("{:?}", s.duplicate()), "***");
    }

    #[test]
    fn header_values_are_sensitive_and_trimmed() {
        let s = Secret::new(" sk-test\n".to_owned());
        let v = s.header_value("Bearer ").expect("valid header");
        assert!(v.is_sensitive());
        assert_eq!(v.to_str().ok(), Some("Bearer sk-test"));
        assert!(Secret::new("a\u{7}b".to_owned()).header_value("").is_none());
    }
}
