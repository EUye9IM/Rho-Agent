//! rho — 轻量本地 agent REPL（库入口）。
//!
//! 集成测试（tests/）经此访问 crate 内部；bin 只是壳。
//! 模块布局见 architecture.md 二节；依赖方向单向向下：
//! repl → chat → provider → sse；config 被所有层读，不反向依赖。

pub mod chat;
pub mod config;
pub mod provider;
pub mod render;
pub mod repl;
pub mod sse;
pub mod term;
pub mod tools;
pub mod ui;
