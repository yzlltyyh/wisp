//! 隐私规则：应用排除名单与疑似密钥检测。各种隐私标记由 platform 打在 `RawClipboard.concealed` 上。

use std::sync::LazyLock;

use regex::Regex;
use wisp_types::{AppRef, ExcludedApp};

/// 隐私规则
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrivacyRules {
    pub excluded_apps: Vec<ExcludedApp>,
    pub secret_detection: bool,
}

/// 内置、不可移除的排除项：系统自带的密码管理器。macOS 15+ 的「密码」应用
/// （com.apple.Passwords）是否给拷贝的密码打 ConcealedType 未经核实；不论有没有，它的内容都不该进历史。放在这里而不是
/// `default_excluded_apps`：默认名单只对新配置生效，已有用户的 settings.json 不会自动补上
pub const BUILTIN_EXCLUDED: [&str; 1] = ["com.apple.Passwords"];

impl PrivacyRules {
    /// 来源应用是否在排除名单中：按 `AppRef.id`（mac bundle id / Win exe / X11 WM_CLASS class）与
    /// `alt_id`（X11 WM_CLASS instance）比较，忽略 ASCII 大小写。名单里三端条目都在，id 形态互不相同，因此不按 os 过滤。
    /// 内置的排除项（系统自带的密码管理器）始终生效。同步来的条目同样按它判断（来源应用标识随条目一起同步）
    pub fn excludes(&self, source: Option<&AppRef>) -> bool {
        let Some(app) = source else { return false };
        if BUILTIN_EXCLUDED.iter().any(|id| app.id.eq_ignore_ascii_case(id)) {
            return true;
        }
        self.excluded_apps.iter().any(|e| {
            let id = e.app_id.trim();
            !id.is_empty()
                && (app.id.eq_ignore_ascii_case(id)
                    || app.alt_id.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(id)))
        })
    }
}

/// 疑似密钥：`sk-…`（OpenAI / Anthropic 等）、`AKIA…`（AWS）、PEM 私钥、JWT、GitHub / GitLab / Slack / Google /
/// Stripe / SendGrid 令牌、`API_KEY=…` 一类的赋值，以及整段是一个高熵长串（≥ 24 字符的 base64 / base62 随机串）。
///
/// 刻意不判为密钥的：全小写或全大写的十六进制串（提交号、校验和）、UUID、驼峰标识符、路径与 URL；
/// 常被复制、需要再次粘贴的公开标识（比特币地址、IPFS CID、`sha512-…` 完整性串、YouTube 播放列表 / 频道 ID、
/// Google Drive 文件 ID、以太坊地址）；赋值右侧是代码里的名字
/// （`accessKeyId: config.s3AccessKeyId`、`token = session.oauth2Token`、`API_KEY_HEADER = 'x-api-key-v2'`）或
/// 「单词 + 数字」形式的占位值（`token = abc12345`）——命中会让整段复制内容被静默丢弃，误伤的代价远大于漏判。
///
/// 正则里的词边界一律用 ASCII 版本（`(?-u:\b)`）：Unicode 词边界遇到非 ASCII 输入时 lazy DFA 无法工作，
/// 会退回慢速引擎（1 MB 中文约慢一个数量级）
pub fn looks_like_secret(text: &str) -> bool {
    let t = text.trim();
    if t.len() < 16 {
        return false;
    }
    known_token(t) || sk_token(t) || secret_assignment(t) || high_entropy_token(t)
}

