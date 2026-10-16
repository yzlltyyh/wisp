//! 验证码识别：记录时（判断是否给条目定过期时间）与芯片条共用同一个检测器。
//!
//! 记录时一旦判定为验证码，条目会在 [`TTL_MS`] 后被自动删除，误判的代价是用户的内容丢失，因此这里宁可漏判：
//! - 强关键词（「验证码」「动态码」「verification code」「one-time password」「認証コード」…）几乎只出现在
//!   验证码消息里，单独出现即可；「OTP」只在紧跟着码时算强关键词（「Use OTP 839201」），
//!   「OTP-1234: fix login retry」这类工单号不算；
//! - 弱关键词（「确认码」「密码」「security code」「access code」「your … code / PIN」「code:」、单独的「OTP」
//!   「2FA」…）在门禁码、订座号、许可证号、银行卡 PIN 里同样常见，必须另有验证码短信特有的线索才算：
//!   分钟级的有效期（「5 分钟内有效」「expires in 10 minutes」）、勿泄露提示（「请勿泄露」「do not share」）、
//!   或短信签名（开头或结尾的「【招商银行】」）；紧挨在前面的词表明是长期有效的码（门禁、促销、恢复码……）时
//!   即使有线索也不算；
//! - 候选只看关键词前后很短的窗口，取离关键词最近的一个；
//! - 候选是 4–8 位数字（或「123-456」「123 456」两段式），或含数字的 4–8 位字母数字；
//! - 排除年份（后接「年」或日期分隔符）、金额（货币符号、小数、「元」「%」）、电话与带连字符的长号码的一段、
//!   链接 / 域名 / 邮箱里的片段。

use std::sync::LazyLock;

use regex::Regex;
use wisp_types::ClipItemMeta;

use super::recognize::Span;

/// 验证码条目的保留时长：10 分钟（所有设备一致，同步时按记录里的绝对时间判断）
pub const TTL_MS: i64 = 10 * 60 * 1000;

/// 记录时只检测不超过这么多字节的文本：验证码短信都很短，长文本里出现「验证码」多半是在讲别的事
pub const SCAN_BYTES: usize = 1024;

/// 关键词之前多少个字符内的候选算数（「G-482915 is your Google verification code」）
const WINDOW_BEFORE_CHARS: usize = 24;
/// 关键词之后多少个字符内的候选算数（「验证码为 482915」「verification code is: 482915」）
const WINDOW_AFTER_CHARS: usize = 40;
/// 弱关键词前后多少个字符内找有效期 / 勿泄露线索：验证码短信很短，线索就在码的附近；
/// 限定范围是为了长文本（芯片条最多扫 16 KB）里不把远处无关的「有效期」算进来
const CUE_WINDOW_CHARS: usize = 160;
/// 判断「前面是不是长期有效的码」时往前看多少个字符（大约两个英文词）
const QUALIFIER_CHARS: usize = 24;

/// 记录时用：这段文本是不是一条验证码消息（超过 [`SCAN_BYTES`] 的一律不是）
pub fn detect(text: &str) -> bool {
    text.len() <= SCAN_BYTES && find(text).is_some()
}

/// 条目是否已到期（收藏的不会过期）。读取时用它过滤：系统睡眠期间定时器会晚到，不能让过期的验证码继续显示
pub fn is_expired(meta: &ClipItemMeta, now_ms: i64) -> bool {
    !meta.pinned && meta.expires_at.is_some_and(|exp| exp <= now_ms)
}

