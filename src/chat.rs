//! 内部消息模型与 agent 工具循环。见 docs/architecture.md 三/四节。

use crate::config::{self, Config, Provider, Selection};
use crate::provider::{self, ProviderAdapter, StreamEvent};
use crate::render::RenderEvent;
use crate::term::{Cancel, SignalState};
use crate::tools::{self, BashEnd, BashOutput};
use crate::ui::{ConfirmOutcome, Ui};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_MAX_TOOL_STEPS: u32 = 25;

/// 取消/退出时未执行调用的占位文本。
const CANCELLED_PLACEHOLDER: &str = "用户取消执行";
/// 步数超限时未执行调用的占位文本。
const LIMIT_PLACEHOLDER: &str = "工具调用次数已达上限，未执行";
/// 终端输出失败时未执行调用的占位文本（exit 1 路径）。
const FATAL_PLACEHOLDER: &str = "终端输出失败，未执行";
/// 流式接收被取消时展示的提示。
const TURN_CANCELLED_MSG: &str = "已取消本轮响应";

fn system_history(prompt: Option<&str>) -> Vec<Msg> {
    prompt.map(|p| vec![Msg::System(p.to_string())]).unwrap_or_default()
}

/// 与协议解耦的消息模型（适配层负责映射）。
#[derive(Debug, Clone)]
pub enum Msg {
    System(String),
    User(String),
    /// Assistant 载荷：流终态与历史共用同一类型（见 [`AssistantMsg`]）。
    Assistant(AssistantMsg),
    Tool {
        tool_call_id: String,
        content: String,
    },
}

/// Assistant 消息的载荷。`Final` 事件与历史中的 Assistant 消息共用此类型，
/// 避免事件携带 System/User/Tool 等无效状态。
#[derive(Debug, Clone)]
pub struct AssistantMsg {
    pub text: Option<String>,
    pub reasoning: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

/// arguments 保持原始 JSON 字符串。
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// 一次流式请求的启动边界：生产实现起 worker 线程跑真实 HTTP；
/// 测试实现注入预编排事件序列（无网络）。见 docs/architecture.md 七节。
pub trait StreamLauncher: Send + Sync {
    fn launch(
        &self,
        adapter: Arc<dyn ProviderAdapter>,
        provider: Provider,
        body: Value,
        headers: Vec<(String, String)>,
        cancelled: Arc<AtomicBool>,
    ) -> Receiver<StreamEvent>;
}

/// 生产实现：每轮请求一个工作线程（docs/architecture.md 五节）。
struct WorkerLauncher;

impl StreamLauncher for WorkerLauncher {
    fn launch(
        &self,
        adapter: Arc<dyn ProviderAdapter>,
        provider: Provider,
        body: Value,
        headers: Vec<(String, String)>,
        cancelled: Arc<AtomicBool>,
    ) -> Receiver<StreamEvent> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            provider::stream_request(adapter, provider, body, headers, tx, cancelled);
        });
        rx
    }
}

/// 单个 bash 调用的执行边界：生产实现跑真实进程组；
/// 测试实现返回预设结果（无进程）。
pub trait ToolRunner: Send + Sync {
    fn run(&self, command: &str, cfg: &crate::config::BashToolConfig, cancel: &Cancel) -> BashOutput;
}

struct BashRunner;

impl ToolRunner for BashRunner {
    fn run(&self, command: &str, cfg: &crate::config::BashToolConfig, cancel: &Cancel) -> BashOutput {
        tools::run_bash(command, cfg, cancel)
    }
}

/// 会话状态：历史 + 当前选择 + 适配器 + 工具步数。
/// adapter 为 Arc 共享：会话长期持有，每轮 clone 一份按值传给 worker
/// （thread::spawn + detach 取消语义需 'static，不能借引用）。
/// thinking 唯一真值源是 selection.thinking（switch_* 同步更新）。
/// launcher / tool_runner 是测试注入边界：默认生产实现，集成测试可替换
/// 为预编排事件与预设结果（见 tests/agent_loop.rs）。
pub struct Session {
    pub cfg: Config,
    pub selection: Selection,
    pub adapter: Arc<dyn ProviderAdapter>,
    pub launcher: Arc<dyn StreamLauncher>,
    pub tool_runner: Arc<dyn ToolRunner>,
    pub yes: bool,
    pub history: Vec<Msg>,
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
    /// 终端 I/O 失败（REPL 无法继续，exit 1；占位已补齐）
    Fatal(String),
}

/// 一次流式请求的接收结果（`Session::receive_stream` 的返回值）。
#[derive(Debug)]
enum StreamOutcome {
    /// 收到完整 Assistant 消息
    Final(AssistantMsg),
    /// 错误（已展示，保留已完成历史）
    Error(String),
    /// 用户取消
    Cancelled,
    /// 用户请求退出
    Quit,
}

