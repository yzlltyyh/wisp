//! 识别器注册表：每种片段一个 [`Recognizer`]，登记在 [`RECOGNIZERS`] 里。加一种识别 = 在 `EntityKind` 里加一个值、
//! 写一个实现并登记（单测保证每个种类恰好有一个识别器）。
//!
//! 约定：
//! - 识别器只产出字节区间与规范化后的值，重叠的取舍、数量上限与 UTF-16 偏移换算都在 [`recognize`] 里统一做；
//! - 正则一律用 ASCII 字符类（`[0-9]` 而不是 `\d`，后者在 Unicode 模式下还匹配全角与其他文字的数字）与 ASCII 词边界
//!   （`(?-u:\b)`：Unicode 词边界遇到非 ASCII 输入时 lazy DFA 无法工作，会退回慢速引擎）。中文与英文紧挨着时
//!   （「电话13800138000」）ASCII 词边界同样成立；
//! - 识别结果与原文都不写日志。

use std::sync::LazyLock;

use regex::Regex;
use wisp_types::{Entity, EntityKind, SmartSettings};

use super::otp;

/// 识别器在原文里找到的一段（字节区间，左闭右开）
#[derive(Clone, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    /// 粘贴用的规范化值
    pub value: String,
}

impl std::fmt::Debug for Span {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 值是用户内容，不输出
        f.debug_struct("Span")
            .field("start", &self.start)
            .field("end", &self.end)
            .finish_non_exhaustive()
    }
}

/// 一种片段的识别器
pub trait Recognizer: Sync {
    fn kind(&self) -> EntityKind;
    /// 与其他种类重叠时优先级高的胜出（同级取更长的）。链接最高：链接里的数字不应再被认成电话或单号
    fn priority(&self) -> u8;
    /// 把找到的片段追加到 `out`（可以重叠，由调用方取舍）
    fn scan(&self, text: &str, out: &mut Vec<Span>);
}

/// 全部识别器（与 `EntityKind::ALL` 一一对应）
pub static RECOGNIZERS: &[&dyn Recognizer] = &[
    &Email, &Phone, &Url, &Ip, &Otp, &Tracking, &Order, &Amount, &Datetime, &IdCard, &BankCard,
];

/// 识别范围：只扫原文的前这么多字节（芯片只为看得见的内容服务，长文本不必全扫）
pub const SCAN_BYTES: usize = 16 * 1024;
/// 一条内容最多给出的片段数
pub const MAX_ENTITIES: usize = 32;

/// 识别选项：启用的种类（按设置里的分组）与数量上限
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecognizeOpts {
    pub smart: SmartSettings,
    pub limit: usize,
}

impl RecognizeOpts {
    pub fn allows(&self, kind: EntityKind) -> bool {
        self.smart.allows(kind)
    }
}

impl From<&SmartSettings> for RecognizeOpts {
    fn from(smart: &SmartSettings) -> Self {
        Self {
            smart: *smart,
            limit: MAX_ENTITIES,
        }
    }
}

impl Default for RecognizeOpts {
    fn default() -> Self {
        Self::from(&SmartSettings::default())
    }
}

/// 识别文本里的片段：按位置排序、互不重叠，最多 `opts.limit` 个；区间是 UTF-16 偏移
pub fn recognize(text: &str, opts: &RecognizeOpts) -> Vec<Entity> {
    let text = crate::infer::prefix_at_char_boundary(text, SCAN_BYTES);
    let mut found: Vec<(u8, EntityKind, Span)> = Vec::new();
    let mut buf = Vec::new();
    for r in RECOGNIZERS.iter().filter(|r| opts.allows(r.kind())) {
        buf.clear();
        r.scan(text, &mut buf);
        found.extend(
            buf.drain(..)
                .filter(|s| s.start < s.end)
                .map(|s| (r.priority(), r.kind(), s)),
        );
    }
    // 重叠取舍：优先级高的先占位，同级取更长的，再同就取靠前的
    found.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then((b.2.end - b.2.start).cmp(&(a.2.end - a.2.start)))
            .then(a.2.start.cmp(&b.2.start))
    });
    let mut kept: Vec<(EntityKind, Span)> = Vec::new();
    for (_, kind, span) in found {
        if kept.iter().all(|(_, k)| span.end <= k.start || span.start >= k.end) {
            kept.push((kind, span));
        }
    }
    kept.sort_by_key(|(_, s)| s.start);
    kept.truncate(opts.limit);
    to_entities(text, kept)
}

