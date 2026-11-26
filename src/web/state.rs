//! 控制台状态：账号库、每账号独立 Client、当前选中账号。
//!
//! 会话状态只存一份 `Client`（含 uid/token/unid），进程退出即失效；
//! 不落盘密码。落盘的只有账号库（accounts.json），恢复逻辑见
//! `State::restore`。

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::api::client::{Client, DeviceProfile};
use crate::api::login::UserProfile;
use crate::api::{accounts, session};

pub(crate) type Shared = Arc<Mutex<State>>;

pub(crate) struct State {
    /// 账号库（落盘）。
    pub(crate) book: crate::api::accounts::Book,
    /// 每个账号一个独立 Client，token 互不干扰。
    pub(crate) clients: std::collections::HashMap<String, Client>,
    /// 当前选中的账号 id。
    pub(crate) current: Option<String>,
}

pub(crate) enum AccountSessionAction {
    Logout,
    Delete,
}

impl State {
    #[allow(dead_code)]
    pub(crate) fn new() -> Self {
        Self {
            book: crate::api::accounts::Book::default(),
            clients: std::collections::HashMap::new(),
            current: None,
        }
    }

    /// 从磁盘恢复所有账号的登录态。
    ///
    /// 两条来源都要看：登录成功时会写 `session.json`，但 `accounts.json` 里那一
    /// 条不一定已经跟上（uid/token 为空）。只看 book 的话，重启一次就掉登录，
    /// 用户得重新过一次极验——而 session.json 里明明有完整 token。
    pub(crate) fn restore() -> Self {
        let book = restore_account_book(&session::data_dir());
        let mut clients = std::collections::HashMap::new();
        for a in &book.accounts {
            if a.logged_in() {
                clients.insert(a.id.clone(), make_client(&a.uid, &a.token, &a.unid));
            }
        }
        let current = book
            .accounts
            .iter()
            .find(|a| a.logged_in())
            .map(|a| a.id.clone());
        if let Some(id) = &current {
            println!("[web] 恢复 {} 个账号，当前={id}", clients.len());
        }
        Self {
            book,
            clients,
            current,
        }
    }

    /// 改了设备字段（机型）之后重建这个账号的 client：下一次请求就带新 cpuModel。
    ///
    /// 只重建 clients 里的条目，不动 book 里的凭据——设备字段和登录态是两回事。
    pub(crate) fn rebuild_client(&mut self, uid: &str) {
        let found = self
            .book
            .accounts
            .iter()
            .find(|a| a.id == uid || a.username == uid)
            .map(|a| (a.id.clone(), a.uid.clone(), a.token.clone(), a.unid.clone()));
        if let Some((id, u, t, un)) = found {
            self.clients.insert(id, make_client(&u, &t, &un));
        }
    }

    /// 全部账号重建 client（改出口后调用）。返回重建个数。
    pub(crate) fn rebuild_all_clients(&mut self) -> usize {
        let list: Vec<(String, String, String, String)> = self
            .book
            .accounts
            .iter()
            .map(|a| (a.id.clone(), a.uid.clone(), a.token.clone(), a.unid.clone()))
            .collect();
        let mut n = 0;
        for (id, u, t, un) in list {
            self.clients.insert(id, make_client(&u, &t, &un));
            n += 1;
        }
        n
    }

    /// 当前账号。
    pub(crate) fn current_account(&self) -> Option<&crate::api::accounts::Account> {
        self.current.as_ref().and_then(|id| self.book.get(id))
    }

    /// 当前账号是否可发请求。
    pub(crate) fn logged_in(&self) -> bool {
        self.current_account()
            .map(|a| a.logged_in())
            .unwrap_or(false)
    }

    /// 当前账号的 Client。
    pub(crate) fn client(&self) -> Option<&Client> {
        self.current.as_ref().and_then(|id| self.clients.get(id))
    }

    /// 某账号的 Client。
    pub(crate) fn client_of(&self, id: &str) -> Option<&Client> {
        self.clients.get(id)
    }

