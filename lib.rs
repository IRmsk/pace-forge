// 提交报文有 45 个字段，json! 宏默认递归上限不够
#![recursion_limit = "512"]

//! 运动世界校园版协议工具。
//!
//! 分层：
//! - [`crypto`]  NetSecKit 加密（已对真实服务器验证）
//! - [`api`]     请求组装与业务接口
//! - [`web`]     Web 控制台（登录 / 记录 / 轨迹地图）
//! - [`track`]   轨迹生成与 10 秒分桶
//! - `cli`       命令行（待实现）

pub mod api;
pub mod crypto;
pub mod track;
pub mod web;

/// 协议层版本。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
