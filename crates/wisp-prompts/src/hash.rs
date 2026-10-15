//! 稳定、无依赖的 FNV-1a 哈希（128 位用于缓存键，64 位用于术语表版本）。
//!
//! 缓存只在内存里，不需要抗碰撞攻击；选择 FNV-1a 是因为结果跨进程、跨编译器版本稳定（便于测试与排查），
//! 且 128 位乘法可以拆成移位加法，8000 字符的原文约 10 µs。

const FNV128_OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
const FNV64_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV64_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a 128 位（prime = 2^88 + 2^8 + 0x3b）
#[derive(Debug, Clone, Copy)]
pub(crate) struct Fnv128(u128);

impl Fnv128 {
    pub fn new() -> Self {
        Self(FNV128_OFFSET)
    }

    pub fn bytes(&mut self, data: &[u8]) {
        let mut h = self.0;
        for &b in data {
            h ^= u128::from(b);
            // h × prime（mod 2^128）= (h << 88) + (h << 8) + h × 0x3b
            h = (h << 88).wrapping_add(h << 8).wrapping_add(h.wrapping_mul(0x3b));
        }
        self.0 = h;
    }

    /// 写入带长度前缀的字段，避免 ("ab","c") 与 ("a","bc") 相同
    pub fn field(&mut self, data: &[u8]) {
        self.bytes(&(data.len() as u64).to_le_bytes());
        self.bytes(data);
    }

    pub fn u8(&mut self, v: u8) {
        self.bytes(&[v]);
    }

    pub fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }

    pub fn finish(self) -> u128 {
        self.0
    }
}

/// FNV-1a 64 位
#[derive(Debug, Clone, Copy)]
pub(crate) struct Fnv64(u64);

impl Fnv64 {
    pub fn new() -> Self {
        Self(FNV64_OFFSET)
    }

    pub fn bytes(&mut self, data: &[u8]) {
        let mut h = self.0;
        for &b in data {
            h ^= u64::from(b);
            h = h.wrapping_mul(FNV64_PRIME);
        }
        self.0 = h;
    }

    pub fn field(&mut self, data: &[u8]) {
        self.bytes(&(data.len() as u64).to_le_bytes());
        self.bytes(data);
    }

    pub fn finish(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv128_known_vectors() {
        // 公开测试向量：FNV-1a 128 of "" / "a"
        assert_eq!(Fnv128::new().finish(), 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d);
        let mut h = Fnv128::new();
        h.bytes(b"a");
        assert_eq!(h.finish(), 0xd228_cb69_6f1a_8caf_7891_2b70_4e4a_8964);
    }

    #[test]
    fn fnv64_known_vectors() {
        let mut h = Fnv64::new();
        h.bytes(b"a");
        assert_eq!(h.finish(), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn length_prefix_separates_fields() {
        let mut a = Fnv128::new();
        a.field(b"ab");
        a.field(b"c");
        let mut b = Fnv128::new();
        b.field(b"a");
        b.field(b"bc");
        assert_ne!(a.finish(), b.finish());
    }
}
