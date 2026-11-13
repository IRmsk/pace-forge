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
use std::path::{Path, PathBuf};

use super::session::{data_dir_in, ensure_dir, home_dir, write_file_private};

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
        /// 把本地所有账号的 token 清空，保留用户名/学校等信息。
    ///
    /// 服务端注销成功后必须调，否则本地还留着已经作废的 token，
    /// 下次请求会拿着它去撞服务端，表现成一堆莫名其妙的鉴权失败。
    pub fn clear_tokens(&mut self) -> usize {
        let mut n = 0;
        for acc in &mut self.accounts {
            if !acc.token.is_empty() {
                acc.token.clear();
                acc.uid.clear();
                acc.unid.clear();
                n += 1;
            }
        }
        n
    }

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

/// 给定 HOME 基目录，拼出账号库路径：`<base>/.pace-forge/accounts.json`。
///
/// **纯路径计算，不碰磁盘。** 测试用它把路径注入到一次性临时目录，从而完全
/// 不碰真实 `$HOME/.pace-forge/accounts.json`。
pub(crate) fn book_path_in(base: &std::path::Path) -> PathBuf {
    data_dir_in(base).join("accounts.json")
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
///
/// 读路径**不建目录**：目录不存在就是「没存过账号」，直接返回空库。
pub fn load() -> Book {
    load_in(&home_dir())
}

/// 同上，但 HOME 基目录由调用方指定（测试注入隔离目录用）。
pub(crate) fn load_in(base: &Path) -> Book {
    let p = book_path_in(base);
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

/// 写账号库。目录不存在时自动建出来，权限 600。
///
/// 目录创建走 [`ensure_dir`]，文件写入走 [`write_file_private`]（0600）——
/// 与 `session.json` 用的是同一套原语，不再各写一遍。
pub fn save(book: &Book) -> Result<(), String> {
    save_in(&home_dir(), book)
}

/// 同上，但 HOME 基目录由调用方指定。
///
/// **测试必须用这个而不是 `save()`**：`save()` 写的是真实
/// `$HOME/.pace-forge/accounts.json`。
pub(crate) fn save_in(base: &Path, book: &Book) -> Result<(), String> {
    let dir = data_dir_in(base);
    // 首次运行目录还不存在，这里负责建出来（写前必须确保）
    ensure_dir(&dir)?;
    let p = dir.join("accounts.json");
    let text = serde_json::to_string_pretty(book).map_err(|e| e.to_string())?;

    // 先写临时文件再改名，避免写一半断电把库毁了
    let tmp = p.with_extension("json.tmp");
    write_file_private(&tmp, text.as_bytes()).map_err(|e| format!("写账号库失败: {e}"))?;
    std::fs::rename(&tmp, &p).map_err(|e| format!("替换账号库失败: {e}"))?;
    Ok(())
}

/// 清掉本地所有账号的 token 并落盘。
pub fn clear_tokens() -> Result<usize, String> {
    clear_tokens_in(&home_dir())
}

/// 同上，但 HOME 基目录由调用方指定。
pub(crate) fn clear_tokens_in(base: &Path) -> Result<usize, String> {
    let mut book = load_in(base);
    let n = book.clear_tokens();
    save_in(base, &book)?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{assert_isolated, TestDir};

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

    fn sample_book() -> Book {
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
        b
    }

    /// 账号库往返必须落在**注入目录**里，不碰真实 `$HOME`。
    #[test]
    fn book_roundtrip_in_isolated_dir() {
        let t = TestDir::new("accounts-roundtrip");
        save_in(t.home(), &sample_book()).unwrap();

        let path = book_path_in(t.home());
        assert!(path.exists(), "应写在隔离目录里: {}", path.display());
        assert!(path.starts_with(t.home()), "路径必须在注入基目录下");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "账号库含 token，权限必须是 600");
        }

        let back = load_in(t.home());
        let a = back.get("x").expect("应读回账号");
        assert_eq!(a.token, "tok");
        assert_eq!(open_password(&a.password_enc).unwrap(), "pw");
        assert!(
            !book_path_in(t.home()).with_extension("json.tmp").exists(),
            "临时文件应已改名，不该残留"
        );
    }

    /// 首次运行：数据目录不存在时，`save_in` 必须自己把它建出来。
    ///
    /// 这是「`data_dir()` 去掉 create_dir_all 副作用之后，谁负责建目录」的
    /// 回归测试——建目录的责任落在 `save_in`（经 `ensure_dir`）。
    #[test]
    fn save_creates_data_dir_on_first_write() {
        let t = TestDir::new("accounts-first-run");
        let data = t.data();
        assert_eq!(data.file_name().unwrap(), ".pace-forge");
        assert!(!data.exists(), "注入目录下不该预先存在 .pace-forge");

        save_in(t.home(), &sample_book()).unwrap();

        assert!(data.exists(), "首次落盘必须自动创建数据目录");
        assert!(data.join("accounts.json").exists());
        assert_eq!(load_in(t.home()).accounts.len(), 1);
    }

    /// 从未写过账号库时返回空库，且**不**因为读而创建目录。
    #[test]
    fn missing_book_is_empty_and_creates_nothing() {
        let t = TestDir::new("accounts-missing");
        let b = load_in(t.home());
        assert!(b.accounts.is_empty());
        assert!(!t.data().exists(), "读路径不该创建数据目录");
    }

    /// `clear_tokens_in` 只改注入目录里的账号库。
    #[test]
    fn clear_tokens_works_on_isolated_dir() {
        let t = TestDir::new("accounts-clear");
        save_in(t.home(), &sample_book()).unwrap();
        assert_eq!(clear_tokens_in(t.home()).unwrap(), 1);
        let back = load_in(t.home());
        let a = back.get("x").unwrap();
        assert!(a.token.is_empty(), "token 应被清空");
        assert!(!a.logged_in());
    }

    /// 护栏：账号库的注入路径**绝不能**是真实 `$HOME/.pace-forge`。
    ///
    /// 模式与 `session::tests::test_data_dir_is_isolated_from_real_home` 一致。
    /// 谁哪天把 `save_in` / `load_in` 改回 `save()` / `load()`，配合下面的
    /// `default_book_path_matches_production_layout` 会立刻暴露。
    #[test]
    fn accounts_paths_are_isolated_from_real_home() {
        let t = TestDir::new("accounts-isolated");
        assert_isolated(&book_path_in(t.home()), "账号库注入路径");
        // 注入路径与生产默认路径必须不同
        assert_ne!(
            book_path_in(t.home()),
            book_path_in(&crate::test_support::real_home()),
            "注入路径不能等于生产默认路径"
        );
    }

    /// 生产默认路径仍是 `$HOME/.pace-forge/accounts.json`（布局不能变）。
    #[test]
    fn default_book_path_matches_production_layout() {
        let p = book_path_in(&crate::test_support::real_home());
        assert_eq!(p.file_name().unwrap(), "accounts.json");
        assert_eq!(p.parent().unwrap().file_name().unwrap(), ".pace-forge");
    }
}
