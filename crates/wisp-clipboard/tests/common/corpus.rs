//! 确定性的剪贴板语料（测试与基准共用，`benches/search.rs` 经 `#[path]` 引入）：中英文混合的句子、段落、
//! 代码、链接，长度分布贴近日常剪贴板（大部分短、少量长），部分条目带富文本 HTML。

/// 常用汉字（约 500 个）
const HANZI: &str = "的一是在不了有和人这中大为上个国我以要他时来用们生到作地于出就分对成会可主发年动同工也能下过子说产种面而方后多定行学法所民得经十三之进着等部度家电力里如水化高自二理起小物现实加量都两体制机当使点从业本去把性好应开它合还因由其些然前外天政四日那社义事平形相全表间样与关各重新线内数正心反你明看原又么利比或但质气第向道命此变条只没结解问意建月公无系军很情者最立代想已通并提直题党程展五果料象员革位入常文总次品式活设及管特件长求老头基资边流路级少图山统接知较将组见计别她手角期根论运农指几九区强放决西被干做必战先回则任取据处府研队南给色光门即保治北造百规热领七海口东导器压志世金增争济阶油思术极交受联什认六共权收证改清己美再采转更单风切打白教速花带安场身车例真务具万每目至达走积示议声报斗完类八离华名确才科张信马节话米整空元况今集温传土许步群广石记需段研界拉林律叫且究观越织装影算低持音众书布复容儿须际商非验连断深难近矿千周委素技备半办青省列习响约支般史感劳便团往酸历市克何除消构府称太准精值号率族维划选标写存候毛亲快效斯院查江型眼王按格养易置派层片始却专状育厂京识适属圆包火住调满县局照参红细引听该铁价严";

/// 常用英文单词（空白分隔）
const WORDS: &str = "the quick brown fox jumps over lazy dog meeting schedule project deadline review please update the \
     calendar and send notes before Friday budget report design system panel window clipboard history \
     search result translate screen capture release version feature request issue fixed build pipeline \
     deploy server client database index query performance memory latency frame animation spring motion \
     settings account password invoice payment customer support ticket priority team weekly summary \
     action items follow up with about from into after while because however therefore although really \
     great thanks hello regards best today tomorrow yesterday morning evening travel flight hotel booking \
     address street city phone number email document draft final version table chart data model prompt \
     token stream cache state event handler";

const CODE_LINES: [&str; 16] = [
    "fn main() {",
    "    let mut cache = RecentCache::load(&store)?;",
    "    for item in items.iter().filter(|i| i.pinned) {",
    "        println!(\"{}\", item.preview);",
    "    }",
    "}",
    "const handler = async (event) => {",
    "  await store.search(query, { limit: 50 });",
    "};",
    "SELECT id, text FROM items WHERE pinned = 1 ORDER BY last_used_at DESC;",
    "def parse(line):",
    "    return [x.strip() for x in line.split(',')]",
    "import { motion } from 'motion/react'",
    "export function useClipboard() { return useStore((s) => s.items) }",
    "if (value === undefined) { throw new Error('missing value') }",
    "    .then((res) => res.json())",
];

/// 简单的确定性伪随机数（xorshift64*）
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    pub fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }
}

fn english(rng: &mut Rng, chars: usize) -> String {
    let words: Vec<&str> = WORDS.split_whitespace().collect();
    let mut s = String::with_capacity(chars + 16);
    let mut since_period = 0;
    while s.len() < chars {
        let w = words[rng.below(words.len())];
        if since_period == 0 {
            let mut c = w.chars();
            if let Some(first) = c.next() {
                s.extend(first.to_uppercase());
                s.push_str(c.as_str());
            }
        } else {
            s.push_str(w);
        }
        since_period += 1;
        if since_period > rng.range(6, 14) {
            s.push_str(". ");
            since_period = 0;
        } else {
            s.push(' ');
        }
    }
    s.trim_end().to_owned()
}

fn chinese(rng: &mut Rng, chars: usize) -> String {
    let pool: Vec<char> = HANZI.chars().collect();
    let mut s = String::with_capacity(chars * 3);
    for i in 0..chars {
        s.push(pool[rng.below(pool.len())]);
        if i % rng.range(12, 30) == 11 {
            s.push(if rng.below(3) == 0 { '。' } else { '，' });
        }
    }
    s
}

fn code(rng: &mut Rng, lines: usize) -> String {
    (0..lines)
        .map(|_| CODE_LINES[rng.below(CODE_LINES.len())])
        .collect::<Vec<_>>()
        .join("\n")
}

/// 第 i 条语料：（纯文本，可选 HTML）
pub fn clip_text(i: u64) -> (String, Option<String>) {
    let mut rng = Rng::new(i + 1);
    let roll = rng.below(100);
    // 长度分布：55% 短、35% 中、10% 长
    let len = match roll {
        0..55 => rng.range(20, 200),
        55..90 => rng.range(200, 2000),
        _ => rng.range(2000, 12_000),
    };
    let body = match rng.below(10) {
        0..4 => english(&mut rng, len),
        4..7 => chinese(&mut rng, len / 2 + 1),
        7..9 => format!("{} {}", english(&mut rng, len / 2), chinese(&mut rng, len / 4 + 1)),
        _ => code(&mut rng, len / 40 + 2),
    };
    let text = format!("{body} #{i}");
    // 15% 带 HTML（网页 / 文档里复制的富文本，标记约是正文的 2–3 倍）
    let html = (rng.below(100) < 15).then(|| {
        let mut h =
            String::from("<div style=\"font-family: -apple-system; font-size: 14px; color: rgb(29, 29, 31);\">");
        for para in text.split(". ") {
            h.push_str("<p style=\"margin: 0 0 8px 0; line-height: 1.5;\"><span class=\"s1\">");
            h.push_str(para);
            h.push_str("</span></p>");
        }
        h.push_str("</div>");
        h
    });
    (text, html)
}
