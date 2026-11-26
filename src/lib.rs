// Tests panic on purpose; the workspace lints deny that only in production code.
#![cfg_attr(
    test,
    expect(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "assertions in tests"
    )
)]
// 提交报文有 45 个字段，json! 宏默认递归上限不够
#![recursion_limit = "512"]

//! 运动世界校园版协议工具。
//!
//! 分层：
//! - [`crypto`]  NetSecKit 加密（已对真实服务器验证）
//! - [`api`]     请求组装与业务接口
//! - [`web`]     Web 控制台（登录 / 记录 / 轨迹地图）
//! - [`track`]   轨迹生成与 10 秒分桶
//! - `cli`       命令行入口（`pace-forge help`，见 `main.rs`）

pub mod api;
pub mod crypto;
pub mod track;
pub mod web;

/// 配置层（`crates/pace-forge-config`）：`config.toml` + `PACE_*` 环境变量
/// 覆盖，启动时一次性加载校验。入口点（`main`）负责 `config::load` +
/// `config::store`，其余代码只读 [`config::global`]。
pub use pace_forge_config as config;

/// 测试专用的一次性隔离目录（只在 `cargo test` 下编译）。
///
/// 见模块注释：测试里绝不允许碰真实 `$HOME/.pace-forge`。
#[cfg(test)]
pub(crate) mod test_support;

/// 协议层版本。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Sums the even numbers in `xs`, skipping any that are divisible by 4.
///
/// Scratch code for a throwaway PR; never merged.
pub fn sum_even_not_four(xs: &[i64]) -> i64 {
    let mut total: i32 = 0;
    for x in xs {
        if x % 2 == 0 && x % 4 != 0 {
            total += x as i32;
        }
    }
    total as i64
}
