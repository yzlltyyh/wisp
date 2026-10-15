//! 日志：tauri-plugin-log 以 Debug 级别构建；bootstrap 读完设置后按 `advanced.log_level`
//! 调用 `log::set_max_level`，改设置时同样即时生效（`app::reactor`）。
//!
//! - 位置：`log_dir/wisp/wisp.log`，2 MB × 5 滚动（`RotationStrategy::KeepSome(5)`）；debug 构建额外输出到 Stdout；
//! - 每行格式：`2026-10-10T07:31:02.123Z INFO  wisp_desktop_lib::tray: 事件 key=value …`（UTC，毫秒精度）；
//! - **禁止**写入日志：原文、译文、剪贴板内容、OCR 图像、Key、请求头的值、URL 的 query。各模块自己负责不写这些内容；
//!   这里的 [`redact`] 是最后一道保险：每行写出前把形似 API Key 的片段（`sk-…`、`AIza…`、`Bearer …`、`Basic …` 等）、URL 里的
//!   userinfo（`https://user:token@host`）与 query 换成 `***`。没有命中时零额外分配（只扫描一遍）。
//! - 日志不做定时 flush（空闲时不留周期唤醒）。

use std::borrow::Cow;

use tauri_plugin_log::{RotationStrategy, Target, TargetKind};
use wisp_types::LogLevel;

/// 单个日志文件的上限（字节）
pub const MAX_FILE_BYTES: u128 = 2_000_000;
/// 保留的滚动文件数
pub const KEEP_FILES: usize = 5;
/// 日志文件名（不含扩展名）
pub const FILE_STEM: &str = "wisp";

/// 日志插件：`log_dir/wisp/wisp.log`，2 MB × 5 滚动；debug 构建额外输出到 Stdout
pub fn plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    let mut targets = vec![Target::new(TargetKind::Folder {
        path: super::paths::log_dir(),
        file_name: Some(FILE_STEM.to_owned()),
    })];
    if cfg!(debug_assertions) {
        targets.push(Target::new(TargetKind::Stdout));
    }
    tauri_plugin_log::Builder::new()
        .clear_targets()
        .targets(targets)
        .level(log::LevelFilter::Debug)
        // 第三方库的 debug 日志量大且可能带 URL：只保留 info 及以上
        .level_for("tao", log::LevelFilter::Info)
        .level_for("wry", log::LevelFilter::Info)
        .level_for("reqwest", log::LevelFilter::Info)
        .level_for("hyper", log::LevelFilter::Info)
        .level_for("hyper_util", log::LevelFilter::Info)
        .level_for("rustls", log::LevelFilter::Info)
        .level_for("notify", log::LevelFilter::Info)
        .level_for("tracing", log::LevelFilter::Warn)
        // 更新插件把「端点没有成功响应」（离线、还没有任何发布）记成 ERROR，同一次失败 Wisp 的 updater 模块还会再记一行：
        // 插件自己的日志关掉，检查的每种结局由 updater 模块记且只记一行（自动检查的预期内失败是 info，手动检查失败是 warn）
        .level_for("tauri_plugin_updater", log::LevelFilter::Off)
        .max_file_size(MAX_FILE_BYTES)
        .rotation_strategy(RotationStrategy::KeepSome(KEEP_FILES))
        .format(|out, message, record| {
            let ts = utc_timestamp(crate::settings::fsio::unix_now_ms());
            let level = record.level();
            let target = record.target();
            match message.as_str() {
                Some(s) => out.finish(format_args!("{ts} {level:<5} {target}: {}", redact(s))),
                None => {
                    let s = message.to_string();
                    out.finish(format_args!("{ts} {level:<5} {target}: {}", redact(&s)));
                }
            }
        })
        .build()
}

/// 按设置调整全局日志级别（改设置时同样即时生效）
pub fn apply_level(level: LogLevel) {
    log::set_max_level(level_filter(level));
}

/// 设置里的级别 → `log` 的过滤级别
pub const fn level_filter(level: LogLevel) -> log::LevelFilter {
    match level {
        LogLevel::Error => log::LevelFilter::Error,
        LogLevel::Warn => log::LevelFilter::Warn,
        LogLevel::Info => log::LevelFilter::Info,
        LogLevel::Debug => log::LevelFilter::Debug,
    }
}

/// Unix 毫秒 → `yyyy-mm-ddThh:mm:ss.mmmZ`（UTC）
pub fn utc_timestamp(unix_ms: i64) -> String {
    let date = crate::settings::state_store::utc_date(unix_ms);
    let ms_of_day = unix_ms.rem_euclid(86_400_000);
    let (h, rest) = (ms_of_day / 3_600_000, ms_of_day % 3_600_000);
    let (m, rest) = (rest / 60_000, rest % 60_000);
    let (s, ms) = (rest / 1000, rest % 1000);
    format!("{date}T{h:02}:{m:02}:{s:02}.{ms:03}Z")
}

