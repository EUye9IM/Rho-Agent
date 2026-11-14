//! rho — 轻量本地 agent REPL（库入口）。
//!
//! 集成测试（tests/）经此访问 crate 内部；bin 只是壳。
//! 模块布局与依赖方向（单向向下）：
//! repl → commands/chat/ui；commands → chat/completion，chat → provider → sse/message，
//! ui → completion/render；config 被所有层读，不反向依赖。

pub mod chat;
pub mod commands;
pub mod completion;
pub mod config;
pub mod message;
pub mod provider;
pub mod render;
pub mod repl;
pub mod sse;
pub mod term;
pub mod tools;
pub mod ui;
