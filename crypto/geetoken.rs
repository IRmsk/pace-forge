//! 本地 geeToken 生成。
//!
//! 极验（GeeTest v4）在真机上会给出一个 `geeToken`，随提交报文一起发给
//! 服务端做「本次请求通过人机校验」的凭据。原实现两处都发空串：
//!
//! ```text
//! api/submit.rs  SignFields → ("geeToken", Some(String::new()))
//! api/submit.rs  body       → "geeToken": ""
//! ```
//!
//! 真机这个字段不是回调拿的，是**本地算的**：用固定 key 做 AES-256-CBC，
//! 明文是设备档案 JSON，外层套一个 `GEE\0` 帧。所以离线也能生成，不需要
//! 真的过一遍滑块。
//!
//! ## 帧格式
//!
//! ```text
//! GEE\0            4B   魔数
//! 0x05             1B   版本
//! iv              16B   ASCII hex 字符
//! len              4B   大端，密文字节数
//! cipher           nB   AES-256-CBC/PKCS7
//! ```
//!
//! 整体再做 base64。
//!
//! ## iv 怎么来的
//!
//! 不是随机 16 字节，而是把 8 字节 nonce 转成小写 hex（正好 16 字符）当 iv
//! 用。nonce 的前 4 字节是调用方给的随机数，后 4 字节是明文的 sha256 前缀：
//!
//! ```text
//! nonce = random4 || sha256(plain)[0..4]
//! iv    = hex(nonce)[0..16]
//! ```
//!
//! 这样同一个明文配同一个 random4 得到的 token 完全一样（可测试），而
//! 服务端又能从 iv 里反推明文摘要做完整性校验。
//!
//! ## 为什么 token 非空很重要
//!
//! 服务端拿不到有效 token 时不一定直接报错拒单，而是把整次 run 的所有采样
//! 点统一打上一个「未通过校验」的兜底类型标记。表现出来就是：里程、时长、
//! 步频、配速四项全过，只有「通过点位」那一项永远是 false，而轨迹本身
//! 无论怎么调都是干净的。这条线索很容易把人带到轨迹上去，其实是报文头部
//! 缺了东西。

use aes::Aes256;
use base64::Engine;
use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use sha2::{Digest, Sha256};

/// AES-256 固定密钥。
///
/// 32 字节 ASCII，直接当 key 用（不是 base64 解码后再用）。来自真机
/// `.rodata` 段按 0x6e 异或还原。
const GT_KEY: &[u8; 32] = b"WS1nbWQfMJ1merhqY5RsFZIxrJ/54JE=";

/// 帧头魔数。
const MAGIC: &[u8; 4] = b"GEE\0";
/// 帧版本。
const VERSION: u8 = 0x05;

type Enc = cbc::Encryptor<Aes256>;

/// 由 nonce 推 iv：8 字节转小写 hex，正好 16 字符。
fn make_iv(random4: [u8; 4], plain: &[u8]) -> [u8; 16] {
    let digest = Sha256::digest(plain);
    let mut nonce = [0u8; 8];
    nonce[..4].copy_from_slice(&random4);
    nonce[4..].copy_from_slice(&digest[..4]);

    let hex = nonce
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&hex.as_bytes()[..16]);
    iv
}

/// 生成 base64 本地 token。
///
/// `plain` 是设备档案的紧凑 JSON（字段顺序由调用方保证，不要在这里重排）；
/// `random4` 同一份明文配同一个值得到同一个 token，便于测试。
pub fn encrypt_token(plain: &[u8], random4: [u8; 4]) -> String {
    let iv = make_iv(random4, plain);
    let cipher = Enc::new(GT_KEY.into(), &iv.into())
        .encrypt_padded_vec_mut::<Pkcs7>(plain);

    let mut out = Vec::with_capacity(25 + cipher.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&iv);
    out.extend_from_slice(&(cipher.len() as u32).to_be_bytes());
    out.extend_from_slice(&cipher);

    base64::engine::general_purpose::STANDARD.encode(&out)
}

/// 紧凑 JSON 明文。
///
/// 真机的明文模板有四十来个设备档案字段，这里只放服务端一定会读的三个：
/// 账号、这次运行的 uuid、场景号。字段顺序固定，不要用 BTreeMap 重排——
/// 明文参与摘要，键序变了 iv 就变了，同一个 random4 不再可复现。
pub fn build_plain(uid: &str, uuid: &str, scene: u32) -> String {
    format!(r#"{{"uid":"{uid}","uuid":"{uuid}","scene":{scene}}}"#)
}

/// 提交时用的场景号。
pub const SCENE_RUN: u32 = 7;

/// 随机 4 字节。
pub fn random4() -> [u8; 4] {
    use rand::RngCore;
    let mut b = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut b);
    b
}

/// 一步到位：给 uid/uuid 生成这次提交要用的 token。
pub fn token_for_run(uid: &str, uuid: &str) -> String {
    let plain = build_plain(uid, uuid, SCENE_RUN);
    encrypt_token(plain.as_bytes(), random4())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 黄金向量：与真机实现逐字节对齐。
    ///
    /// 只要这里能过，说明魔数、版本、iv 推导、长度字节序、AES 模式、
    /// 填充方式这几项全对。
    #[test]
    fn golden_vector() {
        let got = encrypt_token(br#"{"a":1}"#, [0x11, 0x22, 0x33, 0x44]);
        assert_eq!(
            got, "R0VFAAUxMTIyMzM0NDAxNWFiZDdmAAAAECc8nwc1mlvLNbq5fbAGXQs="
        );
    }

    #[test]
    fn frame_layout() {
        let plain = br#"{"a":1}"#;
        let b64 = encrypt_token(plain, [0x11, 0x22, 0x33, 0x44]);
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .unwrap();

        assert_eq!(&raw[..4], MAGIC);
        assert_eq!(raw[4], VERSION);
        // iv 就是 nonce 的 hex
        assert_eq!(&raw[5..21], b"11223344015abd7f");
        // 长度字段：7 字节明文 PKCS7 填充后正好一个块
        assert_eq!(u32::from_be_bytes(raw[21..25].try_into().unwrap()), 16);
        assert_eq!(raw.len(), 25 + 16);
    }

    #[test]
    fn same_input_same_token() {
        let a = encrypt_token(b"hello", [1, 2, 3, 4]);
        let b = encrypt_token(b"hello", [1, 2, 3, 4]);
        assert_eq!(a, b);
    }

    #[test]
    fn different_nonce_different_token() {
        let a = encrypt_token(b"hello", [1, 2, 3, 4]);
        let b = encrypt_token(b"hello", [1, 2, 3, 5]);
        assert_ne!(a, b);
    }

    #[test]
    fn guess_length_field() {
        // 明文刚好 16 字节时，PKCS7 会再补一整块 → 32 字节密文
        let plain = b"0123456789abcdef";
        let b64 = encrypt_token(plain, [0u8; 4]);
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .unwrap();
        assert_eq!(u32::from_be_bytes(raw[21..25].try_into().unwrap()), 32);
    }

    #[test]
    fn plain_is_compact_json() {
        let s = build_plain("14046663", "ABC-123", SCENE_RUN);
        assert_eq!(s, r#"{"uid":"14046663","uuid":"ABC-123","scene":7}"#);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["uid"], "14046663");
        assert_eq!(v["scene"], 7);
    }

    #[test]
    fn token_is_long_and_prefixed() {
        let t = token_for_run("14046663", "ABC-123");
        assert!(t.starts_with("R0VF"), "base64 的 'GEE' 前缀");
        assert!(t.len() > 40, "实际长度 {}", t.len());
    }
}
