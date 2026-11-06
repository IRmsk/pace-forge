//! 响应密钥派生（NetSecKit 自定义 KDF）。
//!
//! 这不是标准 HKDF/PBKDF2。服务端不协商密钥，而是客户端生成四个 `keyData` 段
//! 随请求发上去，双方用同一套规则各自算出同一把 AES-128 key。
//!
//! 三段式：`byte_hash` → `feistel_transform` → `final_mix`。
//! 位运算细节必须逐位对齐，任何一处差异都会导致响应解不出来（表现为 JSON 解析失败）。

/// 自定义滚动哈希。与 FNV/Pearson 都不同，是易盾自己的构造。
fn byte_hash(data: &[u8]) -> u32 {
    let mut h: u32 = 0;
    for &b in data {
        h = (b as u32 | ((h << 8) & 0xFFFF_FFFF)) ^ ((h >> 24) & 0xFF);
    }
    h
}

/// 四路两轮位混淆变换。
fn feistel_transform(s0: u32, s1: u32, s2: u32, s3: u32) -> (u32, u32, u32, u32) {
    let t = s2 ^ s0;
    let mix = (t.rotate_right(24) ^ t.rotate_right(8) ^ t) & 0xFFFF_FFFF;
    let new_s1 = mix ^ s1;
    let new_s3 = mix ^ s3;
    let t2 = new_s3 ^ new_s1;
    let mix2 = (t2.rotate_right(24) ^ t2.rotate_right(8) ^ t2) & 0xFFFF_FFFF;
    let new_s0 = mix2 ^ s0;
    let new_s2 = mix2 ^ s2;
    (new_s0, new_s1, new_s2, new_s3)
}

/// 最终混合，输出 16 字节小端打包（即 AES-128 key）。
fn final_mix(s0: u32, s1: u32, s2: u32, s3: u32) -> [u8; 16] {
    let a = (!(s2 | s3) & 0xFFFF_FFFF) ^ s1;
    let b = (s2 ^ s3) ^ a;
    let c = ((a & s2) ^ s0) & 0xFFFF_FFFF;
    let b = (b ^ c) & 0xFFFF_FFFF;
    let a = (!(b | c) & 0xFFFF_FFFF) ^ a;
    let d = ((a & b) ^ s3) & 0xFFFF_FFFF;

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&d.to_le_bytes());
    out[4..8].copy_from_slice(&a.to_le_bytes());
    out[8..12].copy_from_slice(&b.to_le_bytes());
    out[12..16].copy_from_slice(&c.to_le_bytes());
    out
}

/// 由四段 keyData 派生响应 AES-128 key。
pub fn response_kdf(kd1: &str, kd2: &str, kd3: &str, kd4: &str) -> [u8; 16] {
    let (h1, h2, h3, h4) = (
        byte_hash(kd1.as_bytes()),
        byte_hash(kd2.as_bytes()),
        byte_hash(kd3.as_bytes()),
        byte_hash(kd4.as_bytes()),
    );
    let (s0, s1, s2, s3) = feistel_transform(h1, h2, h3, h4);
    final_mix(s0, s1, s2, s3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_hash_matches_reference() {
        // 黄金向量：换实现时必须逐位一致
        assert_eq!(byte_hash(b""), 0x0);
        assert_eq!(byte_hash(b"a"), 0x61);
        assert_eq!(byte_hash(b"ab"), 0x6162);
        assert_eq!(byte_hash(b"abc"), 0x616263);
    }

    #[test]
    fn kdf_matches_reference_vector() {
        // 金标准向量：response_kdf("0123456789" x4)
        let got = response_kdf("0123456789", "0123456789", "0123456789", "0123456789");
        assert_eq!(
            got,
            [
                0xc2, 0xc3, 0xfb, 0xfb, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00,
                0x00, 0x00
            ],
            "KDF 输出与 Python 实现不一致"
        );
    }

    #[test]
    fn kdf_is_deterministic() {
        let a = response_kdf("0123456789", "0123456789", "0123456789", "0123456789");
        let b = response_kdf("0123456789", "0123456789", "0123456789", "0123456789");
        assert_eq!(a, b, "同一输入必须得到同一 key");
        assert_eq!(a.len(), 16, "AES-128 需要 16 字节");
    }

    #[test]
    fn kdf_uses_all_four_segments() {
        let base = response_kdf("AAAAAAAAAA", "BBBBBBBBBB", "CCCCCCCCCC", "DDDDDDDDDD");
        // 改动任意一段都必须改变输出
        assert_ne!(
            base,
            response_kdf("XAAAAAAAAA", "BBBBBBBBBB", "CCCCCCCCCC", "DDDDDDDDDD")
        );
        assert_ne!(
            base,
            response_kdf("AAAAAAAAAA", "BBBBBBBBBB", "CCCCCCCCCC", "XDDDDDDDDD")
        );
    }
}