/// 形似 API Key 的前缀（命中且后面跟着足够长的 token 时整段换成 `***`）
const KEY_PREFIXES: &[&str] = &[
    "sk-", "sk_", "gsk_", "xai-", "AIza", "pplx-", "nvapi-", "hf_", "r8_", "fw_",
];
/// 前缀之后至少多少个 token 字符才算 Key（避免误伤 `sk-` 这类普通词）
const MIN_KEY_TAIL: usize = 12;
/// `Bearer` 之后至少多少个字符才算凭据
const MIN_BEARER_LEN: usize = 8;
/// `Basic` 之后至少多少个字符才算凭据（WebDAV 的 `账号:密码` 经 base64 编码，最短的实际凭据也远超这个长度）
const MIN_BASIC_LEN: usize = 8;

/// 单词字符：前一个字符是单词字符时不算「词首」（`desk-lamp` 里的 `sk-` 不是 Key）
fn is_word_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_')
}

/// Key / 凭据 token 允许的字符（含 base64 与 JWT 的 `.` `/` `+` `=`）
fn is_token_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~' | b'+' | b'/' | b'=')
}

/// URL 中止于空白、引号、尖括号、右括号
fn is_url_end(c: u8) -> bool {
    c.is_ascii_whitespace() || matches!(c, b'"' | b'\'' | b'<' | b'>' | b')' | b']' | b'`')
}

/// 一处需要替换的区间：`[start, end)` 换成 `replacement`
struct Span {
    start: usize,
    end: usize,
    replacement: &'static str,
}

/// 脱敏：把形似 API Key 的片段、`Bearer` / `Basic` 凭据、URL 的 userinfo 与 query 换成 `***`。没有命中时原样借用
pub fn redact(input: &str) -> Cow<'_, str> {
    let bytes = input.as_bytes();
    let mut spans: Vec<Span> = Vec::new();
    // 当前 URL 的 query 区间（扫描到它时整段替换）；URL 的路径部分仍按 Key 规则扫描
    let mut query: Option<(usize, usize)> = None;
    let mut i = 0;
    while i < bytes.len() {
        if let Some((start, end)) = query
            && i >= start
        {
            spans.push(Span {
                start,
                end,
                replacement: "***",
            });
            query = None;
            i = end;
            continue;
        }
        let at_boundary = i == 0 || !is_word_char(bytes[i - 1]);
        // URL：scheme://… 中的 userinfo（`user:token@`，一些自建网关把凭据写在这里）与 query（? 之后到 URL 结束）
        if query.is_none() && bytes[i..].starts_with(b"://") {
            let url_end = bytes[i..]
                .iter()
                .position(|c| is_url_end(*c))
                .map_or(bytes.len(), |p| i + p);
            let host_start = i + 3;
            let authority_end = bytes[host_start..url_end]
                .iter()
                .position(|c| matches!(c, b'/' | b'?' | b'#'))
                .map_or(url_end, |p| host_start + p);
            let mut resume = host_start;
            // 以最后一个 @ 为界（密码里出现未编码的 @ 时也整段遮住）
            if let Some(at) = bytes[host_start..authority_end].iter().rposition(|c| *c == b'@')
                && at > 0
            {
                spans.push(Span {
                    start: host_start,
                    end: host_start + at,
                    replacement: "***",
                });
                resume = host_start + at;
            }
            if let Some(q) = bytes[resume..url_end].iter().position(|c| *c == b'?') {
                let q = resume + q;
                if q + 1 < url_end {
                    query = Some((q + 1, url_end));
                }
            }
            i = resume;
            continue;
        }
        if at_boundary {
            // Bearer <token>
            if starts_with_ignore_case(&bytes[i..], b"bearer ") {
                let start = i + "bearer ".len();
                let end = token_end(bytes, start);
                if end - start >= MIN_BEARER_LEN {
                    spans.push(Span {
                        start,
                        end,
                        replacement: "***",
                    });
                    i = end;
                    continue;
                }
            }
            // Basic <base64(账号:密码)>：同步用的 WebDAV 认证头。要求形似 base64（只含 base64 字符、长度是 4 的倍数、
            // 不全是小写字母），避免「basic settings」这类普通词组被误遮
            if starts_with_ignore_case(&bytes[i..], b"basic ") {
                let start = i + "basic ".len();
                let end = token_end(bytes, start);
                if looks_like_basic_credential(&bytes[start..end]) {
                    spans.push(Span {
                        start,
                        end,
                        replacement: "***",
                    });
                    i = end;
                    continue;
                }
            }
            // sk-… 等前缀
            if let Some(prefix) = KEY_PREFIXES.iter().find(|p| bytes[i..].starts_with(p.as_bytes())) {
                let end = token_end(bytes, i);
                if end - i >= prefix.len() + MIN_KEY_TAIL {
                    spans.push(Span {
                        start: i,
                        end,
                        replacement: "***",
                    });
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }
    if spans.is_empty() {
        return Cow::Borrowed(input);
    }
    let mut out = String::with_capacity(input.len());
    let mut last = 0;
    for span in spans {
        out.push_str(&input[last..span.start]);
        out.push_str(span.replacement);
        last = span.end;
    }
    out.push_str(&input[last..]);
    Cow::Owned(out)
}

fn token_end(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .iter()
        .position(|c| !is_token_char(*c))
        .map_or(bytes.len(), |p| start + p)
}

fn looks_like_basic_credential(token: &[u8]) -> bool {
    token.len() >= MIN_BASIC_LEN
        && token.len().is_multiple_of(4)
        && token
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'='))
        && !token.iter().all(u8::is_ascii_lowercase)
}

