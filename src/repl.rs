//! REPL 循环：读取输入、处理斜杠命令、判定退出与 Ctrl-C 双按窗口。循环本体依赖 Ui trait。

use crate::chat::{Session, TurnEnd};
use crate::commands::{self, CommandEffect};
use crate::render::RenderEvent;
use crate::term::Cancel;
use crate::ui::{ReadOutcome, Ui, CTRL_C_WINDOW};
use std::time::Instant;

/// REPL 层的退出/致命错误（main 据此映射退出码 0/1）。
#[derive(Debug)]
pub enum ReplExit {
    /// /exit、空行 Ctrl-C 双按、Ctrl-D（正常退出，exit 0）
    Quit,
    /// REPL 无法继续的内部错误（exit 1）
    Fatal(String),
}

/// REPL 主循环。
pub fn run(ui: &mut dyn Ui, mut session: Session, cancel: &Cancel) -> Result<(), ReplExit> {
    // 上一次空行 Ctrl-C 的时刻；仅在本窗口内再按一次才退出。
    let mut ctrl_c_armed: Option<Instant> = None;
    loop {
        // 上一轮清理期间记录的 quit：不等下一次输入，立即退出。
        if cancel.is_quit() {
            return Err(ReplExit::Quit);
        }

        let outcome = ui.read_line(commands::completion_provider(&session));
        check_fatal(ui)?;
        match outcome {
            ReadOutcome::Eof => {
                ui.set_ctrl_c_hint(false);
                return Ok(());
            }
            ReadOutcome::Interrupted { line_empty: false } => {
                // rustyline 已清空编辑行；用户还在输入，撤销武装。
                ctrl_c_armed = None;
                ui.set_ctrl_c_hint(false);
                continue;
            }
            ReadOutcome::Interrupted { line_empty: true } => {
                let now = Instant::now();
                if ctrl_c_armed.is_some_and(|armed| within_window(armed, now)) {
                    ui.set_ctrl_c_hint(false);
                    return Ok(());
                }
                // 第一次：武装并在提示符行尾显示灰提示，等第二次。
                ctrl_c_armed = Some(now);
                ui.set_ctrl_c_hint(true);
                continue;
            }
            ReadOutcome::Line(line) => {
                ctrl_c_armed = None;
                ui.set_ctrl_c_hint(false);
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed.starts_with('/') {
                    let result = handle_command(ui, &mut session, trimmed);
                    check_fatal(ui)?;
                    if cancel.is_quit() {
                        return Err(ReplExit::Quit);
                    }
                    match result {
                        CommandResult::Continue => continue,
                        CommandResult::Exit => return Ok(()),
                    }
                }

                // 切轮：旧轮未消费的 quit 结转到下一轮。
                if cancel.advance_epoch() {
                    return Err(ReplExit::Quit);
                }
                let turn = session.turn(ui, cancel, trimmed);
                check_fatal(ui)?;
                // turn 内部任何返回路径都不应把 pending quit 丢给下一次输入。
                if cancel.is_quit() {
                    return Err(ReplExit::Quit);
                }
                match turn {
                    // 正常回复最后一段通常不带换行；补一个 LineBreak，避免下一次
                    // rustyline 刷新提示符时用 `\r\x1b[K` 擦掉最后一行。
                    Ok(TurnEnd::Done) => {
                        ui.print(&RenderEvent::LineBreak);
                        check_fatal(ui)?;
                    }
                    Ok(TurnEnd::Error(_)) | Ok(TurnEnd::Cancelled) => {}
                    Ok(TurnEnd::Fatal(e)) => return Err(ReplExit::Fatal(e)),
                    Err(_) => return Err(ReplExit::Quit),
                }
            }
        }
    }
}

fn check_fatal(ui: &mut dyn Ui) -> Result<(), ReplExit> {
    match ui.take_fatal() {
        Some(e) => Err(ReplExit::Fatal(e)),
        None => Ok(()),
    }
}

/// 两次空行 Ctrl-C 的间隔是否仍在双按窗口内。
fn within_window(armed: Instant, now: Instant) -> bool {
    now.saturating_duration_since(armed) < CTRL_C_WINDOW
}

/// 斜杠命令处理：解析与执行委托给 `commands`，这里只负责把结果转成 UI 输出。
fn handle_command(ui: &mut dyn Ui, session: &mut Session, line: &str) -> CommandResult {
    match commands::execute(session, line) {
        Ok(CommandEffect::Exit) => CommandResult::Exit,
        Ok(CommandEffect::Continue(message)) => {
            text(ui, &message);
            CommandResult::Continue
        }
        Err(e) => {
            emit_error(ui, e);
            CommandResult::Continue
        }
    }
}

fn text(ui: &mut dyn Ui, message: &str) {
    ui.print(&RenderEvent::TextDelta(format!("{message}\n")));
}

fn emit_error(ui: &mut dyn Ui, message: impl Into<String>) {
    ui.print(&RenderEvent::Error(message.into()));
}

enum CommandResult {
    Continue,
    Exit,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn window_covers_exactly_two_seconds() {
        let armed = Instant::now();
        assert!(within_window(armed, armed));
        assert!(within_window(
            armed,
            armed + CTRL_C_WINDOW - Duration::from_millis(1)
        ));
        assert!(!within_window(armed, armed + CTRL_C_WINDOW));
        assert!(!within_window(armed, armed + Duration::from_secs(30)));
    }
}
