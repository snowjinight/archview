//! 高级搜索模块：语法解析/匹配引擎 + 搜索历史持久化。
//!
//! 语法说明见根目录 serach.md：空格 AND、| OR、! 排除、
//! "短语"、* ? 通配、<分组>（详见 engine.rs 头部注释）

pub mod engine;
pub mod history;

pub use engine::parse_query;
