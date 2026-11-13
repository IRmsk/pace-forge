//! 会话与设备身份的持久化。
//!
//! 两层必须分开存：
//!
//! - **设备身份**（`identity.json`）：`device_id` 全生命周期复用。每次随机生成
//!   会让服务端认成「另一台设备」，直接撞上 `10121 此账号已在其他设备登录`。
//! - **登录会话**（`session.json`）：登录成功即落盘，登出时删除。
//!
//! 会话**必须落盘**，不能只放内存。只在内存的后果是进程一重启 token 就没了，
//! 而服务端那边的会话还活着 —— 此时想登出也拿不出凭证，账号会被锁一小时。
//!
//! 设计要点：
//! `load_identity` / `save_session` / `clear_session` 三段式。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 登录会话。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub unid: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub campus_name: String,
    #[serde(default)]
    pub depart: String,
    #[serde(default)]
    pub grade_class: String,
    /// 登录时使用的设备标识，登出时用于确认是同一台机
    #[serde(default)]
    pub device_id: String,
}

impl Session {
    pub fn is_valid(&self) -> bool {
        !self.token.is_empty() && !self.uid.is_empty()
    }
}

/// 数据目录名（相对 `$HOME`）。
pub const DATA_DIR_NAME: &str = ".pace-forge";

/// `$HOME`。只读环境变量，不写、不创建任何东西。
pub(crate) fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// 给定 HOME 基目录，拼出数据目录：`<base>/.pace-forge`。
///
/// **纯路径计算，不碰磁盘。** 测试用它把「数据目录」注入成
/// `std::env::temp_dir()` 下的一次性目录，从而完全不碰真实
/// `$HOME/.pace-forge/`（否则跑一次 `cargo test` 就会删掉真实登录态）。
pub fn data_dir_in(base: &Path) -> PathBuf {
    base.join(DATA_DIR_NAME)
}

/// 数据目录：`$HOME/.pace-forge/`。
///
/// 只算路径，不做 IO。早先这里带 `create_dir_all`，等于任何一次只读调用
/// 都会在真实 HOME 下建目录；建目录的责任现在落在真正写盘的唯一入口
/// —— [`write_file_private`]（`ensure_dir` + 0600 写文件），
/// `save_session` / `accounts::save` / `client::persistent_device_id` /
/// `track::roadnet` 的四类缓存 / `track::tileroad` 的瓦片 / `policy` 的
/// 调试转储都走它。
pub fn data_dir() -> PathBuf {
    data_dir_in(&home_dir())
}

// ---------------------------------------------------------------------------
// 落盘原语（全项目唯一的「建目录 + 0600 写文件」实现）
// ---------------------------------------------------------------------------

/// 确保目录存在（幂等）。
///
/// **建目录的责任点就在这里**。`data_dir()` / `data_dir_in()` 只算路径、不做
/// IO，所以任何写盘路径都必须先走这里，否则首次运行时目录不存在、写失败。
pub(crate) fn ensure_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("建目录 {} 失败: {e}", dir.display()))
}