/// 文本里的验证码（离关键词最近的一个）；值去掉了两段式中间的分隔符
pub(crate) fn find(text: &str) -> Option<Span> {
    let mut best: Option<(usize, Span)> = None;
    for kw in keywords(text) {
        // 紧跟型关键词只认紧挨着的那个候选，不在窗口里另找（「Enable OTP login for user 12345」）
        let lo = if kw.adjacent {
            kw.end
        } else {
            back_chars(text, kw.start, WINDOW_BEFORE_CHARS)
        };
        let hi = fwd_chars(text, kw.end, WINDOW_AFTER_CHARS);
        for m in CANDIDATE.find_iter(&text[lo..hi]) {
            let (s, e) = (lo + m.start(), lo + m.end());
            if kw.adjacent && s != kw.end {
                break;
            }
            // 候选与关键词重叠（关键词本身含字母数字，例如「2fa」）
            if s < kw.end && e > kw.start {
                continue;
            }
            if !plausible(text, s, e) {
                continue;
            }
            let dist = if e <= kw.start {
                text[e..kw.start].chars().count()
            } else {
                text[kw.end..s].chars().count()
            };
            if best.as_ref().is_none_or(|(d, _)| dist < *d) {
                let value: String = text[s..e].chars().filter(char::is_ascii_alphanumeric).collect();
                best = Some((
                    dist,
                    Span {
                        start: s,
                        end: e,
                        value,
                    },
                ));
            }
        }
    }
    best.map(|(_, span)| span)
}

/// 一个关键词出现的位置（字节区间）
#[derive(Clone, Copy)]
struct Keyword {
    start: usize,
    end: usize,
    /// 候选必须从 `end` 处开始（「OTP 839201」）
    adjacent: bool,
}

/// 强关键词：几乎只用于验证码。CJK 关键词不需要词边界；英文用 ASCII 词边界（Unicode 词边界会让 lazy DFA 退回慢速引擎）
static STRONG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)验证码|驗證碼|校验码|校驗碼|动态码|動態碼|动态密码|動態密碼|动态口令|動態口令|一次性密码|一次性密碼",
        r"|認証コード|確認コード|認証番号|ワンタイムパスワード|인증번호|인증 ?코드",
        r"|(?-u:\b)(?:verification|verify|one[- ]?time|2fa|two[- ]factor|2-step|two[- ]step)",
        r"(?:\s+(?:authentication|verification))?\s+(?:code|passcode|pin|password)(?-u:\b)",
    ))
    .expect("静态正则")
});

/// 「OTP」后面紧跟着码（「Use OTP 839201」「Your OTP is: 839201」「OTP code 839201」）。匹配到分隔符为止，
/// 候选必须从匹配末尾开始；「OTP-1234」不匹配
static OTP_LEAD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?-u:\b)otp(?-u:\b)(?:\s+code)?(?:\s+is)?(?:\s*[:：]\s*|\s+)").expect("静态正则")
});

/// 弱关键词：也常用于长期有效的码，需要另有线索（见 [`cued`]）
static WEAK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)确认码|確認碼|安全码|安全碼|登录码|登錄碼|登入碼|认证码|認證碼|短信码|簡訊碼|密码|密碼",
        r"|(?-u:\b)(?:security|confirmation|login|log-in|sign-in|signin|access|auth|authentication|authorization",
        r"|sms|text|temporary|temp)\s+(?:code|passcode|pin|password)(?-u:\b)",
        r"|(?-u:\b)(?:otp|passcode|2fa)(?-u:\b)",
        r"|(?-u:\b)code\s*[:：]",
    ))
    .expect("静态正则")
});