    /// 用新登录结果更新一个账号。
    pub(crate) fn apply_login(&mut self, id: &str, p: &UserProfile) {
        if let Some(a) = self.book.get_mut(id) {
            a.uid = p.uid.clone();
            a.token = p.token.clone();
            a.unid = p.unid.clone();
            a.name = p.name.clone();
            a.school = p.campus_name.clone();
            a.last_login_ms = crate::crypto::envelope::now_ms() as i64;
            // 能登录成功就说明凭据有效，标记启用；否则 login-all 会一直跳过它
            a.enabled = true;
        }
        self.clients
            .insert(id.to_string(), make_client(&p.uid, &p.token, &p.unid));
        self.current = Some(id.to_string());
        let _ = crate::api::accounts::save(&self.book);
        // 同步写一份 session.json，让 CLI 也能直接用（原来只有 Web 有登录态，
        // CLI 永远报「未登录」）。
        let sess = crate::api::session::Session {
            username: self
                .book
                .get(id)
                .map(|account| account.username.clone())
                .unwrap_or_default(),
            uid: p.uid.clone(),
            token: p.token.clone(),
            unid: p.unid.clone(),
            name: p.name.clone(),
            campus_name: p.campus_name.clone(),
            depart: p.depart.clone(),
            grade_class: p.grade_class.clone(),
            ..Default::default()
        };
        let _ = crate::api::session::save_session(&sess);
    }

    pub(crate) fn persist(&self) {
        if let Err(e) = crate::api::accounts::save(&self.book) {
            eprintln!("保存账号库失败: {e}");
        }
    }

    pub(crate) fn end_account_session(
        &mut self,
        dir: &Path,
        id: &str,
        action: AccountSessionAction,
    ) -> Result<bool, String> {
        let mut book = self.book.clone();
        match action {
            AccountSessionAction::Logout => {
                let account = book.get_mut(id).ok_or("账号不存在")?;
                account.token.clear();
                account.uid.clear();
                account.unid.clear();
            }
            AccountSessionAction::Delete => {
                if !book.remove(id) {
                    return Err("账号不存在".into());
                }
            }
        }
        // 先清理会话，再保存账号库，避免 Web 重启时恢复已退出或删除的账号。
        session::clear_session_if_below(dir, |cached| {
            session_owner(&self.book, cached)
                .is_some_and(|index| self.book.accounts[index].id == id)
        })?;
        accounts::save_in_dir(dir, &book)?;

        self.book = book;
        self.clients.remove(id);
        let was_current = self.current.as_deref() == Some(id);
        if was_current {
            self.current = None;
        }
        Ok(was_current)
    }
}

pub(crate) fn restore_account_book(dir: &Path) -> accounts::Book {
    let mut book = accounts::load_in_dir(dir);
    let cached = session::load_session_below(dir);
    if !cached.is_valid() {
        return book;
    }
    if let Some(index) = session_owner(&book, &cached) {
        let account = &mut book.accounts[index];
        if !account.logged_in() {
            account.uid = cached.uid;
            account.token = cached.token;
            account.unid = cached.unid;
        }
    } else {
        // 旧版本可能只存了 session.json，补回账号行，密码留空。
        book.accounts.push(accounts::Account {
            id: cached.uid.clone(),
            username: cached.username,
            uid: cached.uid,
            token: cached.token,
            unid: cached.unid,
            name: cached.name,
            school: cached.campus_name,
            enabled: true,
            ..Default::default()
        });
    }
    book
}

pub(crate) fn session_owner(book: &accounts::Book, cached: &session::Session) -> Option<usize> {
    if !cached.is_valid() {
        return None;
    }
    book.accounts
        .iter()
        .position(|account| account.id == cached.uid || account.uid == cached.uid)
        .or_else(|| {
            book.accounts.iter().position(|account| {
                account.uid.is_empty()
                    && !cached.username.is_empty()
                    && account.username == cached.username
            })
        })
}