/// 一批 tool_calls 的执行阶段结果（`Session::run_tool_calls` 的返回值）。
/// Cancelled/Quit/Limit/Fatal 的占位补齐已在 run_tool_calls 内完成。
#[derive(Debug)]
enum ToolPhase {
    /// 全部执行完（继续请求）
    Done,
    /// 用户取消
    Cancelled,
    /// 退出请求
    Quit,
    /// 步数超限，携带上限值
    Limit(u32),
    /// 终端 I/O 失败（携带错误文本，exit 1）
    Fatal(String),
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
        let history = system_history(cfg.system_prompt.as_deref());
        Self {
            cfg,
            selection,
            adapter,
            launcher: Arc::new(WorkerLauncher),
            tool_runner: Arc::new(BashRunner),
            yes,
            history,
        }
    }

    /// `/clear`：历史重置为仅含 system 消息（播种语义同 new）。
    pub fn clear(&mut self) {
        self.history = system_history(self.cfg.system_prompt.as_deref());
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
    /// 编排：发起请求并接收流（`receive_stream`）→ 无工具调用即收束；
    /// 有则交 `run_tool_calls` 执行，全部执行完带上结果继续请求。
    pub fn turn(&mut self, ui: &mut dyn Ui, cancel: &Cancel, input: &str) -> ChatResult<TurnEnd> {
        // 本轮工具步数（每次用户输入触发的回合局部计数）。
        let mut steps: u32 = 0;
        self.history.push(Msg::User(input.to_string()));

        loop {
            // 工具执行完回到请求前再查一次：取消/退出不进入下一次请求。
            if let Some(sig) = cancel.pending() {
                if sig == SignalState::Quit {
                    return Err(Quit);
                }
                emit(ui, RenderEvent::Error(TURN_CANCELLED_MSG.into()));
                return Ok(TurnEnd::Cancelled);
            }

            let msg = match self.receive_stream(ui, cancel) {
                StreamOutcome::Final(msg) => msg,
                StreamOutcome::Error(e) => return Ok(TurnEnd::Error(e)),
                StreamOutcome::Cancelled => return Ok(TurnEnd::Cancelled),
                StreamOutcome::Quit => return Err(Quit),
            };

            // 流接收/渲染期间终端写失败：不再提交与执行，直接上抛（exit 1）。
            if let Some(e) = ui.take_fatal() {
                return Ok(TurnEnd::Fatal(e));
            }

            if msg.tool_calls.is_empty() {
                self.history.push(Msg::Assistant(msg));
                // 若恰在此窗口收到第二次 Ctrl-C，直接走退出，不等下一次输入。
                if cancel.is_quit() {
                    return Err(Quit);
                }
                return Ok(TurnEnd::Done);
            }

            // 提交前校验：tool_call_id 唯一且非空；非法则整条按 Error。
            let mut seen = HashSet::new();
            if let Some(bad) = msg
                .tool_calls
                .iter()
                .find(|c| c.id.is_empty() || !seen.insert(c.id.clone()))
            {
                let e = format!("工具调用 id 非法或重复：`{}`", bad.id);
                emit(ui, RenderEvent::Error(e.clone()));
                return Ok(TurnEnd::Error(e));
            }

            let tool_calls = msg.tool_calls.clone();
            self.history.push(Msg::Assistant(msg));

            match self.run_tool_calls(ui, cancel, &mut steps, &tool_calls) {
                // 全部工具执行完：带上 Tool 结果继续请求。
                ToolPhase::Done => continue,
                ToolPhase::Cancelled => {
                    // 清理占位期间第二次 Ctrl-C：收尾完成后退出。
                    if cancel.is_quit() {
                        return Err(Quit);
                    }
                    return Ok(TurnEnd::Cancelled);
                }
                ToolPhase::Quit => return Err(Quit),
                ToolPhase::Limit(max) => {
                    let e = format!("已达到每轮工具调用上限 {max}，本轮结束");
                    emit(ui, RenderEvent::Error(e.clone()));
                    return Ok(TurnEnd::Error(e));
                }
                ToolPhase::Fatal(e) => return Ok(TurnEnd::Fatal(e)),
            }
        }
    }

    /// 发起一次流式请求并接收事件：渲染增量、逐块取消判定都在这里。
    /// 契约见 docs/architecture.md 四节接收循环。
    fn receive_stream(&self, ui: &mut dyn Ui, cancel: &Cancel) -> StreamOutcome {
        let body = self.build_body();
        let provider = self.selection.provider.clone();
        let headers = self.adapter.headers(&provider);
        let adapter = self.adapter.clone();
        // 取消标志：worker 在读取循环里轮询它，不必等下一次 send 失败。
        let cancelled = Arc::new(AtomicBool::new(false));
        let rx = self
            .launcher
            .launch(adapter, provider, body, headers, cancelled.clone());
        let handle = StreamHandle { rx, cancelled };

        let mut final_msg: Option<AssistantMsg> = None;
        let mut error: Option<String> = None;
        let mut reasoning_seen = false;

        loop {
            let ev = match handle.recv(Duration::from_millis(100)) {
                Ok(ev) => ev,
                Err(RecvTimeoutError::Timeout) => {
                    // 空闲轮询间隙只查信号；无信号继续等。
                    if let Some(sig) = cancel.pending() {
                        handle.abandon();
                        return signal_outcome(ui, sig, cancel);
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // worker 契约要求 Final/Error 收尾；防御性兜底。
                    error = Some("响应流异常断开（未收到完整消息）".into());
                    break;
                }
            };
            // 持续有事件时也逐块可取消。
            if let Some(sig) = cancel.pending() {
                handle.abandon();
                return signal_outcome(ui, sig, cancel);
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
        // 正常结束：drop Receiver，worker 已退出（handle 随作用域结束一并释放）。
        drop(handle);

        if let Some(e) = error {
            emit(ui, RenderEvent::Error(e.clone()));
            return StreamOutcome::Error(e);
        }
        let Some(msg) = final_msg else {
            let e = "响应流异常结束（缺少完整消息）".to_string();
            emit(ui, RenderEvent::Error(e.clone()));
            return StreamOutcome::Error(e);
        };
        StreamOutcome::Final(msg)
    }

    /// 执行一批 tool_calls：每个执行前先查限额与取消标志；全部执行完返回 Done。
    /// 任何返回路径都保证剩余调用已补占位（docs/architecture.md 四节配对不变式）。
    /// 结果写入与计数集中在 commit_call_result：单调用分支再多也不会漏掉历史配对。
    fn run_tool_calls(
        &mut self,
        ui: &mut dyn Ui,
        cancel: &Cancel,
        steps: &mut u32,
        tool_calls: &[ToolCall],
    ) -> ToolPhase {
        let max = self.cfg.max_tool_steps.unwrap_or(DEFAULT_MAX_TOOL_STEPS);
        let mut i = 0;

        while i < tool_calls.len() {
            // 限额逐个前置检查：当前及剩余调用补超限占位。
            if *steps >= max {
                self.placeholders(ui, &tool_calls[i..], LIMIT_PLACEHOLDER);
                return ToolPhase::Limit(max);
            }

            if let Some(sig) = cancel.pending() {
                self.placeholders(ui, &tool_calls[i..], CANCELLED_PLACEHOLDER);
                return if sig == SignalState::Quit {
                    ToolPhase::Quit
                } else {
                    ToolPhase::Cancelled
                };
            }

            // 终端写失败：剩余调用补占位后上抛（exit 1；占位写入可能同样失败，
            // 但历史配对仍由本函数保证）。
            if let Some(e) = ui.take_fatal() {
                self.placeholders(ui, &tool_calls[i..], FATAL_PLACEHOLDER);
                return ToolPhase::Fatal(e);
            }

            let outcome = dispatch_tool_call(
                ui,
                cancel,
                &*self.tool_runner,
                &self.cfg.tools.bash,
                self.yes,
                &tool_calls[i],
            );
            // 单一出口：渲染结果、写入历史、计数（含取消/退出路径的当前调用）。
            self.commit_call_result(ui, &tool_calls[i].id, &outcome.content, steps);

            if outcome.stop {
                self.placeholders(ui, &tool_calls[i + 1..], CANCELLED_PLACEHOLDER);
                // 执行/清理期间第二次 Ctrl-C：信号计数已置 quit。
                return if cancel.is_quit() {
                    ToolPhase::Quit
                } else {
                    ToolPhase::Cancelled
                };
            }

            // 执行/清理期间第二次 Ctrl-C：信号计数已置 quit。
            if cancel.is_quit() {
                self.placeholders(ui, &tool_calls[i + 1..], CANCELLED_PLACEHOLDER);
                return ToolPhase::Quit;
            }
            i += 1;
        }

        // 全部工具执行完。
        ToolPhase::Done
    }

    /// 单个调用结果的唯一出口：渲染 ToolResult、写入历史、计数。
    /// 所有终态（真实结果/错误文本/拒绝/取消/部分执行）都经此写入，
    /// 保证 tool_calls 与 Tool 消息的配对不变式只需在这一处维护。
    fn commit_call_result(
        &mut self,
        ui: &mut dyn Ui,
        call_id: &str,
        content: &str,
        steps: &mut u32,
    ) {
        emit(ui, RenderEvent::ToolResult(content.to_string()));
        self.history.push(Msg::Tool {
            tool_call_id: call_id.to_string(),
            content: content.to_string(),
        });
        *steps += 1;
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
        if let Some(e) = config::thinking_level_err(&self.selection.model, &self.selection.model_id, level) {
            return Err(e);
        }
        self.selection.thinking = Some(level.to_string());
        Ok(())
    }
}

/// 一次流式请求的主线程侧句柄：接收器与取消标志绑定在一起。
/// `abandon` 是唯一的放弃路径：置位取消标志（worker 轮询它即静默退出）并 drop
/// Receiver；正常收尾只需让 handle 离开作用域。
struct StreamHandle {
    rx: mpsc::Receiver<StreamEvent>,
    cancelled: Arc<AtomicBool>,
}

impl StreamHandle {
    fn recv(&self, timeout: Duration) -> Result<StreamEvent, RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }

    /// 放弃本次请求：置位取消标志并丢弃接收器（worker 据此静默退出）。
    fn abandon(self) {
        self.cancelled.store(true, Ordering::SeqCst);
        drop(self.rx);
    }
}

/// 单个调用的执行结果（在 run_tool_calls 的逐个循环中被调用）。
/// content 是喂回模型的 Tool 消息文本（真实结果 / 错误文本 / 拒绝 / 占位）；
/// stop = 取消/退出：当前调用结果仍经统一出口写入，剩余调用由调用方补占位。
struct CallOutcome {
    content: String,
    stop: bool,
}

/// 参数/判定类错误（不停止批次）：作为该调用的 Tool 结果文本。
fn tool_outcome(content: String) -> CallOutcome {
    CallOutcome { content, stop: false }
}

fn dispatch_tool_call(
    ui: &mut dyn Ui,
    cancel: &Cancel,
    tool_runner: &dyn ToolRunner,
    bash_cfg: &crate::config::BashToolConfig,
    yes: bool,
    call: &ToolCall,
) -> CallOutcome {
    if call.name != "bash" {
        return tool_outcome(format!("未知工具：{}", call.name));
    }

    let args: serde_json::Value = match serde_json::from_str(&call.arguments) {
        Ok(v) => v,
        Err(e) => {
            return tool_outcome(format!(
                "工具参数解析失败：{e}；原始参数：{}",
                call.arguments
            ));
        }
    };
    let Some(command) = args.get("command").and_then(|v| v.as_str()) else {
        return tool_outcome("工具参数解析失败：缺少字符串字段 command".to_string());
    };
    if command.trim().is_empty() {
        return tool_outcome("工具参数解析失败：command 为空".to_string());
    }

    emit(ui, RenderEvent::ToolCall(command.to_string()));

    let cwd = crate::session_cwd();
    match tools::gate(ui, yes, &cwd, command, &bash_cfg.allow) {
        ConfirmOutcome::No => tool_outcome("用户拒绝执行".to_string()),
        ConfirmOutcome::Cancelled => CallOutcome {
            content: CANCELLED_PLACEHOLDER.to_string(),
            stop: true,
        },
        ConfirmOutcome::Yes => {
            // 确认与执行之间若已收到信号，直接按取消收尾，不再 spawn。
            if cancel.pending().is_some() {
                return CallOutcome {
                    content: CANCELLED_PLACEHOLDER.to_string(),
                    stop: true,
                };
            }
            let out = tool_runner.run(command, bash_cfg, cancel);
            match out.end {
                BashEnd::SpawnFailed(_) | BashEnd::ReapFailed(_) => tool_outcome(out.content),
                BashEnd::Cancelled | BashEnd::QuitRequested => CallOutcome {
                    content: out.content,
                    stop: true,
                },
                BashEnd::TimedOut | BashEnd::Exited(_) => tool_outcome(out.content),
            }
        }
    }
}

/// 信号触发的接收收尾：放弃接收（drop Receiver）后判定方向。
/// 收尾间隙可能再收到一次 Ctrl-C（cancel 升级为 quit），以最终状态为准。
fn signal_outcome(ui: &mut dyn Ui, sig: SignalState, cancel: &Cancel) -> StreamOutcome {
    if sig == SignalState::Quit || cancel.is_quit() {
        return StreamOutcome::Quit;
    }
    emit(ui, RenderEvent::Error(TURN_CANCELLED_MSG.into()));
    StreamOutcome::Cancelled
}

/// 渲染事件转发（agent loop → ui.print）。
fn emit(ui: &mut dyn Ui, ev: RenderEvent) {
    ui.print(&ev);
}
