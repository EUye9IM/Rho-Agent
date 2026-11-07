//! 斜杠命令：元数据、解析、执行与补全候选。
//! 命令执行只修改 `Session` 并返回 `CommandEffect`，输出由 REPL 通过 Ui 完成。

use crate::chat::Session;
use crate::completion::{Candidate, Completion, CompletionProvider};
use crate::config;
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    Help,
    Model,
    Provider,
    Thinking,
    Clear,
    Exit,
}

struct CommandSpec {
    command: Command,
    name: &'static str,
    usage: &'static str,
    description: &'static str,
}

const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        command: Command::Model,
        name: "/model",
        usage: "/model [id]",
        description: "切换或列出当前供应商的模型",
    },
    CommandSpec {
        command: Command::Provider,
        name: "/provider",
        usage: "/provider [id]",
        description: "切换或列出供应商",
    },
    CommandSpec {
        command: Command::Thinking,
        name: "/thinking",
        usage: "/thinking [level]",
        description: "切换或列出思考档位",
    },
    CommandSpec {
        command: Command::Clear,
        name: "/clear",
        usage: "/clear",
        description: "清空对话历史",
    },
    CommandSpec {
        command: Command::Help,
        name: "/help",
        usage: "/help",
        description: "显示本帮助",
    },
    CommandSpec {
        command: Command::Exit,
        name: "/exit",
        usage: "/exit",
        description: "退出",
    },
];

/// 命令执行结果：REPL 负责把文本交给 Ui 渲染。
#[derive(Debug, PartialEq, Eq)]
pub enum CommandEffect {
    /// 继续 REPL，并展示给定文本。
    Continue(String),
    /// 正常退出。
    Exit,
}

/// 解析并执行一条斜杠命令。
pub fn execute(session: &mut Session, line: &str) -> Result<CommandEffect, String> {
    let mut parts = line.split_whitespace();
    let name = parts
        .next()
        .ok_or_else(|| "未知命令：（输入 /help 查看帮助）".to_string())?;
    let arg = parts.collect::<Vec<_>>().join(" ");
    let spec = COMMANDS
        .iter()
        .find(|spec| spec.name == name)
        .ok_or_else(|| format!("未知命令：{name}（输入 /help 查看帮助）"))?;

    match spec.command {
        Command::Help => Ok(CommandEffect::Continue(help_text(session))),
        Command::Model => model(session, &arg),
        Command::Provider => provider(session, &arg),
        Command::Thinking => thinking(session, &arg),
        Command::Clear => {
            session.clear();
            Ok(CommandEffect::Continue("已清空对话历史".into()))
        }
        Command::Exit => Ok(CommandEffect::Exit),
    }
}

fn help_text(session: &Session) -> String {
    let mut lines = vec!["命令：".to_string()];
    for spec in COMMANDS {
        if spec.command == Command::Thinking && session.selection.model.thinking.is_empty() {
            continue;
        }
        lines.push(format!("  {:<16} {}", spec.usage, spec.description));
    }
    lines.join("\n")
}

fn model(session: &mut Session, arg: &str) -> Result<CommandEffect, String> {
    if arg.is_empty() {
        let provider_id = &session.selection.provider_id;
        let mut lines = vec![format!("供应商 {provider_id} 的可用模型：")];
        for (id, _) in config::models_in_order(&session.cfg) {
            if config::model_provider_prefix(id) != Some(provider_id.as_str()) {
                continue;
            }
            let mark = if id == &session.selection.model_id {
                "*"
            } else {
                " "
            };
            lines.push(format!("  {mark} {id}"));
        }
        return Ok(CommandEffect::Continue(lines.join("\n")));
    }

    session.switch_model(arg)?;
    Ok(CommandEffect::Continue(format!(
        "已切换到模型 {}",
        session.selection.model_id
    )))
}

fn provider(session: &mut Session, arg: &str) -> Result<CommandEffect, String> {
    if arg.is_empty() {
        let mut lines = vec!["可用供应商：".to_string()];
        for (id, _) in config::providers_in_order(&session.cfg) {
            let mark = if id == &session.selection.provider_id {
                "*"
            } else {
                " "
            };
            let status = match config::provider_availability(&session.cfg, id) {
                Ok(()) => String::new(),
                Err(e) => format!("（不可用：{e}）"),
            };
            lines.push(format!("  {mark} {id}{status}"));
        }
        return Ok(CommandEffect::Continue(lines.join("\n")));
    }

    session.switch_provider(arg)?;
    Ok(CommandEffect::Continue(format!(
        "已切换到供应商 {}，模型 {}",
        session.selection.provider_id, session.selection.model_id
    )))
}

fn thinking(session: &mut Session, arg: &str) -> Result<CommandEffect, String> {
    let levels: Vec<String> = session.selection.model.thinking.keys().cloned().collect();
    if arg.is_empty() {
        if levels.is_empty() {
            return Ok(CommandEffect::Continue("当前模型未声明思考档位".into()));
        }

        let mut lines = vec!["可用思考档位：".to_string()];
        for level in &levels {
            let mark = if session.selection.thinking.as_deref() == Some(level.as_str()) {
                "*"
            } else {
                " "
            };
            lines.push(format!("  {mark} {level}"));
        }
        return Ok(CommandEffect::Continue(lines.join("\n")));
    }

    session.switch_thinking(arg)?;
    Ok(CommandEffect::Continue(format!("思考档位：{arg}")))
}

/// 当前会话的补全快照；切换模型/供应商后由 REPL 重新生成。
struct CompletionData {
    providers: Vec<String>,
    /// 已按当前 provider 过滤。
    models: Vec<String>,
    /// 当前模型的 thinking 档位。
    thinking: Vec<String>,
    current_provider: String,
    current_model: String,
    current_thinking: Option<String>,
}

