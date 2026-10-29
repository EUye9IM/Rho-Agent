//! rho — 轻量本地 agent REPL（库入口）。
//!
//! 集成测试（tests/）经此访问 crate 内部；bin 只是壳。
//! 模块布局与依赖方向（单向向下）：
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

use std::sync::OnceLock;

/// 会话 cwd 恒为进程启动目录（全程不可变）。
pub fn session_cwd() -> String {
    static CWD: OnceLock<String> = OnceLock::new();
    CWD.get_or_init(|| {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string())
    })
    .clone()
}