fn starts_with_ignore_case(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() >= needle.len() && hay[..needle.len()].eq_ignore_ascii_case(needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_lines_are_borrowed() {
        let line = "req provider=openai model=gpt-4o host=api.openai.com path=/v1/chat/completions status=200";
        assert!(matches!(redact(line), Cow::Borrowed(_)));
        assert!(matches!(redact("tray ready elapsed_ms=123"), Cow::Borrowed(_)));
        // 普通单词里的 sk- / 短 token 不误伤
        assert!(matches!(redact("task-runner sk-short desk-lamp"), Cow::Borrowed(_)));
    }

    #[test]
    fn api_keys_are_masked() {
        assert_eq!(
            redact("auth failed key=sk-proj-AbCdEfGhIjKlMnOp123 for provider"),
            "auth failed key=*** for provider"
        );
        assert_eq!(redact("google AIzaSyA1234567890abcdefghijk"), "google ***");
        assert_eq!(redact("(sk-ant-api03-abcdefghijklmnop)"), "(***)");
    }

    #[test]
    fn bearer_tokens_are_masked() {
        assert_eq!(
            redact("header Authorization: Bearer abcdefghijklmnop.qrs"),
            "header Authorization: Bearer ***"
        );
        assert_eq!(redact("bearer short"), "bearer short");
    }

    #[test]
    fn basic_credentials_are_masked() {
        // 「someone:not-a-real-password」的 base64
        assert_eq!(
            redact("Authorization: Basic c29tZW9uZTpub3QtYS1yZWFsLXBhc3N3b3Jk"),
            "Authorization: Basic ***"
        );
        assert_eq!(
            redact("authorization: basic dTpwYXNzd29yZA== sent"),
            "authorization: basic *** sent"
        );
        // 普通词组不误伤
        for clean in ["using basic settings", "Basic auth failed", "basic short"] {
            assert!(matches!(redact(clean), Cow::Borrowed(_)), "{clean}");
        }
    }

    #[test]
    fn url_queries_are_masked() {
        assert_eq!(
            redact("GET https://example.com/v1/models?key=AIzaXYZ&alt=sse failed"),
            "GET https://example.com/v1/models?*** failed"
        );
        assert_eq!(redact("see \"http://h/p?a=1\""), "see \"http://h/p?***\"");
        assert!(matches!(redact("https://example.com/path?"), Cow::Borrowed(_)));
    }

    #[test]
    fn url_credentials_are_masked() {
        assert_eq!(
            redact("proxy https://user:t0ken@gw.example.com/v1?x=1 failed"),
            "proxy https://***@gw.example.com/v1?*** failed"
        );
        assert_eq!(redact("http://alice@h:8080/p"), "http://***@h:8080/p");
        // 密码里有未编码的 @：以最后一个为界
        assert_eq!(redact("https://u:p@ss@host/"), "https://***@host/");
        // 路径或 query 里的 @ 不是 userinfo
        assert!(matches!(redact("https://host/users/@me"), Cow::Borrowed(_)));
        assert_eq!(redact("https://host/p?mail=a@b.c"), "https://host/p?***");
        // 不带 scheme 的邮件地址不受影响
        assert!(matches!(redact("contact me@example.com"), Cow::Borrowed(_)));
    }

    #[test]
    fn keys_in_url_paths_are_masked() {
        assert_eq!(
            redact("POST https://gw.example.com/keys/sk-abcdefghijklmnop1234?x=1 503"),
            "POST https://gw.example.com/keys/***?*** 503"
        );
        // 路径里的普通单词不受影响
        assert!(matches!(
            redact("GET https://api.example.com/v1/task-runner/desk-lamp"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn multibyte_text_is_preserved() {
        assert_eq!(redact("密钥 sk-1234567890abcdefXYZ 已保存"), "密钥 *** 已保存");
        assert_eq!(redact("设置已保存"), "设置已保存");
    }

    #[test]
    fn timestamps_are_utc_iso() {
        assert_eq!(utc_timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(utc_timestamp(1_760_081_462_123), "2025-10-10T07:31:02.123Z");
    }

    #[test]
    fn level_mapping() {
        assert_eq!(level_filter(LogLevel::Warn), log::LevelFilter::Warn);
        assert_eq!(level_filter(LogLevel::Debug), log::LevelFilter::Debug);
    }
}
