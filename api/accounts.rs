//! 多账号管理：存盘、加密、会话隔离。
//!
//! 运动世界校园有**单设备登录限制**（错误码 `10121`）：同一账号在别处登录会把
//! 当前会话踢掉，并且锁一小时。所以每个账号的 token 必须独立保存、独立刷新，
//! 不能共用一份 session。
//!
//! 密码用 AES-128-CBC 加密后落盘，密钥由**本机指纹**派生——换机器就解不开，
//! 这正是我们要的：配置文件被人拷走也读不出明文密码。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// 一个账号。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Account {
    /// 稳定 id（登录成功后用 uid 填，登录前用手机号哈希）。
    pub id: String,
    /// 手机号。
    pub username: String,
    /// AES 加密后的密码（base64）。空表示没存密码，需要重新输入。
    #[serde(default)]
    pub password_enc: String,
    /// 登录态。
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub unid: String,
    /// 展示用。
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub school: String,
    /// 最后登录毫秒戳。
    #[serde(default)]
    pub last_login_ms: i64,
    /// 最后一次提交毫秒戳（用于静默窗口）。
    #[serde(default)]
    pub last_submit_ms: i64,
    /// 是否启用（批量刷时跳过停用的）。
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Account {
    /// 是否已登录。
    pub fn logged_in(&self) -> bool {
        !self.token.is_empty() && !self.uid.is_empty()
    }

    /// 展示名。
    pub fn label(&self) -> String {
        if !self.name.is_empty() {
            return self.name.clone();
        }
        if !self.username.is_empty() {
            return mask_phone(&self.username);
        }
        "未命名".into()
    }

    /// 距离上次提交过了多少秒。
    pub fn since_submit_sec(&self, now_ms: i64) -> i64 {
        if self.last_submit_ms <= 0 {
            return i64::MAX / 1000;
        }
        (now_ms - self.last_submit_ms) / 1000
    }
}

/// 手机号脱敏：138****8888。
pub fn mask_phone(p: &str) -> String {
    let c: Vec<char> = p.chars().collect();
    if c.len() < 7 {
        return "***".into();
    }
    format!(
        "{}****{}",
        c[..3].iter().collect::<String>(),
        c[c.len() - 4..].iter().collect::<String>()
    )
}

/// 账号库。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Book {
    #[serde(default)]
    pub accounts: Vec<Account>,
}

impl Book {
    pub fn get(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Account> {
        self.accounts.iter_mut().find(|a| a.id == id)
    }

    /// 按手机号找（登录前还不知道 uid）。
    pub fn by_username(&self, username: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.username == username)
    }

    /// 插入或更新。返回 id。
    pub fn upsert(&mut self, acc: Account) -> String {
        let id = acc.id.clone();
        match self.accounts.iter_mut().find(|a| a.id == id) {
            Some(slot) => *slot = acc,
            None => self.accounts.push(acc),
        }
        id
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let n = self.accounts.len();
        self.accounts.retain(|a| a.id != id);
        self.accounts.len() != n
    }

    /// 已登录且启用的账号。
    pub fn ready(&self) -> Vec<&Account> {
        self.accounts
            .iter()
            .filter(|a| a.enabled && a.logged_in())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// 落盘
// ---------------------------------------------------------------------------

fn data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".pace-forge")
}

fn book_path() -> PathBuf {
    data_dir().join("accounts.json")
}

/// 本机密钥：由 hostname + 用户名 + 固定盐派生。
///
/// 不是"安全"的密钥管理，目的是**换机器解不开**，而不是防同机攻击。
fn machine_key() -> [u8; 16] {
    let host = std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into());
    let user = std::env::var("USER").unwrap_or_else(|_| "root".into());
    let mut h = Sha256::new();
    h.update(b"pace-forge/accounts/v1");
    h.update(host.as_bytes());
    h.update(b"|");
    h.update(user.as_bytes());
    let d = h.finalize();
    let mut k = [0u8; 16];
    k.copy_from_slice(&d[..16]);
    k
}

