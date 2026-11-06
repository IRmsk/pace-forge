//! NetSecKit 4.2.4 协议常量。
//!
//! 全部数值来自对 `libnetseckit-4.2.4.so` 的静态分析与实机联调验证。
//! 每一处改动都必须重新跑 `cargo test` 并用 `selftest` 对真实服务器确认。

/// 请求签名盐。跨版本通用（7.3.11 → 7.4.01 未变）。
pub const SIGN_SALT: &str = "2slhe02lsfiwowlcixisla_sls-_slaor";

/// 请求 RSA 公钥（PKCS#1，1024-bit DER，base64）。
/// 用途：加密每次请求随机生成的 AES-128 key。
pub const REQ_RSA_PKCS1_B64: &str = concat!(
    "MIGJAoGBALmmqVPOwY1mTVHGEfg7jHck1MNXsVtUfrqY99bm/W2cjLi3LvG/wMYw",
    "bhmf9O+y3CUlZ5g4AMd2Ly4XbWUloG/O8+1USGJ8ddmrzbl8j2EZi0OhpVCYI381",
    "+zZ0qa3z1cOuG2XdJnE7pO6Y4JNC37miqMXYmjejVi2QDIbONomfAgMBAAE="
);

/// 响应验签公钥（SPKI，1024-bit DER，base64）。
/// 用途：验证服务端对响应明文的 PKCS#1 v1.5 签名。
pub const RESP_VERIFY_SPKI_B64: &str = concat!(
    "MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDMs6bFhb5lrf/xqwtNcKVHW09c",
    "oMbtLAmr9Rti7vnSnVroFuLHJEzxMqPQeItByr1Fu4RAQMR/aq/oknn53kCOuSaV",
    "sd9mnDCusrs7GmExDxlKe2pNAKTEZnT1FITQYT2SJkeCZ8iBKujqN61C3SXQ4Xa8",
    "QRr2uMTicec6XC08WQIDAQAB"
);

/// 每次请求 AES key 的字符集。
pub const KEY_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// keyData 四段拼接用的固定尾巴。
pub const KEYDATA_TAIL: &str = "ABCDEFGHIJKLMN12";

/// 请求信封的协议版本标识。
pub const PROTO_VERSION: &str = "101";

/// 业务成功码。
pub const CODE_OK: i64 = 10000;

/// 网关主机。
pub mod hosts {
    pub const RUN: &str = "https://run.gxapp.iydsj.com";
    pub const DISCOVERY: &str = "https://discovery.gxapp.iydsj.com";
    /// 埋点专用。注意 `startup/pv` 只在这个 host 上存在，run 会返回 404。
    pub const DATAPOINT: &str = "https://datapoint.gxapp.iydsj.com";
    pub const PARTNER: &str = "https://partner.iydsj.com";
}
