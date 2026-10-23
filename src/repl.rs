//! REPL 循环：斜杠命令、Ctrl-C 语义。循环本体依赖 Ui trait。见 design.md 四节。

use crate::chat::{ChatResult, Session};
use crate::term::Cancel;
use crate::ui::{ReadOutcome, Ui};

/// REPL 层的退出/致命错误（main 据此映射退出码 0/1）。
pub enum ReplExit {
    /// /exit、空行 Ctrl-C、Ctrl-D（正常退出，exit 0）
    Quit,
    /// REPL 无法继续的内部错误（exit 1）
    Fatal(String),
}

/// REPL 主循环。
pub fn run(ui: &mut dyn Ui, mut session: Session, cancel: &Cancel) -> Result<(), ReplExit> {
    // TODO: loop {
    //   let outcome = ui.read_line();
    //   ReadOutcome::Eof | Interrupted{line_empty:true} → return Ok(ReplExit::Quit)
    //   Interrupted{line_empty:false} → 清空输入行（rustyline 已处理），continue
    //   Line(s) → 空行 continue；斜杠命令 → handle_command
    //            普通输入 → session.turn(...)：
    //     Ok(TurnEnd) → continue；Err(Quit) → return Quit（占位已由 turn 补齐）
    //   切轮前 cancel 切轮（prev_quit 结转；已置位则先退出）
    // }
    let _ = (ui, session, cancel);
    todo!()
}

/// 斜杠命令处理：/model /provider /thinking /clear /help /exit。
/// 不带参数列出可选项并标注当前值。切换只影响后续请求，不重放历史。
fn handle_command(
    ui: &mut dyn Ui,
    session: &mut Session,
    cancel: &Cancel,
    line: &str,
) -> ChatResult<CommandResult> {
    // TODO: 解析命令名与参数 → 分发：
    //   /exit → CommandResult::Exit
    //   /clear → session.clear()
    //   /model [id] / /provider [id] / /thinking [level] →
    //     有参：session.switch_*（失败则展示错误，状态不变）
    //     无参：列出可选项并标注当前值
    //   /help → 用法文本
    //   未知命令 → 提示
    let _ = (ui, session, cancel, line);
    todo!()
}

enum CommandResult {
    Continue,
    Exit,
}

/// 切轮：活动操作开始前调用（流式请求/回合开始）。
/// prev_quit 已置位 → 退出；否则清空当前轮记录。
fn advance_epoch(cancel: &Cancel) -> bool {
    // TODO: cancel.advance_epoch()：锁内 epoch+=1、旧轮 quit 结转 prev_quit、
    //       清空当前轮 records；返回 prev_quit（true = 应退出）
    let _ = cancel;
    todo!()
}

/// 会话 cwd 展示（恒为进程启动目录）。
pub fn session_cwd() -> String {
    // TODO: 进程启动时缓存一次 cwd（启动目录，全程不可变）
    todo!()
}
