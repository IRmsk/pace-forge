//! 签名计算。
//!
//! 两个用途共用同一套算法（MD5 + 排序拼接 + 盐）：
//! - `token_sign`：请求头 `tokenSign`
//! - `sport_signature`：提交报文的 `signature`
//!
//! 算法：丢弃 None 和 `signature` 键 → 布尔转小写 → 按 (key小写, key) 双键排序
//! → `k1=v1&k2=v2&...` 末尾接盐 → MD5 小写 hex。
//!
//! 注意：这是**摘要**，不是 HMAC。盐是公开常量，安全性不依赖它保密。

use md5::{Digest, Md5};

use super::consts::SIGN_SALT;

/// 参与签名的键值对。值统一转成字符串，`None` 会被丢弃。
pub type SignFields = Vec<(String, Option<String>)>;

/// 计算加盐 MD5 签名。
pub fn sign_with_salt(fields: &SignFields, salt: &str) -> String {
    let mut items: Vec<(String, String)> = fields
        .iter()
        .filter_map(|(k, v)| {
            let v = v.as_ref()?;
            if k.eq_ignore_ascii_case("signature") {
                return None;
            }
            Some((k.clone(), v.clone()))
        })
        .collect();

    // 双键排序：先按小写，再按原样。保证大小写不同的同名键顺序确定。
    items.sort_by(|a, b| {
        a.0.to_lowercase()
            .cmp(&b.0.to_lowercase())
            .then_with(|| a.0.cmp(&b.0))
    });

    let canonical = items
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");

    let mut hasher = Md5::new();
    hasher.update(canonical.as_bytes());
    hasher.update(salt.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// 不带盐的原始拼接串（用于排查问题、比对服务端日志）。
pub fn canonical_string(fields: &SignFields) -> String {
    let mut items: Vec<(String, String)> = fields
        .iter()
        .filter_map(|(k, v)| {
            let v = v.as_ref()?;
            if k.eq_ignore_ascii_case("signature") {
                return None;
            }
            Some((k.clone(), v.clone()))
        })
        .collect();
    items.sort_by(|a, b| {
        a.0.to_lowercase()
            .cmp(&b.0.to_lowercase())
            .then_with(|| a.0.cmp(&b.0))
    });
    items
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// 请求头签名（`tokenSign`）。
pub fn token_sign(fields: &SignFields) -> String {
    sign_with_salt(fields, SIGN_SALT)
}

/// 提交报文签名（`signature`）。
pub fn sport_signature(fields: &SignFields) -> String {
    sign_with_salt(fields, SIGN_SALT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(pairs: &[(&str, &str)]) -> SignFields {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Some(v.to_string())))
            .collect()
    }

    #[test]
    fn sorts_case_insensitively() {
        // uid 与 UID 同时存在时顺序必须稳定
        let a = sign_with_salt(&f(&[("uid", "1"), ("UID", "2")]), "s");
        let b = sign_with_salt(&f(&[("UID", "2"), ("uid", "1")]), "s");
        assert_eq!(a, b, "输入顺序不应影响结果");
    }

    #[test]
    fn drops_signature_key() {
        let with = sign_with_salt(&f(&[("a", "1"), ("signature", "xxx")]), "s");
        let without = sign_with_salt(&f(&[("a", "1")]), "s");
        assert_eq!(with, without, "signature 键必须被排除");
    }

    #[test]
    fn drops_none_values() {
        let mut fields: SignFields = f(&[("a", "1")]);
        fields.push(("b".into(), None));
        assert_eq!(sign_with_salt(&fields, "s"), sign_with_salt(&f(&[("a", "1")]), "s"));
    }

    #[test]
    fn matches_reference_vector() {
        // 金标准：Python 实现 compute_token_sign({"uid":"123","token":"abc","timeStamp":"1730000000000"})
        let got = token_sign(&f(&[
            ("uid", "123"),
            ("token", "abc"),
            ("timeStamp", "1730000000000"),
        ]));
        assert_eq!(got, "3388658301c0b8e4a40f73cc17c81e71", "与 Python 实现不一致");
    }

    #[test]
    fn output_is_lowercase_hex_32() {
        let s = token_sign(&f(&[("a", "1")]));
        assert_eq!(s.len(), 32);
        assert_eq!(s, s.to_lowercase());
    }
}