impl CompletionData {
    fn from_session(session: &Session) -> Self {
        let provider_id = session.selection.provider_id.clone();
        let providers = config::providers_in_order(&session.cfg)
            .map(|(id, _)| id.clone())
            .collect();
        let models = config::models_in_order(&session.cfg)
            .filter(|(id, _)| config::model_provider_prefix(id) == Some(provider_id.as_str()))
            .map(|(id, _)| id.clone())
            .collect();
        let thinking = session.selection.model.thinking.keys().cloned().collect();

        Self {
            providers,
            models,
            thinking,
            current_provider: provider_id,
            current_model: session.selection.model_id.clone(),
            current_thinking: session.selection.thinking.clone(),
        }
    }
}

struct CommandCompletion {
    data: CompletionData,
}

impl CompletionProvider for CommandCompletion {
    fn complete(&self, line: &str, pos: usize) -> Completion {
        complete_line(&self.data, line, pos)
    }
}

/// 按当前会话快照生成补全提供者；REPL 在每次 `read_line` 前调用。
pub fn completion_provider(session: &Session) -> Arc<dyn CompletionProvider> {
    Arc::new(CommandCompletion {
        data: CompletionData::from_session(session),
    })
}

fn complete_line(data: &CompletionData, line: &str, pos: usize) -> Completion {
    let Some(before) = line.get(..pos) else {
        return Completion::none(pos);
    };
    if !before.starts_with('/') {
        return Completion::none(pos);
    }
    // 光标后面还有内容时不补，避免替换范围吃掉行尾。
    if !line
        .get(pos..)
        .is_some_and(|rest| rest.chars().all(char::is_whitespace))
    {
        return Completion::none(pos);
    }

    let Some(space) = before.find(' ') else {
        return Completion {
            start: 0,
            candidates: command_candidates(data, before),
        };
    };

    let command = &before[..space];
    let prefix = &before[space + 1..];
    if prefix.contains(' ') {
        return Completion::none(pos);
    }

    let candidates = match command {
        "/model" => value_candidates(&data.models, prefix, Some(&data.current_model)),
        "/provider" => value_candidates(&data.providers, prefix, Some(&data.current_provider)),
        "/thinking" => value_candidates(&data.thinking, prefix, data.current_thinking.as_deref()),
        _ => Vec::new(),
    };
    Completion {
        start: space + 1,
        candidates,
    }
}

fn command_candidates(data: &CompletionData, prefix: &str) -> Vec<Candidate> {
    COMMANDS
        .iter()
        .filter(|spec| spec.command != Command::Thinking || !data.thinking.is_empty())
        .filter(|spec| spec.name.starts_with(prefix))
        .map(|spec| Candidate {
            display: format!("{}  {}", spec.usage, spec.description),
            replacement: format!("{} ", spec.name),
        })
        .collect()
}

fn value_candidates(values: &[String], prefix: &str, current: Option<&str>) -> Vec<Candidate> {
    values
        .iter()
        .filter(|value| value.starts_with(prefix))
        .map(|value| Candidate {
            display: if current == Some(value.as_str()) {
                format!("* {value}")
            } else {
                format!("  {value}")
            },
            replacement: value.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion(
        models: &[&str],
        thinking: &[&str],
        current_model: &str,
        current_thinking: Option<&str>,
    ) -> CommandCompletion {
        CommandCompletion {
            data: CompletionData {
                providers: vec!["mock".into(), "other".into()],
                models: models.iter().map(|s| (*s).to_string()).collect(),
                thinking: thinking.iter().map(|s| (*s).to_string()).collect(),
                current_provider: "mock".into(),
                current_model: current_model.into(),
                current_thinking: current_thinking.map(str::to_string),
            },
        }
    }

    fn replacements(completion: &Completion) -> Vec<&str> {
        completion
            .candidates
            .iter()
            .map(|c| c.replacement.as_str())
            .collect()
    }

    #[test]
    fn completes_commands_arguments_and_skips_hidden_thinking() {
        let c = completion(
            &["mock/a", "mock/b"],
            &["low", "high"],
            "mock/a",
            Some("low"),
        );

        let command = c.complete("/he", 3);
        assert_eq!(command.start, 0);
        assert_eq!(replacements(&command), vec!["/help "]);

        let models = c.complete("/model ", 7);
        assert_eq!(models.start, 7);
        assert_eq!(replacements(&models), vec!["mock/a", "mock/b"]);

        let thinking = c.complete("/thinking ", 10);
        assert_eq!(thinking.start, 10);
        assert_eq!(replacements(&thinking), vec!["low", "high"]);

        let no_thinking = completion(&["mock/a"], &[], "mock/a", None);
        let all = no_thinking.complete("/", 1);
        let commands = replacements(&all);
        assert!(commands.contains(&"/help "));
        assert!(!commands.contains(&"/thinking "));
    }

    #[test]
    fn completion_follows_model_snapshot() {
        let first = completion(&["mock/a"], &["low", "high"], "mock/a", Some("low"));
        assert_eq!(
            replacements(&first.complete("/thinking ", 10)),
            vec!["low", "high"]
        );

        let second = completion(&["mock/b"], &["medium"], "mock/b", Some("medium"));
        assert_eq!(
            replacements(&second.complete("/thinking ", 10)),
            vec!["medium"]
        );
    }

    #[test]
    fn completion_does_not_replace_text_after_cursor() {
        let c = completion(&["mock/a"], &["low"], "mock/a", Some("low"));
        assert!(c.complete("/model mock/a", 7).candidates.is_empty());
    }
}