/// 字节区间 → UTF-16 偏移（片段已按位置排序且互不重叠，一次线性扫描）
fn to_entities(text: &str, kept: Vec<(EntityKind, Span)>) -> Vec<Entity> {
    let (mut byte, mut u16) = (0usize, 0usize);
    let mut advance = |to: usize| {
        u16 += text[byte..to].encode_utf16().count();
        byte = to;
        u32::try_from(u16).unwrap_or(u32::MAX)
    };
    kept.into_iter()
        .map(|(kind, s)| {
            let start = advance(s.start);
            let end = advance(s.end);
            Entity {
                kind,
                value: s.value,
                start,
                end,
                sensitive: kind.is_sensitive(),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 共用

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("静态正则")
}

fn byte_at(text: &str, i: usize) -> Option<u8> {
    text.as_bytes().get(i).copied()
}

fn byte_before(text: &str, i: usize) -> Option<u8> {
    i.checked_sub(1).and_then(|j| byte_at(text, j))
}

/// 片段前后不能紧挨着数字（避免从更长的数字串里截出一段）
fn digit_isolated(text: &str, s: usize, e: usize) -> bool {
    !byte_before(text, s).is_some_and(|c| c.is_ascii_digit()) && !byte_at(text, e).is_some_and(|c| c.is_ascii_digit())
}

fn digits_of(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

/// 关键词之后（跳过空白与「: ：# ＃ 为 是 is」这类连接词）紧跟的 token
fn token_after(text: &str, kw_end: usize, token: &Regex) -> Option<(usize, usize)> {
    static GLUE: LazyLock<Regex> = LazyLock::new(|| re(r"^(?:[\s:：#＃\-]|为|是|號|号|(?i:is|no\.?|number)(?-u:\b))*"));
    let skip = GLUE.find(&text[kw_end..]).map_or(0, |m| m.end());
    let from = kw_end + skip;
    token
        .find(&text[from..])
        .filter(|m| m.start() == 0)
        .map(|m| (from + m.start(), from + m.end()))
}

// ---------------------------------------------------------------------------
// 联系方式与链接

struct Email;

impl Recognizer for Email {
    fn kind(&self) -> EntityKind {
        EntityKind::Email
    }
    fn priority(&self) -> u8 {
        60
    }
    fn scan(&self, text: &str, out: &mut Vec<Span>) {
        static RE: LazyLock<Regex> = LazyLock::new(|| {
            re(
                r"(?-u:\b)[A-Za-z0-9][A-Za-z0-9._%+\-]*@[A-Za-z0-9](?:[A-Za-z0-9\-]*[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9\-]*[A-Za-z0-9])?)*\.[A-Za-z]{2,24}(?-u:\b)",
            )
        });
        for m in RE.find_iter(text) {
            let local = m.as_str().split('@').next().unwrap_or_default();
            if local.ends_with('.') || local.contains("..") {
                continue;
            }
            out.push(Span {
                start: m.start(),
                end: m.end(),
                value: m.as_str().to_owned(),
            });
        }
    }
}

pub(crate) struct Url;

impl Url {
    /// 文本里的链接（`scheme://` 或 `www.` 开头；去掉句末标点与不成对的右括号），按出现顺序逐个给出。
    /// 「去掉跟踪参数」变换也用它找链接（判断是否适用时找到第一个就停）
    pub(crate) fn iter(text: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
        static RE: LazyLock<Regex> =
            LazyLock::new(|| re(r"(?i)(?-u:\b)(?:(?:https?|ftp)://|www\.)[A-Za-z0-9\-._~:/?#\[\]@!$&'()*+,;=%]+"));
        RE.find_iter(text).filter_map(|m| {
            let mut end = m.end();
            loop {
                let t = &text[m.start()..end];
                let Some(last) = t.chars().last() else { break };
                let unbalanced =
                    |open: char, close: char| last == close && t.matches(open).count() < t.matches(close).count();
                if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '*')
                    || unbalanced('(', ')')
                    || unbalanced('[', ']')
                {
                    end -= last.len_utf8();
                } else {
                    break;
                }
            }
            let t = &text[m.start()..end];
            // 至少要有主机名：`https://` 本身、`www.` 后面什么都没有的不算
            let host = t.split_once("://").map_or(t, |(_, rest)| rest);
            let host = host.split(['/', '?', '#']).next().unwrap_or_default();
            (host.len() >= 3 && host.trim_start_matches("www.").len() > 1).then_some((m.start(), end))
        })
    }
}

impl Recognizer for Url {
    fn kind(&self) -> EntityKind {
        EntityKind::Url
    }
    fn priority(&self) -> u8 {
        90
    }
    fn scan(&self, text: &str, out: &mut Vec<Span>) {
        for (start, end) in Self::iter(text) {
            out.push(Span {
                start,
                end,
                value: text[start..end].to_owned(),
            });
        }
    }
}

struct Phone;

impl Recognizer for Phone {
    fn kind(&self) -> EntityKind {
        EntityKind::Phone
    }
    fn priority(&self) -> u8 {
        40
    }
    fn scan(&self, text: &str, out: &mut Vec<Span>) {
        // 中国大陆手机号（可带 +86 / 86，3-4-4 分组）| 带区号的固话（区号与号码之间要有分隔或括号，否则与长编号难以区分）|
        // 国际号码（+ 开头，8–15 位数字，允许空格 / 连字符 / 括号分组）
        static RE: LazyLock<Regex> = LazyLock::new(|| {
            re(concat!(
                r"(?:\+?86[ \-]?)?1[3-9][0-9](?:[ \-]?[0-9]{4}){2}",
                r"|\(0[0-9]{2,3}\)[ \-]?[0-9]{7,8}|0[0-9]{2,3}[ \-][0-9]{7,8}",
                r"|\+[1-9][0-9 \-()]{6,20}[0-9]",
            ))
        });
        for m in RE.find_iter(text) {
            let (s, e) = (m.start(), m.end());
            let t = m.as_str();
            if !digit_isolated(text, s, e) || byte_before(text, s).is_some_and(|c| c == b'+' || c.is_ascii_alphabetic())
            {
                continue;
            }
            if byte_at(text, e).is_some_and(|c| c.is_ascii_alphabetic()) {
                continue;
            }
            let digits = digits_of(t);
            if t.starts_with('+') && !(8..=15).contains(&digits.len()) {
                continue;
            }
            // 分隔符不能连续出现（「+1  -- 2」这类不是号码）
            if t.contains("  ") || t.contains("--") || t.contains(" -") || t.contains("- ") {
                continue;
            }
            let value = if t.starts_with('+') {
                format!("+{digits}")
            } else {
                digits
            };
            out.push(Span {
                start: s,
                end: e,
                value,
            });
        }
    }
}

struct Ip;

impl Recognizer for Ip {
    fn kind(&self) -> EntityKind {
        EntityKind::Ip
    }
    fn priority(&self) -> u8 {
        50
    }
    fn scan(&self, text: &str, out: &mut Vec<Span>) {
        static RE: LazyLock<Regex> = LazyLock::new(|| {
            re(
                r"(?-u:\b)(?:25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])(?:\.(?:25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])){3}(?::[0-9]{1,5})?(?-u:\b)",
            )
        });
        for m in RE.find_iter(text) {
            let (s, e) = (m.start(), m.end());
            // 更长的点分数字串（版本号「1.2.3.4.5」）里的一段不算
            let prev_dot = byte_before(text, s) == Some(b'.');
            let next_dot_digit =
                byte_at(text, e) == Some(b'.') && byte_at(text, e + 1).is_some_and(|c| c.is_ascii_digit());
            if prev_dot || next_dot_digit {
                continue;
            }
            if let Some((_, port)) = m.as_str().split_once(':')
                && port.parse::<u32>().map_or(true, |p| p == 0 || p > 65_535)
            {
                continue;
            }
            out.push(Span {
                start: s,
                end: e,
                value: m.as_str().to_owned(),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// 验证码

struct Otp;

impl Recognizer for Otp {
    fn kind(&self) -> EntityKind {
        EntityKind::Otp
    }
    fn priority(&self) -> u8 {
        80
    }
    fn scan(&self, text: &str, out: &mut Vec<Span>) {
        out.extend(otp::find(text));
    }
}

// ---------------------------------------------------------------------------
// 单号与数字

struct Tracking;

impl Tracking {
    /// UPU S10 国际邮件编号（`EA123456785CN`）的校验位
    fn s10_valid(t: &str) -> bool {
        let d: Vec<u32> = t[2..11].chars().filter_map(|c| c.to_digit(10)).collect();
        if d.len() != 9 {
            return false;
        }
        const W: [u32; 8] = [8, 6, 4, 2, 3, 5, 9, 7];
        let sum: u32 = d[..8].iter().zip(W).map(|(a, w)| a * w).sum();
        let check = match 11 - sum % 11 {
            10 => 0,
            11 => 5,
            c => c,
        };
        check == d[8]
    }
}

impl Recognizer for Tracking {
    fn kind(&self) -> EntityKind {
        EntityKind::Tracking
    }
    fn priority(&self) -> u8 {
        45
    }
    fn scan(&self, text: &str, out: &mut Vec<Span>) {
        // 格式明确的：顺丰、京东、圆通、极兔、菜鸟、UPS、UPU S10（EMS 等国际邮件，带校验位）
        static FORMAT: LazyLock<Regex> = LazyLock::new(|| {
            re(
                r"(?-u:\b)(?:SF[0-9]{12,13}|JD[0-9A-Z]{13,16}|YT[0-9]{13}|JT[0-9]{13}|LP[0-9]{14}|1Z[0-9A-Z]{16}|[A-Z]{2}[0-9]{9}[A-Z]{2})(?-u:\b)",
            )
        });
        // 纯数字的单号（中通、韵达、申通、FedEx、USPS…）只在「快递 / 运单 / tracking」语境里算
        static CONTEXT: LazyLock<Regex> = LazyLock::new(|| {
            re(
                r"(?i)快递单号|运单号|運單號|物流单号|快递|快遞|运单|物流|(?-u:\b)(?:tracking(?:\s*(?:number|no\.?|#|id))?|waybill|awb)(?-u:\b)",
            )
        });
        static DIGITS: LazyLock<Regex> = LazyLock::new(|| re(r"^[0-9]{10,22}(?-u:\b)"));
        for m in FORMAT.find_iter(text) {
            let t = m.as_str();
            let s10 = t.len() == 13
                && t.as_bytes()[..2].iter().all(u8::is_ascii_uppercase)
                && t.as_bytes()[11..].iter().all(u8::is_ascii_uppercase);
            if s10 && !Self::s10_valid(t) {
                continue;
            }
            out.push(Span {
                start: m.start(),
                end: m.end(),
                value: t.to_owned(),
            });
        }
        for kw in CONTEXT.find_iter(text) {
            if let Some((s, e)) = token_after(text, kw.end(), &DIGITS)
                && digit_isolated(text, s, e)
            {
                out.push(Span {
                    start: s,
                    end: e,
                    value: text[s..e].to_owned(),
                });
            }
        }
    }
}

struct Order;

impl Recognizer for Order {
    fn kind(&self) -> EntityKind {
        EntityKind::Order
    }
    fn priority(&self) -> u8 {
        35
    }
    fn scan(&self, text: &str, out: &mut Vec<Span>) {
        static CONTEXT: LazyLock<Regex> = LazyLock::new(|| {
            re(
                r"(?i)订单编号|订单号|訂單編號|訂單號|订单|訂單|单号|單號|(?-u:\b)order(?:\s*(?:number|no\.?|id|#))?(?-u:\b)",
            )
        });
        static TOKEN: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Za-z0-9][A-Za-z0-9\-_]{5,39}(?-u:\b)"));
        for kw in CONTEXT.find_iter(text) {
            let Some((s, e)) = token_after(text, kw.end(), &TOKEN) else {
                continue;
            };
            let t = &text[s..e];
            // 至少 6 位数字：排除「订单 已发货」后面跟的普通单词
            if t.bytes().filter(u8::is_ascii_digit).count() >= 6 {
                out.push(Span {
                    start: s,
                    end: e,
                    value: t.to_owned(),
                });
            }
        }
    }
}

struct Amount;

impl Recognizer for Amount {
    fn kind(&self) -> EntityKind {
        EntityKind::Amount
    }
    fn priority(&self) -> u8 {
        30
    }
    fn scan(&self, text: &str, out: &mut Vec<Span>) {
        static RE: LazyLock<Regex> = LazyLock::new(|| {
            re(concat!(
                r"(?:[¥￥$€£]|(?i:(?-u:\b)(?:USD|CNY|RMB|EUR|GBP|JPY|HKD)))\s?-?(?:[0-9]{1,3}(?:,[0-9]{3})+|[0-9]+)(?:\.[0-9]{1,2})?",
                r"|-?(?:[0-9]{1,3}(?:,[0-9]{3})+|[0-9]+)(?:\.[0-9]{1,2})?\s?(?:元|块|美元|欧元|英镑|日元|港元|港币|人民币)",
            ))
        });
        for m in RE.find_iter(text) {
            let (s, e) = (m.start(), m.end());
            if !digit_isolated(text, s, e)
                || (byte_at(text, e) == Some(b'.') && byte_at(text, e + 1).is_some_and(|c| c.is_ascii_digit()))
                || shell_positional(m.as_str())
            {
                continue;
            }
            let value: String = m
                .as_str()
                .chars()
                .filter(|c| c.is_ascii_digit() || matches!(c, '.' | '-'))
                .collect();
            out.push(Span {
                start: s,
                end: e,
                value,
            });
        }
    }
}

/// shell / awk / 正则替换里的位置参数（`$1`、`{print $2}`）：`$` 紧跟一位整数。一位数的美元金额很少单独复制，
/// 宁可漏掉也不把命令行认成金额
fn shell_positional(m: &str) -> bool {
    m.strip_prefix('$')
        .is_some_and(|n| n.len() == 1 && n.as_bytes()[0].is_ascii_digit())
}

struct Datetime;

impl Datetime {
    fn valid(y: Option<u32>, mo: u32, d: u32, h: Option<u32>, mi: Option<u32>) -> bool {
        y.is_none_or(|y| (1900..=2099).contains(&y))
            && (1..=12).contains(&mo)
            && (1..=31).contains(&d)
            && h.is_none_or(|h| h < 24)
            && mi.is_none_or(|m| m < 60)
    }
}

impl Recognizer for Datetime {
    fn kind(&self) -> EntityKind {
        EntityKind::Datetime
    }
    fn priority(&self) -> u8 {
        55
    }
    fn scan(&self, text: &str, out: &mut Vec<Span>) {
        // ISO（可带时间与时区）/ 斜杠（可带时间）/ 点分日期与中文日期；三段的分隔符一致。命名组：年 月 日 时 分
        static PATTERNS: LazyLock<[Regex; 4]> = LazyLock::new(|| {
            [
                re(
                    r"(?P<y>[0-9]{4})-(?P<mo>[0-9]{1,2})-(?P<d>[0-9]{1,2})(?:[T ](?P<h>[0-9]{1,2}):(?P<mi>[0-9]{2})(?::[0-9]{2}(?:\.[0-9]{1,3})?)?(?:Z|[+\-][0-9]{2}:?[0-9]{2})?)?",
                ),
                re(
                    r"(?P<y>[0-9]{4})/(?P<mo>[0-9]{1,2})/(?P<d>[0-9]{1,2})(?: (?P<h>[0-9]{1,2}):(?P<mi>[0-9]{2})(?::[0-9]{2})?)?",
                ),
                re(r"(?P<y>[0-9]{4})\.(?P<mo>[0-9]{1,2})\.(?P<d>[0-9]{1,2})"),
                re(
                    r"(?:(?P<y>[0-9]{4}) ?年 ?)?(?P<mo>[0-9]{1,2}) ?月 ?(?P<d>[0-9]{1,2}) ?[日号號](?: ?(?P<h>[0-9]{1,2})[:：](?P<mi>[0-9]{2})(?:[:：][0-9]{2})?)?",
                ),
            ]
        });
        for pattern in PATTERNS.iter() {
            for c in pattern.captures_iter(text) {
                let Some(all) = c.get(0) else { continue };
                let num = |name: &str| c.name(name).and_then(|m| m.as_str().parse::<u32>().ok());
                let ok = Self::valid(
                    num("y"),
                    num("mo").unwrap_or(0),
                    num("d").unwrap_or(0),
                    num("h"),
                    num("mi"),
                );
                // 点分日期后面还跟着「.数字」的是版本号或 IP 的一部分
                let dotted_tail = byte_at(text, all.end()) == Some(b'.')
                    && byte_at(text, all.end() + 1).is_some_and(|c| c.is_ascii_digit());
                if ok && !dotted_tail && digit_isolated(text, all.start(), all.end()) {
                    out.push(Span {
                        start: all.start(),
                        end: all.end(),
                        value: all.as_str().to_owned(),
                    });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 证件与银行卡（敏感，默认关闭）

struct IdCard;

impl IdCard {
    /// GB 11643 校验位
    fn valid(id: &str) -> bool {
        const W: [u32; 17] = [7, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];
        const CHECK: &[u8; 11] = b"10X98765432";
        let b = id.as_bytes();
        if b.len() != 18 {
            return false;
        }
        let mut sum = 0;
        for (c, w) in b[..17].iter().zip(W) {
            let Some(d) = char::from(*c).to_digit(10) else {
                return false;
            };
            sum += d * w;
        }
        CHECK[(sum % 11) as usize] == b[17].to_ascii_uppercase()
    }
}

impl Recognizer for IdCard {
    fn kind(&self) -> EntityKind {
        EntityKind::IdCard
    }
    fn priority(&self) -> u8 {
        75
    }
    fn scan(&self, text: &str, out: &mut Vec<Span>) {
        static RE: LazyLock<Regex> = LazyLock::new(|| {
            re(
                r"(?-u:\b)[1-9][0-9]{5}(?:19|20)[0-9]{2}(?:0[1-9]|1[0-2])(?:0[1-9]|[12][0-9]|3[01])[0-9]{3}[0-9Xx](?-u:\b)",
            )
        });
        for m in RE.find_iter(text) {
            if Self::valid(m.as_str()) {
                out.push(Span {
                    start: m.start(),
                    end: m.end(),
                    value: m.as_str().to_ascii_uppercase(),
                });
            }
        }
    }
}

struct BankCard;

impl BankCard {
    fn luhn(digits: &str) -> bool {
        let mut sum = 0;
        for (i, c) in digits.bytes().rev().enumerate() {
            let mut d = u32::from(c - b'0');
            if i % 2 == 1 {
                d *= 2;
                if d > 9 {
                    d -= 9;
                }
            }
            sum += d;
        }
        sum % 10 == 0
    }

    /// 主流卡组织的号段（Visa 4、万事达 51–55 / 2221–2720、美国运通 34 / 37、JCB 35、Discover 6011 / 65、银联 62 / 81）。
    /// 以日期开头的长编号（「20251015…」）因此不会被当成卡号
    fn known_iin(d: &str) -> bool {
        let p2: u32 = d[..2].parse().unwrap_or(0);
        let p4: u32 = d[..4].parse().unwrap_or(0);
        d.starts_with('4')
            || (51..=55).contains(&p2)
            || (2221..=2720).contains(&p4)
            || matches!(p2, 34 | 35 | 37 | 62 | 65 | 81)
            || p4 == 6011
    }
}

impl Recognizer for BankCard {
    fn kind(&self) -> EntityKind {
        EntityKind::BankCard
    }
    fn priority(&self) -> u8 {
        65
    }
    fn scan(&self, text: &str, out: &mut Vec<Span>) {
        static RE: LazyLock<Regex> = LazyLock::new(|| {
            re(
                r"(?-u:\b)(?:[0-9]{4}(?:[ \-][0-9]{4}){3}(?:[ \-]?[0-9]{1,3})?|[0-9]{4}[ \-][0-9]{6}[ \-][0-9]{5}|[0-9]{15,19})(?-u:\b)",
            )
        });
        for m in RE.find_iter(text) {
            let digits = digits_of(m.as_str());
            if (15..=19).contains(&digits.len()) && Self::known_iin(&digits) && Self::luhn(&digits) {
                out.push(Span {
                    start: m.start(),
                    end: m.end(),
                    value: digits,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn all_on() -> RecognizeOpts {
        RecognizeOpts {
            smart: SmartSettings {
                contacts: true,
                codes: true,
                numbers: true,
                identity: true,
                otp_auto_delete: true,
            },
            limit: MAX_ENTITIES,
        }
    }

    /// 只看一种识别器的结果：（原文片段，值）
    fn scan(r: &dyn Recognizer, text: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        r.scan(text, &mut out);
        out.into_iter()
            .map(|s| (text[s.start..s.end].to_owned(), s.value))
            .collect()
    }

    /// 正例：每条都应识别出给定的值；反例：什么都不识别
    fn table(r: &dyn Recognizer, yes: &[(&str, &str)], no: &[&str]) {
        for (text, want) in yes {
            let got: Vec<String> = scan(r, text).into_iter().map(|(_, v)| v).collect();
            assert!(
                got.iter().any(|v| v == want),
                "{:?} 应识别 {text:?} → {want:?}，实际 {got:?}",
                r.kind()
            );
        }
        for text in no {
            assert_eq!(
                scan(r, text),
                Vec::<(String, String)>::new(),
                "{:?} 不应识别 {text:?}",
                r.kind()
            );
        }
    }

    #[test]
    fn registry_covers_every_kind_once() {
        let kinds: Vec<EntityKind> = RECOGNIZERS.iter().map(|r| r.kind()).collect();
        assert_eq!(kinds.len(), EntityKind::ALL.len());
        let set: HashSet<_> = kinds.iter().collect();
        assert_eq!(set.len(), kinds.len(), "每个种类只能有一个识别器");
        for k in EntityKind::ALL {
            assert!(set.contains(&k), "{k:?} 没有识别器");
        }
    }

    #[test]
    fn email() {
        table(
            &Email,
            &[
                (
                    "联系 someone.name+tag@mail.example.com 获取",
                    "someone.name+tag@mail.example.com",
                ),
                ("Email:a@b.io.", "a@b.io"),
            ],
            &[
                "not an email: a@b",
                "a..b@example.com",
                "user.@example.com",
                "@example.com",
            ],
        );
    }

    #[test]
    fn url() {
        table(
            &Url,
            &[
                ("访问https://example.com/a?b=1了解更多", "https://example.com/a?b=1"),
                (
                    "See (https://en.wikipedia.org/wiki/Rust_(language)).",
                    "https://en.wikipedia.org/wiki/Rust_(language)",
                ),
                ("打开 www.example.cn，然后", "www.example.cn"),
                ("ftp://files.example.org/pub/", "ftp://files.example.org/pub/"),
            ],
            &["https://", "www.", "plain text"],
        );
    }

    #[test]
    fn phone() {
        table(
            &Phone,
            &[
                ("电话13800138000", "13800138000"),
                ("手机：138 0013 8000。", "13800138000"),
                ("+86 138-0013-8000", "+8613800138000"),
                ("座机 010-12345678", "01012345678"),
                ("(0755)12345678", "075512345678"),
                ("Call +1 (415) 555-0132 now", "+14155550132"),
                ("+44 20 7946 0958", "+442079460958"),
            ],
            &[
                "订单 1380013800012345",
                "ID13800138000",
                "12800138000",
                "+12 3",
                "010012345678",
            ],
        );
    }

    #[test]
    fn ip() {
        table(
            &Ip,
            &[
                ("ssh root@203.0.113.7 -p 22", "203.0.113.7"),
                ("服务器 192.168.1.100:8080 已就绪", "192.168.1.100:8080"),
                ("dns 1.1.1.1", "1.1.1.1"),
            ],
            &[
                "version 1.2.3.4.5",
                "256.1.1.1",
                "10.0.0.256",
                "1.2.3",
                "host 10.0.0.1:99999",
            ],
        );
    }

    #[test]
    fn tracking() {
        table(
            &Tracking,
            &[
                ("顺丰 SF1234567890123 已揽收", "SF1234567890123"),
                ("JDVA12345678901 派送中", "JDVA12345678901"),
                ("EMS EA123456785CN", "EA123456785CN"),
                ("UPS 1Z999AA10123456784", "1Z999AA10123456784"),
                ("快递单号：773012345678901", "773012345678901"),
                ("Tracking number: 9400111899223397622235", "9400111899223397622235"),
            ],
            &[
                // S10 校验位不对
                "EA123456789CN",
                // 没有快递语境的纯数字
                "773012345678901",
                "快递 已签收",
            ],
        );
    }

    #[test]
    fn order() {
        table(
            &Order,
            &[
                ("订单号：2025101512345678", "2025101512345678"),
                ("您的订单 E20251015-0042 已发货", "E20251015-0042"),
                ("Order #112-3456789-1234567 shipped", "112-3456789-1234567"),
                ("Order number: A10293847", "A10293847"),
            ],
            &["订单已发货", "order shipped today", "订单号：12345"],
        );
    }

    #[test]
    fn amount() {
        table(
            &Amount,
            &[
                ("合计 ¥1,234.50", "1234.50"),
                ("付款 $ 99.99 成功", "99.99"),
                ("退款 -12.5元", "-12.5"),
                ("共 300 元", "300"),
                ("USD 1,000", "1000"),
                ("余额￥88", "88"),
            ],
            &[
                "版本 1.2",
                "共 300 个",
                "100%",
                // shell / awk 的位置参数
                "awk '{print $1, $2}'",
                "echo \"$1\"",
                "sed 's/a/$1/'",
            ],
        );
    }

    #[test]
    fn datetime() {
        table(
            &Datetime,
            &[
                ("会议 2025-10-15 14:30 开始", "2025-10-15 14:30"),
                ("deadline 2025-10-15T09:00:00Z", "2025-10-15T09:00:00Z"),
                ("2025/1/5", "2025/1/5"),
                ("2025.10.15", "2025.10.15"),
                ("2025年10月15日 9:30", "2025年10月15日 9:30"),
                ("10月1日放假", "10月1日"),
            ],
            &[
                "2025-13-01",
                "2025-10-32",
                "2025-10/15",
                "1.2.3",
                "3025-01-01",
                "99月1日",
            ],
        );
    }

    #[test]
    fn id_card_checksum() {
        table(
            &IdCard,
            &[
                ("身份证 11010519491231002X", "11010519491231002X"),
                ("id 11010519491231002x", "11010519491231002X"),
            ],
            // 校验位错误；出生日期不合法
            &["11010519491231002Y", "110105194912310021", "110105194913310021"],
        );
    }

    #[test]
    fn bank_card_luhn() {
        table(
            &BankCard,
            &[
                ("卡号 6222 0202 0011 2233 446", "6222020200112233446"),
                ("Visa 4111-1111-1111-1111", "4111111111111111"),
                ("Amex 3782 822463 10005", "378282246310005"),
                ("5555555555554444", "5555555555554444"),
            ],
            // Luhn 不通过；号段未知（以日期开头的长编号）
            &["4111 1111 1111 1112", "2025101512345678"],
        );
    }

    #[test]
    fn overlaps_resolve_by_priority_then_length() {
        // 链接里的 IP、日期、邮箱都并入链接
        let es = recognize("打开 http://user@192.168.1.1/2025-10-15 看看", &all_on());
        assert_eq!(es.iter().map(|e| e.kind).collect::<Vec<_>>(), [EntityKind::Url]);
        // 身份证号同时满足 Luhn 时也只算身份证
        let es = recognize("证件 11010519491231002X", &all_on());
        assert_eq!(es.iter().map(|e| e.kind).collect::<Vec<_>>(), [EntityKind::IdCard]);
        // 快递语境的单号不再作为订单号重复出现
        let es = recognize("快递单号 773012345678901", &all_on());
        assert_eq!(es.iter().map(|e| e.kind).collect::<Vec<_>>(), [EntityKind::Tracking]);
    }

    #[test]
    fn disabled_groups_are_skipped() {
        let text = "验证码 482915，卡号 4111 1111 1111 1111，联系 a@b.io";
        let kinds = |o: &RecognizeOpts| recognize(text, o).iter().map(|e| e.kind).collect::<Vec<_>>();
        assert_eq!(
            kinds(&all_on()),
            [EntityKind::Otp, EntityKind::BankCard, EntityKind::Email]
        );
        // 默认不识别证件与银行卡
        assert_eq!(kinds(&RecognizeOpts::default()), [EntityKind::Otp, EntityKind::Email]);
        let mut o = all_on();
        o.smart.codes = false;
        o.smart.contacts = false;
        assert_eq!(kinds(&o), [EntityKind::BankCard]);
        let bank = &recognize(text, &all_on())[1];
        assert!(bank.sensitive);
    }

    #[test]
    fn offsets_are_utf16_in_mixed_text() {
        // 「😀」占两个 UTF-16 单元，中文各占一个
        let text = "😀 邮箱：a@b.io，电话 138 0013 8000";
        let es = recognize(text, &all_on());
        assert_eq!(es.len(), 2);
        assert_eq!((es[0].kind, es[0].start, es[0].end), (EntityKind::Email, 6, 12));
        let u16: Vec<u16> = text.encode_utf16().collect();
        let phone = String::from_utf16(&u16[es[1].start as usize..es[1].end as usize]).expect("utf16");
        assert_eq!((phone.as_str(), es[1].value.as_str()), ("138 0013 8000", "13800138000"));
    }

    #[test]
    fn scan_is_capped() {
        let many = (0..100)
            .map(|i| format!("u{i}@example.com"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(recognize(&many, &all_on()).len(), MAX_ENTITIES);
        let o = RecognizeOpts { limit: 3, ..all_on() };
        assert_eq!(recognize(&many, &o).len(), 3);
        // 只扫前 16 KB
        let far = format!("{}a@b.io", "x ".repeat(SCAN_BYTES));
        assert!(recognize(&far, &all_on()).is_empty());
    }
}