/// 格式明确、不需要二次判断的令牌
fn known_token(t: &str) -> bool {
    static KNOWN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"(?-u:\b)(?:AKIA|ASIA|ABIA|ACCA)[0-9A-Z]{16}(?-u:\b)",
            r"|-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----",
            r"|(?-u:\b)eyJ[A-Za-z0-9_\-]{8,}\.eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}",
            r"|(?-u:\b)gh[pousr]_[A-Za-z0-9]{36,}(?-u:\b)",
            r"|(?-u:\b)github_pat_[A-Za-z0-9_]{50,}(?-u:\b)",
            r"|(?-u:\b)xox[abposr]-[A-Za-z0-9\-]{10,}",
            r"|(?-u:\b)AIza[0-9A-Za-z_\-]{35}(?-u:\b)",
            // Google OAuth 访问令牌（点分形式，赋值规则会把它当成成员访问排除，所以单列）
            r"|(?-u:\b)ya29\.[0-9A-Za-z_\-]{20,}",
            // SendGrid：`SG.` + 22 + `.` + 43（同样是点分形式）
            r"|(?-u:\b)SG\.[0-9A-Za-z_\-]{22}\.[0-9A-Za-z_\-]{43}(?-u:\b)",
            r"|(?-u:\b)[sr]k_(?:live|test)_[A-Za-z0-9]{16,}(?-u:\b)",
            r"|(?-u:\b)glpat-[A-Za-z0-9_\-]{20,}(?-u:\b)",
        ))
        .expect("静态正则")
    });
    KNOWN.is_match(t)
}

/// `sk-` 开头、主体 ≥ 20 字符且同时含字母和数字（排除 `sk-learn-is-a-library` 这类连字符单词）
fn sk_token(t: &str) -> bool {
    static SK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?-u:\b)sk-[A-Za-z0-9_\-]{20,}").expect("静态正则"));
    SK.find_iter(t).any(|m| {
        let body = &m.as_str()[3..];
        body.bytes().any(|b| b.is_ascii_digit()) && body.bytes().any(|b| b.is_ascii_alphabetic())
    })
}