/// 以 0600 权限写一个文件，**自动建父目录**。
///
/// 用 `File::create` + `set_permissions` 而不是 `fs::write`：`fs::write` 吃
/// umask（常见 0644），同机其他用户能读到 token / 加密后的密码。这些文件
/// 一律 0600。
///
/// 目录不存在时由这里负责建出来 —— `~/.pace-forge/` 在首次运行时并不存在，
/// 早先 `data_dir()` 带 `create_dir_all` 副作用把它掩盖了；副作用去掉之后，
/// 这个责任必须显式落在每个写盘路径上。
pub(crate) fn write_file_private(path: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            ensure_dir(parent)?;
        }
    }
    let mut f = std::fs::File::create(path)
        .map_err(|e| format!("写入 {} 失败: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    use std::io::Write;
    f.write_all(data)
        .map_err(|e| format!("写入 {} 失败: {e}", path.display()))
}

fn read_json_below<T: serde::de::DeserializeOwned>(dir: &Path, name: &str) -> Option<T> {
    let text = std::fs::read_to_string(dir.join(name)).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_json_below<T: Serialize>(dir: &Path, name: &str, value: &T) -> Result<(), String> {
    let json = serde_json::to_string_pretty(value).map_err(|e| format!("序列化 {name} 失败: {e}"))?;
    // 会话文件含 token，write_file_private 会建目录并收紧到 0600
    write_file_private(&dir.join(name), json.as_bytes())
}

/// 读取会话。没有或损坏时返回空会话。
pub fn load_session() -> Session {
    load_session_below(&data_dir())
}

/// 同上，但数据目录由调用方指定（测试注入隔离目录用）。
pub(crate) fn load_session_below(dir: &Path) -> Session {
    read_json_below(dir, "session.json").unwrap_or_default()
}

/// 写入会话（登录成功后立即调用）。
pub fn save_session(s: &Session) -> Result<(), String> {
    save_session_below(&data_dir(), s)
}

/// 同上，但数据目录由调用方指定。
pub(crate) fn save_session_below(dir: &Path, s: &Session) -> Result<(), String> {
    write_json_below(dir, "session.json", s)
}

/// 删除会话文件。
pub fn clear_session() {
    clear_session_below(&data_dir());
}

/// 同上，但数据目录由调用方指定。
///
/// **测试必须用这个而不是 `clear_session()`**：`clear_session()` 删的是真实
/// `$HOME/.pace-forge/session.json`，测试里调用它等于把用户的登录态删掉。
pub(crate) fn clear_session_below(dir: &Path) {
    let _ = std::fs::remove_file(dir.join("session.json"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 一次性隔离目录：`$TMPDIR/pace-forge-test-<tag>-<pid>-<seq>-<nanos>`。
    ///
    /// 刻意**不用** `std::env::set_var("HOME", …)`：测试并行执行，改全局环境
    /// 变量会让「谁先改谁赢」，不同测试互相串；这里改成把 HOME 基目录注入给
    /// `*_below` 系列函数，每个测试各拿一个互不重叠的临时目录，`Drop` 时清掉。
    struct TestDir {
        root: PathBuf,
    }

    impl TestDir {
        fn new(tag: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let root = std::env::temp_dir().join(format!(
                "pace-forge-test-{tag}-{}-{n}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).expect("建临时基目录失败");
            Self { root }
        }

        /// 注入给 `*_below` 的「HOME 基目录」。
        fn home(&self) -> &Path {
            &self.root
        }

        /// 本测试独占的数据目录（等价于隔离环境里的 `$HOME/.pace-forge`）。
        fn data(&self) -> PathBuf {
            data_dir_in(self.home())
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn temp_session() -> Session {
        Session {
            uid: "123".into(),
            token: "abc".into(),
            unid: "88".into(),
            username: "13800000000".into(),
            name: "测试".into(),
            campus_name: "某大学".into(),
            depart: "计算机".into(),
            grade_class: "2024级".into(),
            device_id: "deadbeef".into(),
        }
    }

    #[test]
    fn session_roundtrip() {
        let t = TestDir::new("roundtrip");
        let mut s = temp_session();
        s.name = "往返测试".into();
        save_session_below(&t.data(), &s).unwrap();
        let back = load_session_below(&t.data());
        assert_eq!(back.uid, "123");
        assert_eq!(back.token, "abc");
        assert_eq!(back.name, "往返测试");
        assert!(t.data().join("session.json").exists(), "应写在隔离目录里");
    }

    #[test]
    fn json_roundtrip_in_isolated_dir() {
        let t = TestDir::new("json");
        let s = temp_session();
        write_json_below(&t.data(), "session_test.json", &s).unwrap();
        let back: Session = read_json_below(&t.data(), "session_test.json").unwrap();
        assert_eq!(back.uid, "123");
        assert!(t.data().join("session_test.json").exists());
    }

    /// 没有会话时返回空会话而不是报错。
    ///
    /// 这个测试以前调 `clear_session()`，删的是**真实**的
    /// `$HOME/.pace-forge/session.json` —— 跑一次 `cargo test` 就把本地登录态
    /// 删了，CLI 立刻变「未登录」，而服务端会话还活着，重登直接撞
    /// `10121 此账号已在其他设备登录`（锁号一小时）。现在只在隔离目录里删。
    #[test]
    fn missing_session_is_empty_not_error() {
        let t = TestDir::new("missing");
        // 先放一个真会话进去，再清掉，模拟「登录过又退出」
        save_session_below(&t.data(), &temp_session()).unwrap();
        clear_session_below(&t.data());
        let s = load_session_below(&t.data());
        assert!(s.token.is_empty(), "没有会话文件时 token 应为空");
        assert!(s.uid.is_empty());
        assert!(!s.is_valid(), "空会话不算有效会话");

        // 从未写过会话的目录同样返回空会话，不 panic、不报错
        let t2 = TestDir::new("never-written");
        let s2 = load_session_below(&t2.data());
        assert!(!s2.is_valid());
    }

    #[test]
    fn validity_requires_token_and_uid() {
        let mut s = temp_session();
        assert!(s.is_valid());
        s.token.clear();
        assert!(!s.is_valid(), "没有 token 就不算有效会话");
        s.token = "x".into();
        s.uid.clear();
        assert!(!s.is_valid(), "没有 uid 也不算了");
    }

    /// 首次写盘时应自动建出数据目录（`data_dir_in` 本身不做 IO）。
    #[test]
    fn data_dir_is_created_on_first_write() {
        let t = TestDir::new("created");
        let d = t.data();
        assert!(d.to_string_lossy().contains(".pace-forge"));
        assert!(!d.exists(), "只算路径的 data_dir_in 不该建目录");
        save_session_below(&d, &temp_session()).unwrap();
        assert!(d.exists(), "首次落盘应自动创建数据目录");
    }

    /// 多级目录（如 `~/.pace-forge/tiles/`）也要一次建成。
    ///
    /// 瓦片缓存写的是 `<data>/tiles/<z>_<x>_<y>.png`，父目录有两级；`create_dir_all`
    /// 语义必须覆盖这种情况，否则首次拉瓦片会失败。
    #[test]
    fn write_creates_nested_dirs_on_first_use() {
        let t = TestDir::new("nested");
        let nested = t.data().join("tiles");
        assert!(!nested.exists());
        write_json_below(&nested, "probe.json", &temp_session()).unwrap();
        assert!(nested.exists(), "嵌套目录也应被建出来");
        assert!(nested.join("probe.json").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(nested.join("probe.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "写出的文件权限必须是 600");
        }
    }

    /// 回归护栏：测试用的数据目录**绝不能**是真实的 `$HOME/.pace-forge`。
    ///
    /// 谁哪天把 `_below` 改回 `data_dir()`，这条会立刻炸。
    #[test]
    fn test_data_dir_is_isolated_from_real_home() {
        let t = TestDir::new("isolated");
        let test_dir = t.data();
        // data_dir_in(&home_dir()) 与 data_dir() 同值，但不创建目录、不读文件
        let real_dir = data_dir_in(&home_dir());

        assert_ne!(
            test_dir, real_dir,
            "测试数据目录不能等于真实 $HOME/.pace-forge"
        );
        assert!(
            !test_dir.starts_with(&real_dir),
            "测试数据目录不能落在真实 $HOME/.pace-forge 之下: {}",
            test_dir.display()
        );
        assert!(
            test_dir.starts_with(std::env::temp_dir()),
            "测试数据目录应在临时目录下: {}",
            test_dir.display()
        );
        assert!(
            test_dir.to_string_lossy().contains("pace-forge-test-"),
            "测试数据目录应带一次性前缀: {}",
            test_dir.display()
        );
    }
}