/// 加密密码。
pub fn seal_password(plain: &str) -> Result<String, String> {
    use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
    type Enc = cbc::Encryptor<aes::Aes128>;

    let key = machine_key();
    let iv = [0u8; 16];
    let enc = Enc::new(&key.into(), &iv.into());
    let ct = enc
        .encrypt_padded_vec_mut::<Pkcs7>(plain.as_bytes());
    use base64::Engine;
    Ok(base64::engine::general_purpose::STANDARD.encode(ct))
}

/// 解密密码。
pub fn open_password(enc: &str) -> Result<String, String> {
    use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
    type Dec = cbc::Decryptor<aes::Aes128>;

    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(enc)
        .map_err(|e| format!("密码字段不是合法 base64: {e}"))?;
    let key = machine_key();
    let iv = [0u8; 16];
    let dec = Dec::new(&key.into(), &iv.into());
    let pt = dec
        .decrypt_padded_vec_mut::<Pkcs7>(&raw)
        .map_err(|e| format!("密码解密失败（可能换了机器）: {e}"))?;
    String::from_utf8(pt).map_err(|e| format!("密码不是 UTF-8: {e}"))
}

/// 读账号库。文件不存在返回空库（不是错误）。
pub fn load() -> Book {
    let p = book_path();
    let Ok(text) = std::fs::read_to_string(&p) else {
        return Book::default();
    };
    match serde_json::from_str::<Book>(&text) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("账号库解析失败（已忽略）: {e}");
            Book::default()
        }
    }
}

/// 写账号库。权限 600。
pub fn save(book: &Book) -> Result<(), String> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("建目录失败: {e}"))?;
    let p = book_path();
    let text = serde_json::to_string_pretty(book).map_err(|e| e.to_string())?;

    // 先写临时文件再改名，避免写一半断电把库毁了
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, &text).map_err(|e| format!("写账号库失败: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &p).map_err(|e| format!("替换账号库失败: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrip() {
        let p = "myS3cretPass!";
        let enc = seal_password(p).unwrap();
        assert_ne!(enc, p);
        assert_eq!(open_password(&enc).unwrap(), p);
    }

    #[test]
    fn empty_password_roundtrip() {
        let enc = seal_password("").unwrap();
        assert_eq!(open_password(&enc).unwrap(), "");
    }

    #[test]
    fn mask_hides_middle() {
        assert_eq!(mask_phone("13812348888"), "138****8888");
        assert_eq!(mask_phone("123"), "***");
    }

    #[test]
    fn upsert_replaces_same_id() {
        let mut b = Book::default();
        b.upsert(Account {
            id: "u1".into(),
            name: "甲".into(),
            ..Default::default()
        });
        b.upsert(Account {
            id: "u1".into(),
            name: "乙".into(),
            ..Default::default()
        });
        assert_eq!(b.accounts.len(), 1);
        assert_eq!(b.get("u1").unwrap().name, "乙");
    }

    #[test]
    fn ready_filters_disabled_and_logged_out() {
        let mut b = Book::default();
        b.upsert(Account {
            id: "a".into(),
            token: "t".into(),
            uid: "1".into(),
            enabled: true,
            ..Default::default()
        });
        b.upsert(Account {
            id: "b".into(),
            token: "t".into(),
            uid: "2".into(),
            enabled: false,
            ..Default::default()
        });
        b.upsert(Account {
            id: "c".into(),
            enabled: true,
            ..Default::default()
        });
        let r = b.ready();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].id, "a");
    }

    #[test]
    fn since_submit_handles_never() {
        let a = Account::default();
        assert!(a.since_submit_sec(1_700_000_000_000) > 1_000_000);
        let a2 = Account {
            last_submit_ms: 1_700_000_000_000,
            ..Default::default()
        };
        assert_eq!(a2.since_submit_sec(1_700_000_600_000), 600);
    }

    #[test]
    fn book_roundtrip_json() {
        let mut b = Book::default();
        b.upsert(Account {
            id: "x".into(),
            username: "13812348888".into(),
            password_enc: seal_password("pw").unwrap(),
            token: "tok".into(),
            uid: "42".into(),
            enabled: true,
            ..Default::default()
        });
        let js = serde_json::to_string(&b).unwrap();
        let back: Book = serde_json::from_str(&js).unwrap();
        let a = back.get("x").unwrap();
        assert_eq!(open_password(&a.password_enc).unwrap(), "pw");
        assert_eq!(a.token, "tok");
    }
}