/// 「your Uber code」「your PIN」：中间至多 3 个词。属于弱关键词
static YOUR_CODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?-u:\b)your\s+((?:[a-z0-9][a-z0-9'\-]*\s+){0,3}?)(?:code|pin|passcode)(?-u:\b)")
        .expect("静态正则")
});

/// 分钟级的有效期（「5 分钟内有效」「expires in 10 minutes」「10分間有効」）。「valid until Friday」这类
/// 按天计的期限是促销码的特征，不算
static EXPIRY_CUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)(?-u:\b)(?:expire[sd]?|expiring|expiry|expiration|valid|validity)(?-u:\b)[^\n.。]{0,30}?",
        r"(?-u:\b)[0-9]{1,3}\s*(?:minutes?|mins?|seconds?|secs?)(?-u:\b)",
        r"|(?-u:\b)[0-9]{1,3}\s*(?:minutes?|mins?)(?-u:\b)[^\n.。]{0,20}?(?:expire|valid)",
        r"|[0-9]{1,3}\s*(?:分钟|分鐘|分|秒)[^，。,.\n]{0,8}?(?:有效|失效|过期|過期|完成|输入|輸入|使用)",
        r"|(?:有效期|有效时间|有效時間)[^，。,.\n]{0,6}?[0-9]{1,3}\s*(?:分钟|分鐘|分|秒)",
        r"|[0-9]{1,3}\s*分(?:間)?[^、。\n]{0,6}?有効|有効期限[^、。\n]{0,6}?[0-9]{1,3}\s*分",
    ))
    .expect("静态正则")
});

/// 勿泄露提示（「请勿泄露」「切勿告知他人」「Don't share it」「Do not give this code to anyone」）
static SHARE_CUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)(?-u:\b)(?:do\s+not|don['’]?t|never|not\s+to)\s+(?:share|give|disclose|forward|tell|reveal|send)(?-u:\b)",
        r"|(?:请勿|請勿|切勿|勿|不要|严禁|嚴禁)[^，。,.\n]{0,4}?",
        r"(?:泄露|洩露|泄漏|洩漏|透露|告诉|告訴|告知|转发|轉發|提供|转告|轉告|交给|交給)",
        r"|他人に(?:教え|伝え)|(?:教え|伝え|共有し)ないで",
    ))
    .expect("静态正则")
});

/// 短信签名：开头或结尾的「【招商银行】」，或 Android 短信自动填充的「<#>」前缀
static SENDER_CUE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:【[^】\n]{1,20}】|<#>)|【[^】\n]{1,20}】\s*$").expect("静态正则"));

/// 弱关键词前紧挨着这些词时是长期有效的码（门禁码、促销码、恢复码、许可证号、银行卡 PIN……），即使有线索也不算
const NOT_OTP_WORDS: &[&str] = &[
    "promo",
    "promotion",
    "promotional",
    "coupon",
    "discount",
    "voucher",
    "gift",
    "referral",
    "invite",
    "invitation",
    "redeem",
    "redemption",
    "reward",
    "rewards",
    "zip",
    "postal",
    "post",
    "area",
    "country",
    "dial",
    "error",
    "status",
    "exit",
    "return",
    "response",
    "tracking",
    "source",
    "qr",
    "color",
    "colour",
    "hex",
    "license",
    "licence",
    "product",
    "activation",
    "serial",
    "registration",
    "member",
    "membership",
    "booking",
    "reservation",
    "ticket",
    "door",
    "gate",
    "garage",
    "alarm",
    "lock",
    "locker",
    "safe",
    "wifi",
    "wi-fi",
    "network",
    "router",
    "admin",
    "card",
    "atm",
    "debit",
    "credit",
    "sim",
    "puk",
    "voicemail",
    "parking",
    "building",
    "entry",
    "entrance",
    "room",
    "backup",
    "recovery",
    "app",
    "account",
    "default",
    "initial",
    "master",
];

/// 同上，中文的「门禁密码」「初始密码」「支付密码」「银行卡密码」……
const NOT_OTP_PREFIXES: &[&str] = &[
    "门禁",
    "門禁",
    "门锁",
    "門鎖",
    "保险箱",
    "保險箱",
    "保险柜",
    "保險櫃",
    "无线",
    "無線",
    "网络",
    "網絡",
    "網路",
    "初始",
    "默认",
    "默認",
    "原始",
    "支付",
    "交易",
    "取款",
    "提款",
    "卡",
    "账户",
    "帳戶",
    "账号",
    "帳號",
    "登录",
    "登錄",
    "登入",
    "开机",
    "開機",
    "管理员",
    "管理員",
    "邀请",
    "邀請",
    "取件",
    "提货",
    "提貨",
    "兑换",
    "兌換",
    "优惠",
    "優惠",
    "激活",
    "序列",
];