/// `API_KEY=…`、`"client_secret": "…"`、`password: …` 一类的赋值：值 ≥ 8 字符、只含令牌字符、同时含字母和数字，
/// 不像代码里的名字（[`looks_like_name`]），并且字符类别（大写 / 小写 / 数字 / 符号）至少切换两次——
/// 只切换一次的是「单词 + 数字」（`abc12345`、`test1234`、`user2024`），在代码与文档里几乎都是示例或占位值；
/// 代价是这种形状的弱口令不报。
///
/// 匹配从关键字开始（`DB_PASSWORD=` 从 `PASSWORD` 匹配起），以字面量开头的正则可以用前缀过滤器在关键字之间跳跃，
/// 1 MB 文本只需约 0.1 ms 级的扫描；键名部分只做 ASCII 大小写折叠（`(?i-u)`），键名都是 ASCII
fn secret_assignment(t: &str) -> bool {
    static ASSIGN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?i-u:secret|token|passwd|password|api[_\-]?key|access[_\-]?key|private[_\-]?key|auth[_\-]?key)(?i-u:[a-z0-9_.\-]*)["']?\s*[:=]\s*["']?([^\s"'`,;]{8,})"#,
        )
        .expect("静态正则")
    });
    ASSIGN.captures_iter(t).any(|c| {
        c.get(1).is_some_and(|v| {
            let v = v.as_str();
            v.bytes().all(is_token_byte)
                && v.bytes().any(|b| b.is_ascii_digit())
                && v.bytes().any(|b| b.is_ascii_alphabetic())
                && class_transitions(v.as_bytes()) >= 2
                && !looks_like_name(v, true)
        })
    })
}

/// 整段是一个高熵长串：24–512 字节、只含 base64 / base64url 字符、同时含大写小写数字，
/// 香农熵 ≥ 3.9 bit/字符，字符类别切换频繁（随机串约 0.6，驼峰标识符约 0.3），不像代码里的名字，也不是公开标识
fn high_entropy_token(t: &str) -> bool {
    let b = t.as_bytes();
    if !(24..=512).contains(&b.len()) || !b.iter().copied().all(is_token_byte) {
        return false;
    }
    let has = |f: fn(&u8) -> bool| b.iter().any(f);
    if !(has(u8::is_ascii_uppercase) && has(u8::is_ascii_lowercase) && has(u8::is_ascii_digit)) {
        return false;
    }
    shannon_entropy(b) >= 3.9
        && class_transition_ratio(b) >= 0.35
        && !looks_like_name(t, false)
        && !looks_like_public_id(t)
}

/// 形状随机、但公开且常被复制粘贴的标识（只用于整段高熵规则；格式明确的令牌规则不受影响）：
/// - 比特币 legacy / P2SH 地址：`1` / `3` 开头的 26–35 位 base58；IPFS CIDv0：`Qm` + 44 位 base58；
/// - 十六进制串（可带 `0x`）：以太坊地址的 EIP-55 校验和用大小写混排；
/// - Subresource Integrity / npm 的完整性串：`sha1-` / `sha256-` / `sha384-` / `sha512-` + base64；
/// - YouTube 播放列表 / 频道等 ID：`PL` `UU` `UC` `OL` `RD` `FL` `LL` 开头的 18–43 位 base64url；
/// - Google Drive / Docs 文件 ID：`1` 开头的 33 或 44 位 base64url，或 `0B` 开头的 28 位。
///
/// 前缀与长度限定得很窄：随机密钥恰好落进这些形状的概率约 0.5%–2%（例如 44 位随机串以 `1` 开头的概率 1/62），
/// 单测按统计上限守住
fn looks_like_public_id(t: &str) -> bool {
    let b = t.as_bytes();
    let n = b.len();
    let is_base58 = |s: &[u8]| {
        s.iter()
            .all(|c| c.is_ascii_alphanumeric() && !matches!(c, b'0' | b'O' | b'I' | b'l'))
    };
    let is_b64url = |s: &[u8]| s.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'));
    let hex = b.strip_prefix(b"0x").unwrap_or(b);
    (matches!(b[0], b'1' | b'3') && (26..=35).contains(&n) && is_base58(b))
        || (b.starts_with(b"Qm") && n == 46 && is_base58(b))
        || (!hex.is_empty() && hex.iter().all(u8::is_ascii_hexdigit))
        || ["sha1-", "sha256-", "sha384-", "sha512-"]
            .iter()
            .any(|p| t.starts_with(p))
        || (["PL", "UU", "UC", "OL", "RD", "FL", "LL"]
            .iter()
            .any(|p| t.starts_with(p))
            && (18..=43).contains(&n)
            && is_b64url(b))
        || (b[0] == b'1' && matches!(n, 33 | 44) && is_b64url(b))
        || (b.starts_with(b"0B") && n == 28 && is_b64url(b))
}

/// 值的形状像代码里的名字，而不是随机串：
/// - 成员访问 / 点分路径：按 `.` 切成 ≥ 2 段，每段（再按 `-` `_` 切开后）都由「单词」组成
///   （`config.s3AccessKeyId`、`session.oauth2Token`、`process.env.API_KEY`、`v2.1.0`）；
///   随机令牌里几乎不会出现 `.`，带点的令牌格式（JWT、`ya29.`、`SG.`）由 [`known_token`] 先行识别；
/// - kebab-case / snake_case / SCREAMING_SNAKE：按 `-` `_` 切成 ≥ 2 段，所有段大小写一致，
///   每段是「字母 + 可选的末尾数字」或不超过 8 位的数字（`x-api-key-v2`、`DEFAULT_TOKEN_V2`）；
/// - 全小写的路径（`auth/v2/token`，见 [`looks_like_path`]）；
/// - 没有分隔符的驼峰名：小写开头（变量名；`strict` 为 false 时也允许大写开头的类型名），≥ 2 个词，
///   1–2 个字母的碎词不超过一半，大写字母不超过全部字母的 1/3（`oauth2Token`、`s3AccessKeyId`）。
///
/// 「单词」= 字母（全小写、首字母大写或全大写的缩写）+ 可选的末尾数字：数字夹在字母中间（`Tr0ub4dor`）
/// 或以数字开头（十六进制）的不算。随机串大小写频繁交替、数字随处出现，实测（每种长度各 20 万个）
/// 16 字符的 base62 随机串只有约 0.5% 被认作名字，32 字符约 0.03%，40 字符约 0.01%。
///
/// `strict` 用于赋值规则：大写开头、没有分隔符的单段（`ThisIsMyPassword2024`）更像人设的密码而不是变量，不排除
fn looks_like_name(v: &str, strict: bool) -> bool {
    let b = v.as_bytes();
    if b.contains(&b'/') {
        return looks_like_path(v);
    }
    if b.is_empty()
        || !b
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
    {
        return false;
    }
    if b.contains(&b'.') {
        let mut words = 0u32;
        for seg in v.split('.') {
            if seg.is_empty() {
                return false;
            }
            for part in seg.split(['-', '_']).filter(|p| !p.is_empty()) {
                if part.bytes().all(|c| c.is_ascii_digit()) {
                    if part.len() > 8 {
                        return false;
                    }
                    words += 1;
                } else {
                    let Some(w) = camel_words(part.as_bytes()) else {
                        return false;
                    };
                    words += w.count;
                }
            }
        }
        return words >= 2;
    }
    if b.contains(&b'-') || b.contains(&b'_') {
        let parts: Vec<&[u8]> = v
            .split(['-', '_'])
            .filter(|p| !p.is_empty())
            .map(str::as_bytes)
            .collect();
        let lower = parts.iter().all(|p| !p.iter().any(u8::is_ascii_uppercase));
        let upper = parts.iter().all(|p| !p.iter().any(u8::is_ascii_lowercase));
        return parts.len() >= 2 && (lower || upper) && parts.iter().all(|p| is_plain_word(p));
    }
    if strict && !b[0].is_ascii_lowercase() {
        return false;
    }
    let Some(w) = camel_words(b) else { return false };
    let letters = b.iter().filter(|c| c.is_ascii_alphabetic()).count();
    let upper = b.iter().filter(|c| c.is_ascii_uppercase()).count();
    w.count >= 2 && w.short * 2 <= w.count && upper * 3 <= letters
}

/// 全小写的相对 / 绝对路径（`auth/v2/token`、`./config/tokens/github2.json`）：每段由 `.` `-` `_` 连接的单词组成。
/// base64 里也有 `/`，但随机串几乎不可能全是小写
fn looks_like_path(v: &str) -> bool {
    let rest = v.trim_start_matches(['.', '~']).trim_start_matches('/');
    !rest.is_empty()
        && !v.bytes().any(|c| c.is_ascii_uppercase())
        && rest.split('/').all(|seg| {
            !seg.is_empty()
                && seg
                    .split(['.', '-', '_'])
                    .all(|p| p.is_empty() || is_plain_word(p.as_bytes()))
        })
}

/// 驼峰切词的统计
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct CamelWords {
    /// 词数
    count: u32,
    /// 只有 1–2 个字母的词数
    short: u32,
}

/// 把一段（只含字母数字）按驼峰边界切词：小写串只能出现在开头；之后每个词以大写开头（`Access`），
/// 连续大写是缩写（`HTTPServer` → `HTTP` / `Server`）；每个词末尾可以带数字，但数字后面不能紧跟小写字母。
/// 不满足时返回 None
fn camel_words(seg: &[u8]) -> Option<CamelWords> {
    let n = seg.len();
    if n == 0 || !seg[0].is_ascii_alphabetic() {
        return None;
    }
    let mut w = CamelWords::default();
    let mut i = 0;
    while i < n {
        let start = i;
        if seg[i].is_ascii_lowercase() {
            if w.count > 0 {
                return None;
            }
            while i < n && seg[i].is_ascii_lowercase() {
                i += 1;
            }
        } else if seg[i].is_ascii_uppercase() {
            let caps = i;
            while i < n && seg[i].is_ascii_uppercase() {
                i += 1;
            }
            if i < n && seg[i].is_ascii_lowercase() {
                if i - caps > 1 {
                    // 缩写后接新词：最后一个大写字母属于下一个词
                    i -= 1;
                    w.count += 1;
                    w.short += u32::from(i - start < 3);
                    continue;
                }
                while i < n && seg[i].is_ascii_lowercase() {
                    i += 1;
                }
            }
        } else {
            return None;
        }
        let letters = i - start;
        while i < n && seg[i].is_ascii_digit() {
            i += 1;
        }
        if i < n && seg[i].is_ascii_lowercase() {
            return None;
        }
        w.count += 1;
        w.short += u32::from(letters < 3);
    }
    Some(w)
}

/// 「字母 + 可选的末尾数字」或不超过 8 位的纯数字（kebab / snake 的一段）
fn is_plain_word(p: &[u8]) -> bool {
    let letters = p.iter().take_while(|c| c.is_ascii_alphabetic()).count();
    if letters == 0 {
        return !p.is_empty() && p.len() <= 8 && p.iter().all(u8::is_ascii_digit);
    }
    p[letters..].iter().all(u8::is_ascii_digit)
}

/// 令牌字符：字母数字与 `+ / = _ - . ~`
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'_' | b'-' | b'.' | b'~')
}

/// 每字节的香农熵（bit）
fn shannon_entropy(b: &[u8]) -> f64 {
    let mut freq = [0u32; 256];
    for &c in b {
        freq[usize::from(c)] += 1;
    }
    let n = b.len() as f64;
    freq.iter()
        .filter(|&&f| f > 0)
        .map(|&f| {
            let p = f64::from(f) / n;
            -p * p.log2()
        })
        .sum()
}

/// 字符类别：大写 / 小写 / 数字 / 其他
fn char_class(c: u8) -> u8 {
    if c.is_ascii_uppercase() {
        0
    } else if c.is_ascii_lowercase() {
        1
    } else if c.is_ascii_digit() {
        2
    } else {
        3
    }
}

/// 相邻字符类别不同的次数
fn class_transitions(b: &[u8]) -> usize {
    b.windows(2).filter(|w| char_class(w[0]) != char_class(w[1])).count()
}

/// 相邻字符类别不同的比例
fn class_transition_ratio(b: &[u8]) -> f64 {
    if b.len() < 2 {
        return 0.0;
    }
    class_transitions(b) as f64 / (b.len() - 1) as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use wisp_types::OsKind;

    #[test]
    fn secrets() {
        let positives = [
            // 48 位 sk- 串
            "sk-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789AbCdEfGhIjKl",
            "sk-proj-Xy12_ab-CDefGHijKLmnOPqrSTuv0123",
            "sk-ant-api03-AbCdEf0123456789-GhIjKlMnOp",
            "export OPENAI_API_KEY=sk-AbCdEfGhIjKlMnOpQrStUvWx12",
            "AKIAIOSFODNN7EXAMPLE",
            "aws_access_key_id = AKIAIOSFODNN7EXAMPLE",
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\n-----END RSA PRIVATE KEY-----",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjE=\n-----END OPENSSH PRIVATE KEY-----",
            "-----BEGIN PRIVATE KEY-----",
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
            "Authorization: Bearer eyJhbGciOiJSUzI1NiJ9.eyJpc3MiOiJ3aXNwIn0.c2lnbmF0dXJlLXBhcnQ",
            "ghp_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
            "xoxb-1234567890-abcdefghij",
            "AIzaSyA1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q",
            "sk_live_AbCdEf0123456789XyZ",
            "DB_PASSWORD=Tr0ub4dor3x",
            "\"client_secret\": \"9f8e7d6c5b4a3f2e1d0c\"",
            "Xk9mP2qR7vL4nB8wT3yZ6cF1hJ5sD0gA",
            "q3J8vN1xR6tY9wB2mK5pL7zC4fH0dG8s",
            // AWS 文档中的示例 secret（含 `/`，不是名字）
            "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "token: ya29.a0AfH6SMBx3kLmN0pQrStUvWxYz12345",
            "SENDGRID_API_KEY=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",
            "PASSWORD=s3cr3tP4ss",
            "api_key = '3f9a2b7c1d8e4f60'",
            "password: ThisIsMyPassword2024",
            "access_token=AbCdEfGh12345678",
            "secret: t3st1234x",
            "password = Example2024",
            // 密钥紧挨着中文（ASCII 词边界）
            "密钥AKIAIOSFODNN7EXAMPLE已作废",
        ];
        for p in positives {
            assert!(looks_like_secret(p), "应判为密钥：{p}");
        }
        let negatives = [
            "hello world",
            "sk-learn-is-a-machine-learning-library",
            "task-management-application-backend",
            "da39a3ee5e6b4b0d3255bfef95601890afd80709",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "550e8400-e29b-41d4-a716-446655440000",
            "handleClick2FactorAuthVerification",
            "src/components/Button/index.tsx",
            "https://example.com/path/to/Resource123?a=b",
            "The quick brown fox jumps over the lazy dog 12 times.",
            "password: ********",
            "API_KEY=your_api_key_here",
            "const token = getToken();",
            "AKIA is a prefix used by AWS",
            "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkq\n-----END PUBLIC KEY-----",
            "-----BEGIN CERTIFICATE-----",
            "今天的会议改到下午三点，地点在 3 楼会议室。",
            "README_FOR_CONTRIBUTORS_2024.md",
            // 赋值右侧是代码里的名字（成员访问、变量、名称字面量）
            "const client = new S3Client({ accessKeyId: config.s3AccessKeyId });",
            "let token = session.oauth2Token;",
            "export const API_KEY_HEADER = 'x-api-key-v2';",
            "headers['x-api-key'] = process.env.API_KEY_V2;",
            "self.secret_key = settings.SECRET_KEY2",
            "const accessToken = refreshToken2;",
            "password_hash = bcrypt2.hashSync(pw)",
            "token_url: auth/v2/token",
            "secretName: myapp-db-secret-v2",
            "API_TOKEN_TTL=3600s",
            "authKey: S3Client.defaultKey2",
            "config.s3AccessKeyId.toString2Fn",
            // 「单词 + 数字」形式的示例 / 占位值
            "token = abc12345",
            "secret: test1234",
            "password = example2024",
            // 常被复制的公开标识（形状随机，但不是密钥）
            "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa",
            "3J98t1WpEZ73CNmQviecrnyiWrnqRhWNLy",
            "QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdG",
            "sha512-WZv5x8eCqEjY2yQUs9ByN0D2B3DhZ6P4vMZ0bQ8RS1VgVp9lrE4rPd3aNgXcYGt6k3wwV6NxJRbQ5xU9a0oVdQ==",
            "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=",
            "PLrAXtmErZgOeiKm4sgNOknGvNjby9efdf",
            "UCBR8-60-B28hp2BmDPdntcQ",
            "1BxiMVs0XRA5nFMdKvBdBZjgmUUqptlbs74OgvE2upms",
            "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed",
        ];
        for n in negatives {
            assert!(!looks_like_secret(n), "不应判为密钥：{n}");
        }
    }

    /// 公开标识的形状限定得足够窄：随机串恰好落进去的比例很低（防止排除规则被放宽到漏掉真密钥）
    #[test]
    fn random_tokens_are_rarely_public_ids() {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = || {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            x.wrapping_mul(0x9e37_79b9_7f4a_7c15)
        };
        // 期望值：24 位约 0.8%（`1` / `3` 开头且恰好全是 base58 + YouTube 前缀），32–34 位约 0.5%，44 位约 1.8%（`1` 开头）
        for (len, max_rate) in [(24usize, 0.012), (32, 0.01), (34, 0.01), (44, 0.025)] {
            let mut hits = 0u32;
            for _ in 0..20_000 {
                let s: String = (0..len)
                    .map(|_| char::from(ALPHABET[(next() % ALPHABET.len() as u64) as usize]))
                    .collect();
                hits += u32::from(looks_like_public_id(&s));
            }
            let rate = f64::from(hits) / 20_000.0;
            assert!(rate <= max_rate, "长度 {len} 的随机串有 {rate:.4} 被认作公开标识");
        }
    }

    #[test]
    fn name_shapes() {
        for v in [
            "config.s3AccessKeyId",
            "session.oauth2Token",
            "x-api-key-v2",
            "process.env.API_KEY",
            "DEFAULT_TOKEN_V2",
            "s3AccessKeyId",
            "oauth2Token",
            "getHTTP2Response",
            "v2.1.0",
        ] {
            assert!(looks_like_name(v, true), "应是名字：{v}");
        }
        for v in [
            "Tr0ub4dor3x",
            "9f8e7d6c5b4a3f2e1d0c",
            "Xk9mP2qR7vL4nB8wT3yZ6cF1hJ5sD0gA",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "AbCdEfGh12",
            "password123",
            "s3cr3tP4ss",
            "Mixed-case_Parts",
            "a1b2-c3d4",
        ] {
            assert!(!looks_like_name(v, true), "不应是名字：{v}");
        }
        // 大写开头的单段：赋值规则里按人设密码处理，整段判断时按类型名处理
        assert!(!looks_like_name("ThisIsMyPassword2024", true));
        assert!(looks_like_name("ThisIsMyPassword2024", false));
        assert_eq!(
            camel_words(b"parseHTTPResponse2"),
            Some(CamelWords { count: 3, short: 0 })
        );
        assert_eq!(camel_words(b"oauth2token"), None);
    }

    /// 随机串很少被认作名字（统计上限，防止规则被放宽到误放密钥）
    #[test]
    fn random_tokens_are_rarely_names() {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            x.wrapping_mul(0x2545_f491_4f6c_dd1d)
        };
        for (len, max_rate) in [(16usize, 0.01), (24, 0.004), (40, 0.001)] {
            let (mut eligible, mut named) = (0u32, 0u32);
            for _ in 0..20_000 {
                let s: String = (0..len)
                    .map(|_| char::from(ALPHABET[(next() % ALPHABET.len() as u64) as usize]))
                    .collect();
                if s.bytes().any(|b| b.is_ascii_digit()) && s.bytes().any(|b| b.is_ascii_alphabetic()) {
                    eligible += 1;
                    named += u32::from(looks_like_name(&s, true));
                }
            }
            let rate = f64::from(named) / f64::from(eligible);
            assert!(rate <= max_rate, "长度 {len} 的随机串有 {rate:.4} 被认作名字");
        }
    }

    #[test]
    fn exclusion_matches_id_and_alt_id_case_insensitively() {
        let rules = PrivacyRules {
            excluded_apps: vec![
                ExcludedApp {
                    os: OsKind::Macos,
                    app_id: "com.1password.1password".into(),
                    name: "1Password".into(),
                },
                ExcludedApp {
                    os: OsKind::Linux,
                    app_id: "keepassxc".into(),
                    name: String::new(),
                },
                ExcludedApp {
                    os: OsKind::Windows,
                    app_id: "  ".into(),
                    name: String::new(),
                },
            ],
            secret_detection: true,
        };
        let app = |id: &str, alt: Option<&str>| AppRef {
            id: id.into(),
            alt_id: alt.map(Into::into),
            name: String::new(),
            pid: None,
            path: None,
            window: None,
        };
        assert!(rules.excludes(Some(&app("com.1Password.1password", None))));
        assert!(rules.excludes(Some(&app("org.keepassxc.KeePassXC", Some("keepassxc")))));
        assert!(!rules.excludes(Some(&app("com.apple.TextEdit", None))));
        assert!(!rules.excludes(Some(&app("", None))));
        assert!(!rules.excludes(None));
        // 系统「密码」应用：即使名单里没有（已有用户的旧配置）也不记录
        assert!(rules.excludes(Some(&app("com.apple.Passwords", None))));
        assert!(PrivacyRules::default().excludes(Some(&app("com.apple.passwords", None))));
    }
}
