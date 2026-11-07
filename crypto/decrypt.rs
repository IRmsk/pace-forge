//! 响应解密与验签（NetSecKit 4.2.4）。
//!
//! 服务端响应结构：
//!
//! ```json
//! {"r":"<AES密文b64>","s":"<RSA签名b64>","v":101}
//! ```
//!
//! 解密流程：
//! 1. `v` 必须是 101
//! 2. 用会话派生的 `response_key` 做 AES-128-CBC 解密（IV 全零），去 PKCS#7
//! 3. 用响应公钥验 `s`：签名是 MD5(明文) 的 PKCS#1 v1.5 原始签名
//! 4. 内层是 `{"data":"<业务JSON的b64>"}`，解出来才是业务数据
//!
//! **验签默认强制**（fail-closed）：验不过就报错，不返回可能被篡改的数据。
//! 这与上游实现一致——宁可失败，也不要静默接受一个来路不明的响应。

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use md5::{Digest, Md5};
use rsa::pkcs8::DecodePublicKey;
use rsa::traits::PublicKeyParts;
use rsa::RsaPublicKey;

use super::consts::RESP_VERIFY_SPKI_B64;
use super::envelope::SessionKeys;

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// 解密后的响应。
#[derive(Debug, Clone)]
pub struct DecryptedResponse {
    /// 业务 JSON 原文（已从 base64 解出）。
    pub body: String,
    /// 验签是否通过。`require_signature=false` 时为 `None`。
    pub signature_ok: Option<bool>,
}

/// 解密响应。
///
/// `require_signature = true` 时验签失败直接返回 Err（fail-closed）。
pub fn open(
    wire: &str,
    session: &SessionKeys,
    require_signature: bool,
) -> Result<DecryptedResponse, String> {
    let v: serde_json::Value =
        serde_json::from_str(wire).map_err(|e| format!("响应不是合法 JSON: {e}"))?;

    // 1. 版本校验
    let ver = v.get("v").and_then(|x| x.as_i64());
    if ver != Some(101) {
        return Err(format!("响应版本不是 101，实为 {ver:?}"));
    }

    // 2. AES 解密
    let r = v
        .get("r")
        .and_then(|x| x.as_str())
        .ok_or("响应缺 r 字段")?;
    let cipher = B64.decode(r).map_err(|e| format!("r 不是合法 base64: {e}"))?;
    let plain_padded = Aes128CbcDec::new_from_slices(&session.response_key, &[0u8; 16])
        .map_err(|e| format!("AES 初始化失败: {e}"))?
        .decrypt_padded_vec_mut::<Pkcs7>(&cipher)
        .map_err(|_| "PKCS#7 去填充失败（响应 key 不匹配？）".to_string())?;

    // 3. 验签
    let signature_ok = match v.get("s").and_then(|x| x.as_str()) {
        Some(s) if !s.is_empty() => {
            let ok = verify_signature(&plain_padded, s)?;
            if require_signature && !ok {
                return Err("响应签名校验失败，数据可能被篡改".into());
            }
            Some(ok)
        }
        _ => {
            if require_signature {
                return Err("响应缺 s 字段，强制验签模式下不接受".into());
            }
            None
        }
    };

    // 4. 内层 data
    let inner: serde_json::Value = serde_json::from_slice(&plain_padded)
        .map_err(|e| format!("内层明文不是 JSON: {e}"))?;
    let data_b64 = inner
        .get("data")
        .and_then(|x| x.as_str())
        .ok_or("内层缺 data 字段")?;
    let body_bytes = B64
        .decode(data_b64)
        .map_err(|e| format!("data 不是合法 base64: {e}"))?;
    let body = String::from_utf8_lossy(&body_bytes).into_owned();

    Ok(DecryptedResponse { body, signature_ok })
}

/// 校验 RSA 签名（PKCS#1 v1.5，摘要为 MD5）。
///
/// 手工做模幂与填充检查，而不是走 `rsa` crate 的 VerifyingKey——
/// 因为后者要求摘要类型实现 `AssociatedOid`，而我们要精确复刻服务端的
/// 「MD5 + PKCS#1 v1.5 原始签名」语义。
fn verify_signature(plaintext: &[u8], sig_b64: &str) -> Result<bool, String> {
    let der = B64
        .decode(RESP_VERIFY_SPKI_B64)
        .map_err(|e| format!("响应公钥 base64 解码失败: {e}"))?;
    let pubkey = RsaPublicKey::from_public_key_der(&der)
        .map_err(|e| format!("响应公钥解析失败: {e}"))?;

    let sig = B64.decode(sig_b64).map_err(|e| format!("s 不是合法 base64: {e}"))?;
    let block_size = pubkey.n().bits().div_ceil(8);
    if sig.len() != block_size {
        return Err(format!(
            "签名长度 {} 与模数 {} 字节不匹配",
            sig.len(),
            block_size
        ));
    }

    let sig_int = rsa::BigUint::from_bytes_be(&sig);
    let recovered = sig_int.modpow(pubkey.e(), pubkey.n());
    let recovered_bytes = right_pad_to(&recovered.to_bytes_be(), block_size);

    // PKCS#1 v1.5 签名填充：0x00 0x01 0xFF... 0x00 <digest>
    if !recovered_bytes.starts_with(&[0x00, 0x01]) {
        return Ok(false);
    }
    let sep = match recovered_bytes[2..].iter().position(|&b| b == 0x00) {
        Some(i) => i + 2,
        None => return Ok(false),
    };
    if sep < 10 || !recovered_bytes[2..sep].iter().all(|&b| b == 0xFF) {
        return Ok(false);
    }
    let recovered_digest = &recovered_bytes[sep + 1..];
    let expected = {
        let mut h = Md5::new();
        h.update(plaintext);
        format!("{:x}", h.finalize())
    };
    Ok(recovered_digest == expected.as_bytes())
}

fn right_pad_to(v: &[u8], size: usize) -> Vec<u8> {
    if v.len() >= size {
        return v[v.len() - size..].to_vec();
    }
    let mut out = vec![0u8; size - v.len()];
    out.extend_from_slice(v);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_wrong_version() {
        let s = SessionKeys::generate();
        let wire = r#"{"r":"AAAA","s":"","v":100}"#;
        let err = open(wire, &s, false).unwrap_err();
        assert!(err.contains("101"), "应提示版本不符: {err}");
    }

    #[test]
    fn rejects_garbage() {
        let s = SessionKeys::generate();
        assert!(open("not json", &s, false).is_err());
    }

    #[test]
    fn requires_signature_when_asked() {
        let s = SessionKeys::generate();
        let wire = r#"{"r":"AAAA","s":"","v":101}"#;
        assert!(open(wire, &s, true).is_err(), "强制验签时缺 s 必须失败");
    }
}