/// 候选：两段式数字，或 4–8 位字母数字
static CANDIDATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?-u:\b)(?:[0-9]{3}[- ][0-9]{3}|[0-9A-Za-z]{4,8})(?-u:\b)").expect("静态正则"));

/// 文本里全部算数的关键词：强关键词全部算，弱关键词只在前面不是长期码、且附近有验证码线索时算
fn keywords(text: &str) -> Vec<Keyword> {
    let at = |start, end, adjacent| Keyword { start, end, adjacent };
    let mut out: Vec<Keyword> = STRONG.find_iter(text).map(|m| at(m.start(), m.end(), false)).collect();
    out.extend(OTP_LEAD.find_iter(text).map(|m| at(m.start(), m.end(), true)));
    let mut weak: Vec<(usize, usize)> = WEAK
        .find_iter(text)
        .filter(|m| !long_lived_before(text, m.start()))
        .map(|m| (m.start(), m.end()))
        .collect();
    for c in YOUR_CODE.captures_iter(text) {
        let (Some(all), Some(gap)) = (c.get(0), c.get(1)) else {
            continue;
        };
        let gap = gap.as_str().to_ascii_lowercase();
        if gap.split_whitespace().any(|w| NOT_OTP_WORDS.contains(&w)) {
            continue;
        }
        weak.push((all.start(), all.end()));
    }
    out.extend(
        weak.into_iter()
            .filter(|&(s, e)| cued(text, s, e))
            .map(|(s, e)| at(s, e, false)),
    );
    out
}

/// 弱关键词 `text[s..e]` 附近有没有验证码短信特有的线索（有效期、勿泄露、短信签名）
fn cued(text: &str, s: usize, e: usize) -> bool {
    let near = &text[back_chars(text, s, CUE_WINDOW_CHARS)..fwd_chars(text, e, CUE_WINDOW_CHARS)];
    EXPIRY_CUE.is_match(near) || SHARE_CUE.is_match(near) || SENDER_CUE.is_match(text)
}

/// 弱关键词前面紧挨着的词是否表明这是长期有效的码（「door access code」「门禁密码」）
fn long_lived_before(text: &str, kw_start: usize) -> bool {
    let before = text[back_chars(text, kw_start, QUALIFIER_CHARS)..kw_start].trim_end();
    if NOT_OTP_PREFIXES.iter().any(|p| before.ends_with(p)) {
        return true;
    }
    before
        .rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '\''))
        .filter(|w| !w.is_empty())
        .take(2)
        .any(|w| NOT_OTP_WORDS.contains(&w.to_ascii_lowercase().as_str()))
}

/// 候选 `text[s..e]` 是否像验证码（形状与前后文排除）
fn plausible(text: &str, s: usize, e: usize) -> bool {
    let tok = &text[s..e];
    let digits = tok.bytes().filter(u8::is_ascii_digit).count();
    let letters = tok.bytes().filter(u8::is_ascii_alphabetic).count();
    if digits == 0 {
        return false;
    }
    let all_digits = letters == 0;
    let b = text.as_bytes();
    let prev = s.checked_sub(1).map(|i| b[i]);
    let prev2 = s.checked_sub(2).map(|i| b[i]);
    let next = b.get(e).copied();
    let next2 = b.get(e + 1).copied();
    let rest = &text[e..];
    // 链接、域名、邮箱、参数里的片段（句末的句点不算）
    if matches!(prev, Some(b'/' | b'=' | b'@' | b'#' | b'&' | b'?' | b'.' | b'_'))
        || matches!(next, Some(b'/' | b'@' | b'=' | b'&' | b'_'))
        || (next == Some(b'.') && next2.is_some_and(|c| c.is_ascii_alphanumeric()))
    {
        return false;
    }
    // 金额：货币符号、小数、「元」「%」
    if text[..s].trim_end().ends_with(['¥', '￥', '$', '€', '£'])
        || (prev == Some(b'.') && prev2.is_some_and(|c| c.is_ascii_digit()))
        || rest.starts_with(['%', '元', '块'])
        || rest.trim_start().starts_with("元")
    {
        return false;
    }
    // 电话与带连字符的长号码的一段（「400-820-8820」「+86 138…」）
    if (matches!(prev, Some(b'-' | b' ')) && prev2.is_some_and(|c| c.is_ascii_digit()) && !tok.contains([' ', '-']))
        || (matches!(next, Some(b'-')) && next2.is_some_and(|c| c.is_ascii_digit()))
        || prev == Some(b'+')
    {
        return false;
    }
    // 年份（「2025年」「2025-10-15」）
    if all_digits && tok.len() == 4 {
        let year = tok.parse::<u32>().unwrap_or(0);
        if (1900..=2099).contains(&year)
            && (rest.starts_with('年')
                || (matches!(next, Some(b'-' | b'/' | b'.')) && next2.is_some_and(|c| c.is_ascii_digit())))
        {
            return false;
        }
    }
    true
}

