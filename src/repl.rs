//! REPL 循环：斜杠命令、Ctrl-C 语义。循环本体依赖 Ui trait。见 design.md 四节。

use crate::chat::{ChatResult, Session, TurnEnd};
use crate::config;
use crate::render::RenderEvent;
use crate::term::{Cancel, SignalState};
use crate::ui::{ReadOutcome, Ui};

/// REPL 层的退出/致命错误（main 据此映射退出码 0/1）。
#[derive(Debug)]
pub enum ReplExit {
    /// /exit、空行 Ctrl-C、Ctrl-D（正常退出，exit 0）
    Quit,
    /// REPL 无法继续的内部错误（exit 1）
    Fatal(String),
}

/// REPL 主循环。
pub fn run(ui: &mut dyn Ui, mut session: Session, cancel: &Cancel) -> Result<(), ReplExit> {
    loop {
        // 上一轮清理期间记录的 quit：不等下一次输入，立即退出。
        if cancel.state() == SignalState::Quit {
            return Err(ReplExit::Quit);
        }

        let outcome = ui.read_line();
        check_fatal(ui)?;
        match outcome {
            ReadOutcome::Eof => return Ok(()),
            ReadOutcome::Interrupted { line_empty: true } => return Ok(()),
            ReadOutcome::Interrupted { line_empty: false } => {
                // rustyline 已清空编辑行；回到提示符。
                continue;
            }
            ReadOutcome::Line(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed.starts_with('/') {
                    let result = match handle_command(ui, &mut session, cancel, trimmed) {
                        Ok(r) => r,
                        Err(_) => return Err(ReplExit::Quit),
                    };
                    check_fatal(ui)?;
                    if cancel.state() == SignalState::Quit {
                        return Err(ReplExit::Quit);
                    }
                    match result {
                        CommandResult::Continue => continue,
                        CommandResult::Exit => return Ok(()),
                    }
                }

                if advance_epoch(cancel) {
                    return Err(ReplExit::Quit);
                }
                let turn = session.turn(ui, cancel, trimmed);
                check_fatal(ui)?;
                // turn 内部任何返回路径都不应把 pending quit 丢给下一次输入。
                if cancel.state() == SignalState::Quit {
                    return Err(ReplExit::Quit);
                }
                match turn {
                    Ok(TurnEnd::Done) | Ok(TurnEnd::Error(_)) | Ok(TurnEnd::Cancelled) => {}
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

/// 斜杠命令处理：/model /provider /thinking /clear /help /exit。
/// 不带参数列出可选项并标注当前值。切换只影响后续请求，不重放历史。
fn handle_command(
    ui: &mut dyn Ui,
    session: &mut Session,
    _cancel: &Cancel,
    line: &str,
) -> ChatResult<CommandResult> {
    let mut parts = line.split_whitespace();
    let command = parts.next().unwrap_or("");
    let arg = parts.collect::<Vec<_>>().join(" ");

    match command {
        "/exit" => Ok(CommandResult::Exit),
        "/clear" => {
            session.clear();
            text(ui, "已清空对话历史");
            Ok(CommandResult::Continue)
        }
        "/help" => {
            let mut lines = vec![
                "命令：".to_string(),
                "  /model [id]      切换或列出当前供应商的模型".to_string(),
                "  /provider [id]  切换或列出供应商".to_string(),
            ];
            if !session.selection.model.thinking.is_empty() {
                lines.push("  /thinking [level] 切换或列出思考档位".to_string());
            }
            lines.push("  /clear           清空对话历史".to_string());
            lines.push("  /help            显示本帮助".to_string());
            lines.push("  /exit            退出".to_string());
            text(ui, &lines.join("\n"));
            Ok(CommandResult::Continue)
        }
        "/model" => {
            if arg.is_empty() {
                let provider_id = &session.selection.provider_id;
                let mut lines = vec![format!("供应商 {provider_id} 的可用模型：")];
                for (id, _) in config::models_in_order(&session.cfg) {
                    if config::model_provider_prefix(id) != Some(provider_id.as_str()) {
                        continue;
                    }
                    let mark = if id == &session.selection.model_id { "*" } else { " " };
                    lines.push(format!("  {mark} {id}"));
                }
                text(ui, &lines.join("\n"));
            } else if let Err(e) = session.switch_model(&arg) {
                emit_error(ui, e);
            } else {
                text(ui, &format!("已切换到模型 {}", session.selection.model_id));
            }
            Ok(CommandResult::Continue)
        }
        "/provider" => {
            if arg.is_empty() {
                let mut lines = vec!["可用供应商：".to_string()];
                for (id, _) in config::providers_in_order(&session.cfg) {
                    let mark = if id == &session.selection.provider_id { "*" } else { " " };
                    let status = match config::provider_availability(&session.cfg, id) {
                        Ok(()) => String::new(),
                        Err(e) => format!("（不可用：{e}）"),
                    };
                    lines.push(format!("  {mark} {id}{status}"));
                }
                text(ui, &lines.join("\n"));
            } else if let Err(e) = session.switch_provider(&arg) {
                emit_error(ui, e);
            } else {
                text(
                    ui,
                    &format!(
                        "已切换到供应商 {}，模型 {}",
                        session.selection.provider_id, session.selection.model_id
                    ),
                );
            }
            Ok(CommandResult::Continue)
        }
        "/thinking" => {
            let levels: Vec<String> = session
                .selection
                .model
                .thinking
                .keys()
                .cloned()
                .collect();
            if arg.is_empty() {
                if levels.is_empty() {
                    text(ui, "当前模型未声明思考档位");
                } else {
                    let mut lines = vec!["可用思考档位：".to_string()];
                    for level in &levels {
                        let mark = if session.selection.thinking.as_deref() == Some(level.as_str()) {
                            "*"
                        } else {
                            " "
                        };
                        lines.push(format!("  {mark} {level}"));
                    }
                    text(ui, &lines.join("\n"));
                }
            } else if let Err(e) = session.switch_thinking(&arg) {
                emit_error(ui, e);
            } else {
                text(ui, &format!("思考档位：{arg}"));
            }
            Ok(CommandResult::Continue)
        }
        other => {
            emit_error(ui, format!("未知命令：{other}（输入 /help 查看帮助）"));
            Ok(CommandResult::Continue)
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

/// 切轮：活动操作开始前调用（流式请求/回合开始）。
/// prev_quit 已置位 → 退出；否则清空当前轮记录。
fn advance_epoch(cancel: &Cancel) -> bool {
    cancel.advance_epoch()
}

/// 会话 cwd 展示（恒为进程启动目录）。
pub fn session_cwd() -> String {
    crate::session_cwd()
}