/// 对某个账号发一次服务端退出，失败也返回 false 而不是报错。
///
/// 优先用内存里的 client；没有（进程刚重启、或者这个账号还没被激活过）
/// 就用存盘的 uid/token 临时建一个，只为发这一个请求。
///
/// 这一步不能省。重启后直接删账号时内存里没有 client，如果就此跳过，
/// 服务端会话仍然活着，账号在服务端还是「已登录」——下次登录 10121 并
/// 锁一小时。用户视角就是「我明明删了，怎么还锁我」。
pub(crate) fn remote_logout(state: &mut State, id: &str) -> bool {
    if let Some(client) = state.clients.get_mut(id) {
        return crate::api::login::logout(client).unwrap_or(false);
    }
    let Some(account) = state.book.get(id).filter(|account| account.logged_in()) else {
        return false;
    };
    let mut client = make_client(&account.uid, &account.token, &account.unid);
    crate::api::login::logout(&mut client).unwrap_or(false)
}

pub(crate) fn make_client(uid: &str, token: &str, unid: &str) -> Client {
    // 每个账号独立的 device_id，避免被按设备聚合。
    //
    // 作用域必须和登录时完全一致：登录前不知道 uid，用的是手机号。
    // 之前这里传 uid、登录时传手机号，同一个账号会算出两套 device_id，
    // 服务端当成两台手机，直接触发 10121「此账号已在其他设备登录」并把
    // 账号锁一小时。
    let mut c = Client::new(DeviceProfile::for_account(&device_scope(uid)));
    c.creds = crate::api::client::Credentials {
        uid: Some(uid.to_string()),
        token: Some(token.to_string()),
        unid: Some(unid.to_string()),
    };
    c
}

/// 设备作用域：一个账号有且只有一套 device_id。
///
/// 账号库里 id 是 uid、username 是手机号。登录前后拿到的标识不同，但必须
/// 映射到同一个作用域，否则就是「换手机登录」。
pub(crate) fn device_scope(id_or_username: &str) -> String {
    let book = crate::api::accounts::load();
    // id 命中就直接用 username，保证和登录时一致
    if let Some(a) = book.accounts.iter().find(|a| a.id == id_or_username)
        && !a.username.is_empty()
    {
        return a.username.clone();
    }
    // 传进来本来就是手机号
    id_or_username.to_string()
}

/// 取全局状态锁，**中毒也恢复**。
///
/// 默认的 `lock()` 在某次持锁线程 panic 之后会永久返回 `Err`——`PoisonError`
/// 是粘的，之后每一个请求都跟着失败，整个控制台只能重启。对一个本机单进程
/// 控制台来说，"数据可能不一致" 远好过 "必须重启"，所以这里强制取回内层值
/// 并留一行日志，至少让现象可见。
pub(crate) fn lock_state(state: &Shared) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| {
        eprintln!("! [state] 状态锁已中毒（有线程持锁时 panic 过），强制恢复继续");
        e.into_inner()
    })
}

#[cfg(test)]
mod account_session_tests {
    use super::*;
    use crate::test_support::{TestDir, assert_isolated};

    fn saved_account(id: &str, uid: &str, username: &str) -> accounts::Account {
        accounts::Account {
            id: id.into(),
            uid: uid.into(),
            username: username.into(),
            token: format!("test-key-{id}"),
            unid: "test-unid".into(),
            password_enc: "saved-password".into(),
            name: "同名测试账号".into(),
            enabled: true,
            ..Default::default()
        }
    }

    fn fixture(name: &str) -> (TestDir, State) {
        let temp = TestDir::new(name);
        let dir = temp.data();
        assert_isolated(&dir, "Web 账号会话");
        let mut state = State::new();
        state.book.upsert(saved_account("first", "uid-a", "user-a"));
        state
            .book
            .upsert(saved_account("second", "uid-b", "user-b"));
        state.current = Some("first".into());
        accounts::save_in_dir(&dir, &state.book).unwrap();
        (temp, state)
    }

    fn cache_session(dir: &Path, uid: &str, username: &str) {
        let cached = session::Session {
            uid: uid.into(),
            username: username.into(),
            token: "test-cached-key".into(),
            unid: "test-cached-unid".into(),
            name: "同名测试账号".into(),
            ..Default::default()
        };
        session::save_session_below(dir, &cached).unwrap();
    }

