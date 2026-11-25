//! 生产实现：rustyline 后端的 `TermUi`。
//!
//! - `stdin 非 TTY 拒绝启动`的检查在 `TermUi::new` 里做
//! - REPL 输入态 Ctrl-C 由 rustyline 返回 Interrupted（其原始模式清 ISIG，
//!   信号线程只处理活动操作，不会双重计数）
//! - 空行 Ctrl-C 的灰色双按提示由 helper 提供，到期由 `rho-hint` 线程擦掉
//! - 每次 `readline` 收尾都会被 rustyline 的换行钉死一行（输入长时多行）：Ctrl-C 后
//!   下一次读取前把这些行收回重用，输入过的内容也一并清掉，不另起新行

mod helper;
mod hint;
mod scripted;
mod term;

pub use hint::CTRL_C_WINDOW;
pub use scripted::ScriptedUi;
pub use term::{TermUi, TermUiInitError};

use crate::completion::CompletionProvider;
use crate::render::RenderEvent;
/// 提示符（定义在 `render`，与重放共用同一份文本）。
pub(crate) use crate::render::PROMPT;
use std::sync::Arc;

/// 读取一行输入的结果。
#[derive(Debug)]
pub enum ReadOutcome {
    /// 普通输入或斜杠命令（Ui 不识别命令）
    Line(String),
    /// Ctrl-C：编辑中（false）清空输入；空行（true）进入双按窗口
    /// —— 走 rustyline 按键路径，不经信号记录
    Interrupted { line_empty: bool },
    /// Ctrl-D
    Eof,
}

/// 用户交互抽象：读一行输入、渲染输出、Ctrl-C 双按提示开关与致命错误上报。
/// 退出语义由 REPL 层负责，Ui 不承载。
pub trait Ui {
    /// 读取一行输入。补全提供者按调用注入，生命周期仅限本次读取。
    fn read_line(&mut self, completer: Arc<dyn CompletionProvider>) -> ReadOutcome;
    fn print(&mut self, ev: &RenderEvent);

    /// Ctrl-C 双按提示开关：`true` 在提示符行尾显示灰色提示，`false` 立刻撤掉。
    /// 窗口到期由 Ui 自行撤掉；退出判定归 REPL 层。
    /// 默认实现无动作（测试 Ui 不必关心）。
    fn set_ctrl_c_hint(&mut self, _armed: bool) {}

    /// 渲染/读取 I/O 失败：REPL 据此映射 exit 1。
    /// 默认实现无错误（测试 Ui 不必关心）。
    fn take_fatal(&mut self) -> Option<String> {
        None
    }
}
