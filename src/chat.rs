//! 内部消息模型与 agent 工具循环。见 docs/architecture.md 三/四节。

use crate::config::{self, Config, Selection};
use crate::provider::{self, ProviderAdapter, StreamEvent};
use crate::render::RenderEvent;
use crate::term::{Cancel, SignalState};
use crate::tools::{self, BashEnd};
use crate::ui::{ConfirmOutcome, Ui};
use std::collections::HashSet;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_MAX_TOOL_STEPS: u32 = 25;

/// 与协议解耦的消息模型（适配层负责映射）。
#[derive(Debug, Clone)]
pub enum Msg {
    System(String),
    User(String),
    Assistant {
        text: Option<String>,
        reasoning: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

/// arguments 保持原始 JSON 字符串。
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// 会话状态：历史 + 当前选择 + 适配器 + 工具步数。
/// adapter 为 Arc 共享：会话长期持有，每轮 clone 一份按值传给 worker
/// （thread::spawn + detach 取消语义需 'static，不能借引用）。
/// thinking 唯一真值源是 selection.thinking（switch_* 同步更新）。
pub struct Session {
    pub cfg: Config,
    pub selection: Selection,
    pub adapter: Arc<dyn ProviderAdapter>,
    pub yes: bool,
    pub history: Vec<Msg>,
    /// 本轮工具步数（每次用户输入触发的一轮对话计数，turn 开头重置）
    pub steps: u32,
}

/// REPL → agent loop 的内部错误通道。
pub type ChatResult<T> = Result<T, Quit>;

/// 退出请求（REPL 层判定后传入；占位补齐与清理完成后上抛）。
#[derive(Debug)]
pub struct Quit;

/// 一轮回合的终态。
#[derive(Debug)]
pub enum TurnEnd {
    /// 正常完成（最终 assistant 无 tool_calls）
    Done,
    /// 轮内错误（已展示，保留已完成历史，回 REPL）
    Error(String),
    /// 用户取消（占位已补齐，回 REPL）
    Cancelled,
}

impl Session {
    /// thinking 已在 main 层校验并写入 selection.thinking（resolve_thinking，非法 exit 2）。
    /// history 以 cfg.system_prompt 播种（Msg::System；省略则不含 system 消息）。
    pub fn new(
        cfg: Config,
        selection: Selection,
        adapter: Arc<dyn ProviderAdapter>,
        yes: bool,
    ) -> Self {
        let history = match &cfg.system_prompt {
            Some(p) => vec![Msg::System(p.clone())],
            None => Vec::new(),
        };
        Self { cfg, selection, adapter, yes, history, steps: 0 }
    }

    /// `/clear`：历史重置为仅含 system 消息（播种语义同 new）；steps 归零。
    pub fn clear(&mut self) {
        self.history = match &self.cfg.system_prompt {
            Some(p) => vec![Msg::System(p.clone())],
            None => Vec::new(),
        };
        self.steps = 0;
    }

    fn build_body(&self) -> serde_json::Value {
        let thinking = self
            .selection
            .thinking
            .as_ref()
            .and_then(|level| self.selection.model.thinking.get(level));
        self.adapter.build_body(
            &self.history,
            &self.selection.model,
            &self.selection.model_id,
            thinking,
        )
    }

    /// agent 工具循环（docs/architecture.md 四节）。
    pub fn turn(&mut self, ui: &mut dyn Ui, cancel: &Cancel, input: &str) -> ChatResult<TurnEnd> {
        self.steps = 0;
        self.history.push(Msg::User(input.to_string()));

        loop {
            // 工具执行完回到请求前再查一次：取消/退出不进入下一次请求。
            match cancel.state() {
                SignalState::None => {}
                SignalState::Cancel => {
                    emit(ui, RenderEvent::Error("已取消本轮响应".into()));
                    return Ok(TurnEnd::Cancelled);
                }
                SignalState::Quit => return Err(Quit),
            }

            let body = self.build_body();
            let provider = self.selection.provider.clone();
            let headers = self.adapter.headers(&provider);
            let adapter = self.adapter.clone();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                provider::stream_request(adapter, provider, body, headers, tx);
            });

            let mut final_msg: Option<Msg> = None;
            let mut error: Option<String> = None;
            let mut interrupt: Option<bool> = None;
            let mut reasoning_seen = false;

            loop {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(ev) => {
                        // 持续有事件时也逐块可取消。
                        match cancel.state() {
                            SignalState::None => {}
                            SignalState::Cancel => {
                                interrupt = Some(false);
                                break;
                            }
                            SignalState::Quit => {
                                interrupt = Some(true);
                                break;
                            }
                        }
                        match ev {
                            StreamEvent::Delta { text, reasoning } => {
                                if let Some(r) = reasoning {
                                    emit(ui, RenderEvent::ReasoningDelta(r));
                                    reasoning_seen = true;
                                }
                                if let Some(t) = text {
                                    if reasoning_seen {
                                        emit(ui, RenderEvent::LineBreak);
                                        reasoning_seen = false;
                                    }
                                    emit(ui, RenderEvent::TextDelta(t));
                                }
                            }
                            StreamEvent::ToolDelta { .. } | StreamEvent::Usage { .. } => {}
                            StreamEvent::Done { .. } => {}
                            StreamEvent::Final(msg) => {
                                final_msg = Some(*msg);
                                break;
                            }
                            StreamEvent::Error(e) => {
                                error = Some(e);
                                break;
                            }
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => match cancel.state() {
                        SignalState::None => {}
                        SignalState::Cancel => {
                            interrupt = Some(false);
                            break;
                        }
                        SignalState::Quit => {
                            interrupt = Some(true);
                            break;
                        }
                    },
                    Err(RecvTimeoutError::Disconnected) => {
                        // worker 契约要求 Final/Error 收尾；防御性兜底。
                        error = Some("响应流异常断开（未收到完整消息）".into());
                        break;
                    }
                }
            }
            // 取消时放弃流线程：drop Receiver，worker 下次 send 报错即退出。
            drop(rx);

            if let Some(quit) = interrupt {
                if quit || cancel.state() == SignalState::Quit {
                    return Err(Quit);
                }
                emit(ui, RenderEvent::Error("已取消本轮响应".into()));
                return Ok(TurnEnd::Cancelled);
            }
            if let Some(e) = error {
                emit(ui, RenderEvent::Error(e.clone()));
                return Ok(TurnEnd::Error(e));
            }
            let Some(msg) = final_msg else {
                let e = "响应流异常结束（缺少完整消息）".to_string();
                emit(ui, RenderEvent::Error(e.clone()));
                return Ok(TurnEnd::Error(e));
            };

            let Msg::Assistant { text, reasoning, tool_calls } = msg else {
                let e = "内部协议错误：Final 不是 Assistant 消息".to_string();
                emit(ui, RenderEvent::Error(e.clone()));
                return Ok(TurnEnd::Error(e));
            };

            if tool_calls.is_empty() {
                self.history.push(Msg::Assistant { text, reasoning, tool_calls });
                // 若恰在此窗口收到第二次 Ctrl-C，直接走退出，不等下一次输入。
                if cancel.state() == SignalState::Quit {
                    return Err(Quit);
                }
                return Ok(TurnEnd::Done);
            }

            // 提交前校验：tool_call_id 唯一且非空；非法则整条按 Error。
            let mut seen = HashSet::new();
            if let Some(bad) = tool_calls
                .iter()
                .find(|c| c.id.is_empty() || !seen.insert(c.id.clone()))
            {
                let e = format!("工具调用 id 非法或重复：`{}`", bad.id);
                emit(ui, RenderEvent::Error(e.clone()));
                return Ok(TurnEnd::Error(e));
            }

            self.history.push(Msg::Assistant {
                text,
                reasoning,
                tool_calls: tool_calls.clone(),
            });

            let max = self.cfg.max_tool_steps.unwrap_or(DEFAULT_MAX_TOOL_STEPS);
            let mut cancelled = false;
            let mut quit = false;
            let mut limit_hit = false;
            let mut i = 0;

            while i < tool_calls.len() {
                if self.steps >= max {
                    for call in &tool_calls[i..] {
                        let content = "工具调用次数已达上限，未执行".to_string();
                        emit(ui, RenderEvent::ToolResult(content.clone()));
                        self.history.push(Msg::Tool {
                            tool_call_id: call.id.clone(),
                            content,
                        });
                    }
                    limit_hit = true;
                    break;
                }

                match cancel.state() {
                    SignalState::None => {}
                    SignalState::Cancel => {
                        self.placeholders(ui, &tool_calls[i..], "用户取消执行");
                        cancelled = true;
                        break;
                    }
                    SignalState::Quit => {
                        self.placeholders(ui, &tool_calls[i..], "用户取消执行");
                        quit = true;
                        break;
                    }
                }

                let outcome = dispatch_tool_call(self, ui, cancel, &tool_calls[i])?;
                match outcome {
                    CallOutcome::Result(content) => {
                        self.history.push(Msg::Tool {
                            tool_call_id: tool_calls[i].id.clone(),
                            content,
                        });
                        self.steps += 1;
                    }
                    CallOutcome::Cancelled(content) => {
                        self.history.push(Msg::Tool {
                            tool_call_id: tool_calls[i].id.clone(),
                            content,
                        });
                        self.steps += 1;
                        self.placeholders(ui, &tool_calls[i + 1..], "用户取消执行");
                        if cancel.state() == SignalState::Quit {
                            quit = true;
                        } else {
                            cancelled = true;
                        }
                        break;
                    }
                }

                // 执行/清理期间第二次 Ctrl-C：信号计数已置 quit。
                if cancel.state() == SignalState::Quit {
                    self.placeholders(ui, &tool_calls[i + 1..], "用户取消执行");
                    quit = true;
                    break;
                }
                i += 1;
            }

            if quit {
                return Err(Quit);
            }
            if cancelled {
                // 清理占位期间第二次 Ctrl-C：收尾完成后退出。
                if cancel.state() == SignalState::Quit {
                    return Err(Quit);
                }
                return Ok(TurnEnd::Cancelled);
            }
            if limit_hit {
                let e = format!("已达到每轮工具调用上限 {max}，本轮结束");
                emit(ui, RenderEvent::Error(e.clone()));
                return Ok(TurnEnd::Error(e));
            }
            // 全部工具执行完：带上 Tool 结果继续请求。
        }
    }

    fn placeholders(&mut self, ui: &mut dyn Ui, calls: &[ToolCall], content: &str) {
        for call in calls {
            emit(ui, RenderEvent::ToolResult(content.to_string()));
            self.history.push(Msg::Tool {
                tool_call_id: call.id.clone(),
                content: content.to_string(),
            });
        }
    }

    /// 切换供应商（REPL /provider）：联动重置模型与 thinking，并按新 provider
    /// 重建适配器（compat / protocol 可能不同）。
    pub fn switch_provider(&mut self, id: &str) -> Result<(), String> {
        let mut selection = config::selection_for_provider(&self.cfg, id)?;
        // 当前模型本来就属于新 provider 时保留模型与 thinking；
        // 只有跨 provider 才重置为该 provider 声明顺序第一个模型。
        if config::model_provider_prefix(&self.selection.model_id) == Some(id) {
            selection.model_id = self.selection.model_id.clone();
            selection.model = self.selection.model.clone();
            selection.thinking = self.selection.thinking.clone();
        }
        let adapter = provider::adapter_for(&selection.provider)?;
        self.selection = selection;
        self.adapter = adapter;
        Ok(())
    }

    /// 切换模型（REPL /model）：跨 provider 拒绝；thinking 重置。
    pub fn switch_model(&mut self, id: &str) -> Result<(), String> {
        if config::find_model(&self.cfg, id).is_none() {
            return Err(format!("未声明的模型：{id}"));
        }
        let prefix = config::model_provider_prefix(id)
            .ok_or_else(|| format!("模型 id 必须带供应商前缀：{id}"))?;
        if prefix != self.selection.provider_id {
            return Err(format!(
                "模型 {id} 不属于当前供应商 {}，请先 /provider {prefix}",
                self.selection.provider_id
            ));
        }
        let selection = config::selection_for_model(&self.cfg, id)?;
        self.selection = selection;
        Ok(())
    }

    /// 切换思考档位（REPL /thinking）：无效档位拒绝；更新 selection.thinking。
    pub fn switch_thinking(&mut self, level: &str) -> Result<(), String> {
        let levels: Vec<&str> = self.selection.model.thinking.keys().map(String::as_str).collect();
        if levels.is_empty() {
            return Err(format!("模型 {} 未声明思考档位", self.selection.model_id));
        }
        if !self.selection.model.thinking.contains_key(level) {
            return Err(format!(
                "无效思考档位 `{level}`；可选：{}",
                levels.join(", ")
            ));
        }
        self.selection.thinking = Some(level.to_string());
        Ok(())
    }
}

/// 执行单个工具调用（在 turn 的逐个循环中被调用）。
/// 返回该调用的 Tool 消息内容（真实结果 / 错误文本 / 占位文本）。
enum CallOutcome {
    Result(String),      // 正常完成 / 真实错误文本 / 占位文本
    Cancelled(String),   // 取消：当前"已部分执行/未执行"文本；剩余调用补占位
}

fn dispatch_tool_call(
    session: &mut Session,
    ui: &mut dyn Ui,
    cancel: &Cancel,
    call: &ToolCall,
) -> ChatResult<CallOutcome> {
    if call.name != "bash" {
        let content = format!("未知工具：{}", call.name);
        emit(ui, RenderEvent::ToolResult(content.clone()));
        return Ok(CallOutcome::Result(content));
    }

    let args: serde_json::Value = match serde_json::from_str(&call.arguments) {
        Ok(v) => v,
        Err(e) => {
            let content = format!("工具参数解析失败：{e}；原始参数：{}", call.arguments);
            emit(ui, RenderEvent::ToolResult(content.clone()));
            return Ok(CallOutcome::Result(content));
        }
    };
    let Some(command) = args.get("command").and_then(|v| v.as_str()) else {
        let content = "工具参数解析失败：缺少字符串字段 command".to_string();
        emit(ui, RenderEvent::ToolResult(content.clone()));
        return Ok(CallOutcome::Result(content));
    };
    if command.trim().is_empty() {
        let content = "工具参数解析失败：command 为空".to_string();
        emit(ui, RenderEvent::ToolResult(content.clone()));
        return Ok(CallOutcome::Result(content));
    }

    emit(ui, RenderEvent::ToolCall(command.to_string()));

    let cwd = crate::session_cwd();
    match tools::gate(
        ui,
        session.yes,
        &cwd,
        command,
        &session.cfg.tools.bash.allow,
    ) {
        ConfirmOutcome::No => {
            let content = "用户拒绝执行".to_string();
            emit(ui, RenderEvent::ToolResult(content.clone()));
            Ok(CallOutcome::Result(content))
        }
        ConfirmOutcome::Cancelled => {
            let content = "用户取消执行".to_string();
            emit(ui, RenderEvent::ToolResult(content.clone()));
            Ok(CallOutcome::Cancelled(content))
        }
        ConfirmOutcome::Yes => {
            // 确认与执行之间若已收到信号，直接按取消收尾，不再 spawn。
            if cancel.state() != SignalState::None {
                let content = "用户取消执行".to_string();
                emit(ui, RenderEvent::ToolResult(content.clone()));
                return Ok(CallOutcome::Cancelled(content));
            }
            let out = tools::run_bash(command, &session.cfg.tools.bash, cancel);
            emit(ui, RenderEvent::ToolResult(out.content.clone()));
            match out.end {
                BashEnd::SpawnFailed(e) => {
                    Ok(CallOutcome::Result(format!("无法启动命令：{e}")))
                }
                BashEnd::Cancelled | BashEnd::QuitRequested => {
                    Ok(CallOutcome::Cancelled(out.content))
                }
                BashEnd::TimedOut | BashEnd::Exited(_) => {
                    Ok(CallOutcome::Result(out.content))
                }
            }
        }
    }
}

/// 渲染事件转发（agent loop → ui.print）。
fn emit(ui: &mut dyn Ui, ev: RenderEvent) {
    ui.print(&ev);
}
