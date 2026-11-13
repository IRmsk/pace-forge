//! 测试专用工具：一次性隔离目录。
//!
//! 存在的理由是**一次真实的教训**：跑 `cargo test` 把用户本地的
//! `$HOME/.pace-forge/session.json` 删了，CLI 立刻变「未登录」，而服务端会话
//! 还活着，重登直接撞 `10121 此账号已在其他设备登录`（锁号一小时）。
//!
//! 所以测试里**不允许**碰真实 `$HOME`：
//!
//! - 不用 `std::env::set_var("HOME", …)`：测试并行执行，改全局环境变量等于
//!   「谁先改谁赢」，不同测试互相串；
//! - 改成把「HOME 基目录」注入给每个 `*_in(base)` 系列函数，每个测试各拿一个
//!   互不重叠的临时目录，`Drop` 时整棵删掉。
//!
//! 各模块的 `*_in` 版本 + 本文件构成两层防御：
//!
//! 1. 实现层：默认路径（生产用）与注入路径（测试用）共用同一段代码，只是 base
//!    不同 —— 测试覆盖的正是生产会走的那条逻辑；
//! 2. 护栏层：每个模块各有一条 `*_is_isolated_from_real_home` 测试，断言
//!    「注入出来的路径」既不等于、也不落在真实 `$HOME/.pace-forge` 之下。
//!    谁哪天把 `*_in` 改回 `data_dir()`，那条测试立刻炸。
//!
//! 本模块只在 `#[cfg(test)]` 下编译，不进生产二进制。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// 一次性隔离目录：`$TMPDIR/pace-forge-test-<tag>-<pid>-<seq>-<nanos>`。
pub(crate) struct TestDir {
    root: PathBuf,
}

impl TestDir {
    pub(crate) fn new(tag: &str) -> Self {
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

    /// 注入给 `*_in` 的「HOME 基目录」。
    pub(crate) fn home(&self) -> &Path {
        &self.root
    }

    /// 本测试独占的数据目录（等价于隔离环境里的 `$HOME/.pace-forge`）。
    pub(crate) fn data(&self) -> PathBuf {
        crate::api::session::data_dir_in(self.home())
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// 真实 `$HOME` 基目录（生产默认值）。护栏测试用它做对照。
pub(crate) fn real_home() -> PathBuf {
    crate::api::session::home_dir()
}

/// 真实数据目录 `$HOME/.pace-forge`，**只算路径、不碰磁盘**。
pub(crate) fn real_data_dir() -> PathBuf {
    crate::api::session::data_dir_in(&real_home())
}

/// 护栏断言：`injected` 必须是「临时目录下的一次性目录」，且不落在真实
/// `$HOME/.pace-forge` 里。
///
/// 每个模块各写一条这样的测试，防止将来有人把 `*_in` 改回默认路径。
pub(crate) fn assert_isolated(injected: &Path, what: &str) {
    let real = real_data_dir();
    assert_ne!(
        injected,
        real,
        "{what} 不能等于真实 $HOME/.pace-forge"
    );
    assert!(
        !injected.starts_with(&real),
        "{what} 不能落在真实 $HOME/.pace-forge 之下: {}",
        injected.display()
    );
    assert!(
        injected.starts_with(std::env::temp_dir()),
        "{what} 应在临时目录下: {}",
        injected.display()
    );
    assert!(
        injected.to_string_lossy().contains("pace-forge-test-"),
        "{what} 应带一次性前缀: {}",
        injected.display()
    );
}
