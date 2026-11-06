//! NetSecKit 4.2.4 协议加密层。
//!
//! 模块划分与行为均以真实服务器联调为准：
//! - [`envelope`] 请求封包（AES-128-CBC + RSA-1024 + 全零 IV）
//! - [`decrypt`]  响应解密与 RSA 验签（fail-closed）
//! - [`kdf`]      响应密钥派生（自定义三段式）
//! - [`sign`]     tokenSign / signature（MD5 + 盐）
//! - [`consts`]   盐、公钥、主机等常量

pub mod consts;
pub mod decrypt;
pub mod envelope;
pub mod kdf;
pub mod sign;

pub use consts::{hosts, CODE_OK, SIGN_SALT};
pub use decrypt::{open, DecryptedResponse};
pub use envelope::{seal, Envelope, SessionKeys};
pub use kdf::response_kdf;
pub use sign::{sport_signature, token_sign, SignFields};