    #[test]
    fn reloading_after_logout_keeps_saved_account_without_login() {
        let (temp, mut state) = fixture("web-logout-restart");
        let dir = temp.data();
        // 模拟旧版 Web 只在会话文件里保存了 UID 和令牌。
        cache_session(&dir, "uid-a", "");

        assert!(
            state
                .end_account_session(&dir, "first", AccountSessionAction::Logout)
                .unwrap()
        );
        assert!(state.current.is_none());
        assert!(!dir.join("session.json").exists());

        let restored = restore_account_book(&dir);
        let first = restored.get("first").unwrap();
        assert!(!first.logged_in());
        assert!(first.unid.is_empty());
        assert_eq!(first.password_enc, "saved-password");
        assert!(restored.get("second").unwrap().logged_in());
    }

    #[test]
    fn reloading_after_delete_does_not_resurrect_the_account() {
        let (temp, mut state) = fixture("web-delete-restart");
        let dir = temp.data();
        cache_session(&dir, "uid-b", "user-b");

        assert!(
            !state
                .end_account_session(&dir, "second", AccountSessionAction::Delete)
                .unwrap()
        );
        assert_eq!(state.current.as_deref(), Some("first"));
        assert!(!dir.join("session.json").exists());

        let restored = restore_account_book(&dir);
        assert!(restored.get("first").unwrap().logged_in());
        assert_eq!(restored.accounts.len(), 1);
        assert!(restored.get("second").is_none());
    }

    #[test]
    fn logout_preserves_a_legacy_session_until_its_owner_logs_out() {
        let (temp, mut state) = fixture("web-logout-other-session");
        let dir = temp.data();
        let second = state.book.get_mut("second").unwrap();
        second.uid.clear();
        second.token.clear();
        cache_session(&dir, "uid-b", "user-b");

        state
            .end_account_session(&dir, "first", AccountSessionAction::Logout)
            .unwrap();

        let restored = restore_account_book(&dir);
        assert!(!restored.get("first").unwrap().logged_in());
        assert!(restored.get("second").unwrap().logged_in());
        assert_eq!(session::load_session_below(&dir).uid, "uid-b");

        state
            .end_account_session(&dir, "second", AccountSessionAction::Logout)
            .unwrap();
        assert!(!dir.join("session.json").exists());
        assert!(
            !restore_account_book(&dir)
                .get("second")
                .unwrap()
                .logged_in()
        );
    }

    #[test]
    fn malformed_session_is_preserved_and_does_not_prevent_logout() {
        let (temp, mut state) = fixture("web-logout-corrupt-session");
        let dir = temp.data();
        let path = dir.join("session.json");
        std::fs::write(&path, b"\xffmalformed").unwrap();

        state
            .end_account_session(&dir, "first", AccountSessionAction::Logout)
            .unwrap();

        assert_eq!(std::fs::read(path).unwrap(), b"\xffmalformed");
        assert!(!restore_account_book(&dir).get("first").unwrap().logged_in());
    }

    #[test]
    fn session_io_error_leaves_account_state_unchanged() {
        let (temp, mut state) = fixture("web-logout-session-error");
        let dir = temp.data();
        std::fs::create_dir(dir.join("session.json")).unwrap();

        assert!(
            state
                .end_account_session(&dir, "first", AccountSessionAction::Logout)
                .is_err()
        );
        assert!(state.book.get("first").unwrap().logged_in());
        assert!(
            accounts::load_in_dir(&dir)
                .get("first")
                .unwrap()
                .logged_in()
        );
    }
}

#[cfg(test)]
mod device_scope_tests {
    use super::device_scope;

    /// 登录前后必须算出同一个设备作用域。
    ///
    /// 登录前只知道手机号、登录后只知道 uid。两者若映射到不同 scope，
    /// 就会生成两套 device_id，服务端当成两台手机，触发 10121
    /// 「此账号已在其他设备登录」并把账号锁一小时。
    #[test]
    fn device_scope_is_stable_across_login() {
        assert_eq!(device_scope("13900000000"), "13900000000");
        assert_eq!(device_scope("99999"), "99999");
        for _ in 0..3 {
            assert_eq!(device_scope("13900000000"), device_scope("13900000000"));
        }
    }
}