/// 从 `from` 往前数 `n` 个字符的字节位置
fn back_chars(text: &str, from: usize, n: usize) -> usize {
    text[..from]
        .char_indices()
        .rev()
        .nth(n.saturating_sub(1))
        .map_or(0, |(i, _)| i)
}

/// 从 `from` 往后数 `n` 个字符的字节位置
fn fwd_chars(text: &str, from: usize, n: usize) -> usize {
    text[from..].char_indices().nth(n).map_or(text.len(), |(i, _)| from + i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(text: &str) -> Option<String> {
        find(text).map(|s| {
            // 区间与值一致（值只是去掉了分隔符）
            let raw: String = text[s.start..s.end]
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .collect();
            assert_eq!(raw, s.value, "{text}");
            s.value
        })
    }

    #[test]
    fn recognizes_common_messages() {
        let cases = [
            // 强关键词：单独出现即可
            ("【招商银行】您的验证码为482915，请于5分钟内完成验证。", "482915"),
            ("验证码：7319（10分钟内有效），请勿泄露给他人", "7319"),
            ("您正在登录，动态密码 663102，切勿告知他人。", "663102"),
            ("【某某】驗證碼 552190，有效期5分鐘", "552190"),
            ("G-482915 is your Google verification code.", "482915"),
            ("Your one-time password is 551204", "551204"),
            ("Use OTP 839201 to complete your payment.", "839201"),
            ("Your OTP is: 839201", "839201"),
            ("認証コード：812345 を入力してください", "812345"),
            ("確認コード: 482915", "482915"),
            ("[Wisp] 인증번호 [337119]를 입력해주세요", "337119"),
            ("Your verification code is A7K9Q2", "A7K9Q2"),
            // 弱关键词 + 有效期 / 勿泄露 / 短信签名
            ("Your Apple ID Code is: 905512. Don't share it with anyone.", "905512"),
            (
                "Your WhatsApp code: 123-456\nDon't share this code with others",
                "123456",
            ),
            ("Your Uber code: 4821. Never share this code.", "4821"),
            ("Telegram code: 75319. Do not give this code to anyone.", "75319"),
            ("Your security code is 482915. It expires in 10 minutes.", "482915"),
            ("Your code:482915 (valid for 5 minutes)", "482915"),
            ("839201 is your OTP for login. Valid for 10 mins.", "839201"),
            ("您的确认码是 662817，请勿泄露。", "662817"),
            ("【某银行】您的短信密码为 304918", "304918"),
        ];
        for (text, want) in cases {
            assert_eq!(code(text).as_deref(), Some(want), "{text}");
            assert!(detect(text), "{text}");
        }
    }

    #[test]
    fn rejects_lookalikes() {
        let cases = [
            // 没有验证码关键词
            "会议改到 2025 年 10 月 15 日 1430 开始",
            "Order 482915 has shipped",
            "The door code is 4821",
            "The code is 2024",
            "The door code is 4821, I'll be there in 10 minutes",
            // 需要长期保留的码
            "您的取件码 8-3-2201，请凭码取件",
            "邀请码 A8K2Q9，注册即得 10 元",
            "Use promo code SAVE2025 at checkout",
            "Your discount code is SPRING25",
            "Enter your zip code 94103",
            "HTTP error code 5001",
            // 弱关键词，没有任何验证码线索
            "Your PIN is 1234",
            "Your confirmation code is ABC123",
            "Your license code is 7HG2-K8P1",
            "Your code:482915",
            "Your security code is 482915",
            "Telegram login code: 75319",
            "OTP-1234: fix login retry",
            "Enable 2FA for user 12345",
            "Enable OTP login for user 12345",
            "密码 4821，10分钟后到",
            "Your code SPRING25 is valid until Friday",
            // 弱关键词前面是长期码，即使有线索也不算
            "Your door access code is 4821. Do not share it.",
            "门禁密码 4821，请勿泄露",
            "WiFi 密码 20251015，请勿外传给他人，请勿泄露",
            "Promo code: SAVE25, expires in 30 minutes",
            "Your recovery code is ab12-cd34. Do not share it.",
            "Your card PIN is 7301. Do not share it with anyone.",
            // 年份、金额、电话、链接里的片段
            "验证码将于2025年过期",
            "验证码服务费 ¥1280",
            "验证码套餐 12.50 元",
            "验证码问题请致电 400-820-8820",
            "验证码问题请致电 +86 13800138000",
            "验证码说明见 https://example.com/otp/a1b2c3",
            // 只有字母
            "verification code is ABCDEF",
        ];
        for text in cases {
            assert_eq!(code(text), None, "{text}");
            assert!(!detect(text), "{text}");
        }
    }

    #[test]
    fn otp_lead_needs_the_code_right_after() {
        // 「OTP」后面隔了别的词：只当弱关键词，需要线索
        assert_eq!(code("OTP for login 482915"), None);
        assert_eq!(
            code("OTP for login 482915, valid for 5 minutes").as_deref(),
            Some("482915")
        );
    }

    #[test]
    fn nearest_candidate_wins() {
        // 关键词后面最近的候选，而不是前面更远的数字
        assert_eq!(
            code("订单 98765432 已支付。验证码 102938，5 分钟内有效").as_deref(),
            Some("102938")
        );
    }

    #[test]
    fn long_texts_are_not_otp_at_ingest() {
        let long = format!("{}验证码 482915", "说明".repeat(400));
        assert!(long.len() > SCAN_BYTES);
        assert!(!detect(&long));
        assert!(find(&long).is_some(), "芯片条仍然可以识别");
    }

    #[test]
    fn cues_far_from_a_weak_keyword_do_not_count() {
        // 长文本里远处的「请勿泄露」与这里的「Your PIN」无关
        let text = format!("Your PIN is 1234.{}请勿泄露", " lorem ipsum".repeat(40));
        assert_eq!(code(&text), None);
    }

    #[test]
    fn expiry_ignores_favorites() {
        let mut m = ClipItemMeta {
            id: 1,
            kind: wisp_types::ClipKind::Text,
            created_at: 0,
            last_used_at: 0,
            use_count: 1,
            pinned: false,
            source: None,
            preview: String::new(),
            char_count: None,
            line_count: None,
            truncated: false,
            image: None,
            files: None,
            link: None,
            color: None,
            name: None,
            expires_at: Some(1_000),
        };
        assert!(!is_expired(&m, 999));
        assert!(is_expired(&m, 1_000));
        m.pinned = true;
        assert!(!is_expired(&m, 5_000));
        m.pinned = false;
        m.expires_at = None;
        assert!(!is_expired(&m, i64::MAX));
    }
}
