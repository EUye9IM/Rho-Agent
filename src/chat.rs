//! 内部消息模型与 agent 工具循环：历史、流接收、工具批次执行。

use crate::config::{self, Config, Provider, Selection};
use crate::context;
pub use crate::message::{AssistantMsg, Msg, TokenUsage, ToolCall};
use crate::provider::{self, ProviderAdapter, StreamEvent};
use crate::render::{RenderEvent, StatusLine};
use crate::term::{Cancel, SignalState};
use crate::tools::{ToolContext, ToolRegistry, ToolResult, ToolStatus};
use crate::ui::Ui;
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_MAX_TOOL_STEPS: u32 = 100;

/// 取消/退出时未执行调用的占位文本。
const CANCELLED_PLACEHOLDER: &str = "用户取消执行";
/// 步数超限时未执行调用的占位文本。
const LIMIT_PLACEHOLDER: &str = "工具调用次数已达上限，未执行";
/// 终端输出失败时未执行调用的占位文本（exit 1 路径）。
const FATAL_PLACEHOLDER: &str = "终端输出失败，未执行";
/// 流式接收被取消时展示的提示。
const TURN_CANCELLED_MSG: &str = "已取消本轮响应";
/// 压缩被取消时展示的提示。
const COMPACT_CANCELLED_MSG: &str = "已取消压缩";
/// 没有可压缩的轮次时展示的提示。
const COMPACT_EMPTY_MSG: &str = "历史不足以压缩（可压缩的轮次不足）";
/// 摘要请求没有返回正文时展示的提示。
const COMPACT_EMPTY_RESULT_MSG: &str = "压缩返回空内容，历史未改动";

fn system_history(prompt: Option<&str>) -> Vec<Msg> {
    prompt
        .map(|p| vec![Msg::System(p.to_string())])
        .unwrap_or_default()
}

/// 一次流式请求的启动边界：生产实现起 worker 线程跑真实 HTTP；
/// 测试实现注入预编排事件序列（无网络）。
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

/// 生产实现：每轮请求一个工作线程。
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

/// 会话状态：历史 + 当前选择 + 适配器 + 工具注册表。
/// adapter 为 Arc 共享：会话长期持有，每轮 clone 一份按值传给 worker
/// （thread::spawn + detach 取消语义需 'static，不能借引用）；切换供应商时按新
/// protocol/compat 重建，与 selection 保持一致由本类型的方法维护，字段不对外暴露。
/// thinking 唯一真值源是 selection.thinking（switch_* 同步更新）。
/// launcher 与 tools 是测试注入边界：`with_launcher` / `with_tools` 替换，
/// 配置与选择经 `cfg()` / `selection()` 只读访问。
pub struct Session {
    cfg: Config,
    selection: Selection,
    adapter: Arc<dyn ProviderAdapter>,
    launcher: Arc<dyn StreamLauncher>,
    tools: ToolRegistry,
    /// rho 启动目录，用于解析工具的相对路径。
    cwd: PathBuf,
    /// 只由 agent loop 写入；对外只读，保证 tool_calls 与 Tool 结果的配对不变式。
    history: Vec<Msg>,
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
    /// 收到完整 Assistant 消息，以及该请求的 token 用量
    Final {
        msg: AssistantMsg,
        usage: Option<TokenUsage>,
    },
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
    pub fn new(cfg: Config, selection: Selection, adapter: Arc<dyn ProviderAdapter>) -> Self {
        let history = system_history(cfg.system_prompt.as_deref());
        Self {
            cfg,
            selection,
            adapter,
            launcher: Arc::new(WorkerLauncher),
            tools: ToolRegistry::builtin(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            history,
        }
    }

    /// 只读历史（测试与 REPL 检查用）；写入路径留在 agent loop 内。
    pub fn history(&self) -> &[Msg] {
        &self.history
    }

    /// 当前配置（命令层与补全快照读取）。
    pub fn cfg(&self) -> &Config {
        &self.cfg
    }

    /// 当前选择：供应商、模型与思考档位（命令层与补全快照读取）。
    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    /// 测试注入边界：替换流式请求启动器（生产为每轮起 worker 线程的 `WorkerLauncher`）。
    pub fn with_launcher(mut self, launcher: Arc<dyn StreamLauncher>) -> Self {
        self.launcher = launcher;
        self
    }

    /// 测试注入边界：替换工具注册表（默认为内置 bash / read / write / edit）。
    pub fn with_tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = tools;
        self
    }

    /// 会话内第几条用户输入；`/clear` 重置历史后自然归零。
    /// 失败/取消的回合同样计入（回合入口就写入 User 消息）。
    fn turns(&self) -> u32 {
        self.history
            .iter()
            .filter(|m| matches!(m, Msg::User(_)))
            .count() as u32
    }

    /// 回合结束状态栏的内容：选择来自 selection，轮次来自历史，用量由调用方传入。
    fn status_line(&self, usage: Option<TokenUsage>) -> StatusLine {
        StatusLine {
            turn: self.turns(),
            model: self.selection.model_id.clone(),
            thinking: self.selection.thinking.clone(),
            usage,
            context_window: self.selection.model.context_window,
        }
    }

    /// `/clear`：历史重置为仅含 system 消息（播种语义同 new）。
    pub fn clear(&mut self) {
        self.history = system_history(self.cfg.system_prompt.as_deref());
    }

    /// `/compact`：把较早的若干轮换成一条 `Msg::Summary`。
    ///
    /// 请求形状与普通回合同形（真实历史前缀的副本 + 末尾一条 user 指令，
    /// 同样的 tools 与输出上限），只是收流不渲染。摘要正文原样使用、不再裁；
    /// 失败、空回复与取消都不改动历史。
    pub fn compact(&mut self, ui: &mut dyn Ui, cancel: &Cancel) -> ChatResult<()> {
        let keep = self
            .cfg
            .compact
            .keep_turns
            .unwrap_or(context::DEFAULT_KEEP_TURNS);
        let Some(plan) = context::plan(
            &self.history,
            keep,
            self.selection.model.context_window,
            self.selection.model.max_output,
        ) else {
            emit(ui, RenderEvent::Error(COMPACT_EMPTY_MSG.into()));
            return Ok(());
        };
        let (start, end, dropped) = (plan.start, plan.end, plan.dropped());
        let prompt = self
            .cfg
            .compact
            .prompt
            .as_deref()
            .unwrap_or(context::DEFAULT_COMPACT_PROMPT);

        let mut head = plan.head;
        head.push(Msg::User(prompt.to_string()));
        // thinking 传 None：摘要不需要思维链，省预算也更快。
        let body = self.adapter.build_body(
            &head,
            &self.selection.model,
            &self.selection.model_id,
            None,
            &self.tools.specs(),
        );

        match self.drive_stream(ui, cancel, body, false, COMPACT_CANCELLED_MSG) {
            StreamOutcome::Final { msg, usage: _ } => {
                let Some(summary) = msg
                    .text
                    .map(|text| text.trim().to_string())
                    .filter(|text| !text.is_empty())
                else {
                    emit(ui, RenderEvent::Error(COMPACT_EMPTY_RESULT_MSG.into()));
                    return Ok(());
                };
                let before = crate::render::compact_tokens(context::estimate_tokens(
                    &self.history[start..end],
                ));
                let after = crate::render::compact_tokens(context::estimate_text(&summary));
                self.history.splice(start..end, [Msg::Summary(summary)]);
                emit(
                    ui,
                    RenderEvent::TextDelta(format!(
                        "已压缩 {dropped} 条消息 → 摘要（约 {before} → {after} tokens）\n"
                    )),
                );
                Ok(())
            }
            // 错误与取消已由 drive_stream 渲染。
            StreamOutcome::Error(_) | StreamOutcome::Cancelled => Ok(()),
            StreamOutcome::Quit => Err(Quit),
        }
    }

    fn build_body(&self) -> serde_json::Value {
        let thinking = self
            .selection
            .thinking
            .as_ref()
            .and_then(|level| self.selection.model.thinking.get(level));
        let tools = self.tools.specs();
        self.adapter.build_body(
            &self.history,
            &self.selection.model,
            &self.selection.model_id,
            thinking,
            &tools,
        )
    }

    /// agent 工具循环：一次用户输入触发一个回合。
    /// 回合结束（除 fatal 与退出路径）补一条状态栏事件；用量随回合结束一次性交付，
    /// 不进会话状态。
    pub fn turn(&mut self, ui: &mut dyn Ui, cancel: &Cancel, input: &str) -> ChatResult<TurnEnd> {
        // 只在本回合内有效：最近一次成功请求的用量。
        let mut usage: Option<TokenUsage> = None;
        let end = self.turn_inner(ui, cancel, input, &mut usage)?;
        // 终端写失败时再写只会重复失败：状态栏跳过。
        if !matches!(end, TurnEnd::Fatal(_)) {
            emit(ui, RenderEvent::Status(self.status_line(usage)));
        }
        Ok(end)
    }

    /// 工具循环本体。`usage` 从每次完成的请求刷新，提前返回的路径也把它带出去。
    fn turn_inner(
        &mut self,
        ui: &mut dyn Ui,
        cancel: &Cancel,
        input: &str,
        usage: &mut Option<TokenUsage>,
    ) -> ChatResult<TurnEnd> {
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
                StreamOutcome::Final { msg, usage: u } => {
                    *usage = u;
                    msg
                }
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

    fn receive_stream(&self, ui: &mut dyn Ui, cancel: &Cancel) -> StreamOutcome {
        self.drive_stream(ui, cancel, self.build_body(), true, TURN_CANCELLED_MSG)
    }

    /// 发起一次流式请求并接收事件：渲染增量、逐块取消判定都在这里。
    /// `render` 为 false 时只收终态与用量（压缩请求不把摘要正文写进屏幕）；
    /// `cancelled_msg` 是用户取消时展示的提示。
    /// 契约：worker 必经 Final 或 Error 收尾；收到终态或取消后返回。
    fn drive_stream(
        &self,
        ui: &mut dyn Ui,
        cancel: &Cancel,
        body: Value,
        render: bool,
        cancelled_msg: &str,
    ) -> StreamOutcome {
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
        let mut usage: Option<TokenUsage> = None;
        let mut error: Option<String> = None;

        loop {
            let ev = match handle.recv(Duration::from_millis(100)) {
                Ok(ev) => ev,
                Err(RecvTimeoutError::Timeout) => {
                    // 空闲轮询间隙只查信号；无信号继续等。
                    if let Some(sig) = cancel.pending() {
                        handle.abandon();
                        return signal_outcome(ui, sig, cancel, cancelled_msg);
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
                return signal_outcome(ui, sig, cancel, cancelled_msg);
            }
            match ev {
                StreamEvent::Delta { text, reasoning } => {
                    // 块间过渡（思考→正文的折行与样式收尾）由渲染器负责。
                    if !render {
                        continue;
                    }
                    if let Some(r) = reasoning {
                        emit(ui, RenderEvent::ReasoningDelta(r));
                    }
                    if let Some(t) = text {
                        emit(ui, RenderEvent::TextDelta(t));
                    }
                }
                StreamEvent::Usage(u) => {
                    usage = Some(u);
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
        StreamOutcome::Final { msg, usage }
    }

    /// 执行一批 tool_calls：每个执行前先查限额与取消标志；全部执行完返回 Done。
    /// 任何返回路径都保证剩余调用已补占位：Assistant 的每个 tool_call 都有对应 Tool 消息。
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
                self.placeholders(ui, &tool_calls[i..], LIMIT_PLACEHOLDER, ToolStatus::Failure);
                return ToolPhase::Limit(max);
            }

            if let Some(sig) = cancel.pending() {
                self.placeholders(
                    ui,
                    &tool_calls[i..],
                    CANCELLED_PLACEHOLDER,
                    ToolStatus::Interrupted,
                );
                return if sig == SignalState::Quit {
                    ToolPhase::Quit
                } else {
                    ToolPhase::Cancelled
                };
            }

            // 终端写失败：剩余调用补占位后上抛（exit 1；占位写入可能同样失败，
            // 但历史配对仍由本函数保证）。
            if let Some(e) = ui.take_fatal() {
                self.placeholders(ui, &tool_calls[i..], FATAL_PLACEHOLDER, ToolStatus::Failure);
                return ToolPhase::Fatal(e);
            }

            let result = {
                let ctx = ToolContext {
                    cwd: &self.cwd,
                    cancel,
                };
                dispatch_tool_call(ui, &self.tools, &ctx, &tool_calls[i])
            };
            let interrupted = result.status == ToolStatus::Interrupted;
            // 单一出口：渲染结果、写入历史、计数（含取消/退出路径的当前调用）。
            self.commit_call_result(ui, &tool_calls[i].name, &tool_calls[i].id, result, steps);

            if interrupted {
                self.placeholders(
                    ui,
                    &tool_calls[i + 1..],
                    CANCELLED_PLACEHOLDER,
                    ToolStatus::Interrupted,
                );
                // 执行/清理期间第二次 Ctrl-C：信号计数已置 quit。
                return if cancel.is_quit() {
                    ToolPhase::Quit
                } else {
                    ToolPhase::Cancelled
                };
            }

            // 执行/清理期间第二次 Ctrl-C：信号计数已置 quit。
            if cancel.is_quit() {
                self.placeholders(
                    ui,
                    &tool_calls[i + 1..],
                    CANCELLED_PLACEHOLDER,
                    ToolStatus::Interrupted,
                );
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
        name: &str,
        call_id: &str,
        result: ToolResult,
        steps: &mut u32,
    ) {
        emit(
            ui,
            RenderEvent::ToolResult {
                name: name.to_string(),
                display: result.display,
                status: result.status,
            },
        );
        self.history.push(Msg::Tool {
            tool_call_id: call_id.to_string(),
            content: result.output,
        });
        *steps += 1;
    }

    fn placeholders(
        &mut self,
        ui: &mut dyn Ui,
        calls: &[ToolCall],
        content: &str,
        status: ToolStatus,
    ) {
        for call in calls {
            emit(
                ui,
                RenderEvent::ToolResult {
                    name: call.name.clone(),
                    display: Some(content.to_string()),
                    status,
                },
            );
            self.history.push(Msg::Tool {
                tool_call_id: call.id.clone(),
                content: content.to_string(),
            });
        }
    }

    /// 切换供应商（REPL /provider）：联动重置模型与 thinking，并按新 provider
    /// 重建适配器（compat / protocol 可能不同）。规则在 config::switch_provider_selection。
    pub fn switch_provider(&mut self, id: &str) -> Result<(), String> {
        let selection = config::switch_provider_selection(&self.cfg, &self.selection, id)?;
        let adapter = provider::adapter_for(&selection.provider)?;
        self.selection = selection;
        self.adapter = adapter;
        Ok(())
    }

    /// 切换模型（REPL /model）：跨供应商拒绝；thinking 重置。
    /// 规则在 config::switch_model_selection。
    pub fn switch_model(&mut self, id: &str) -> Result<(), String> {
        let selection = config::switch_model_selection(&self.cfg, &self.selection.provider_id, id)?;
        self.selection = selection;
        Ok(())
    }

    /// 切换思考档位（REPL /thinking）：无效档位拒绝；更新 selection.thinking。
    /// 规则在 config::switch_thinking_selection。
    pub fn switch_thinking(&mut self, level: &str) -> Result<(), String> {
        let selection = config::switch_thinking_selection(&self.selection, level)?;
        self.selection = selection;
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

/// 解析并执行单个调用。参数 JSON / object 校验和未知工具由公共层负责；
/// 字段缺失、类型错误、多余字段、语义校验都在工具自己的 invoke 中完成。
fn dispatch_tool_call(
    ui: &mut dyn Ui,
    registry: &ToolRegistry,
    ctx: &ToolContext<'_>,
    call: &ToolCall,
) -> ToolResult {
    let tool = registry.get(&call.name);
    let parsed: Result<Value, _> = serde_json::from_str(&call.arguments);
    let args = match parsed {
        Ok(Value::Object(map)) => Value::Object(map),
        Ok(other) => {
            emit_tool_call(ui, &call.name, &other.to_string());
            return ToolResult::failure(format!(
                "工具参数解析失败：arguments 必须是 JSON object，收到 {}",
                json_kind(&other)
            ));
        }
        Err(e) => {
            emit_tool_call(ui, &call.name, &call.arguments);
            return ToolResult::failure(format!(
                "工具参数解析失败：{e}；原始参数：{}",
                call.arguments
            ));
        }
    };

    let summary = tool
        .map(|tool| tool.summary(&args))
        .unwrap_or_else(|| args.to_string());
    emit_tool_call(ui, &call.name, &summary);

    let Some(tool) = tool else {
        return ToolResult::failure(format!("未知工具：{}", call.name));
    };
    tool.invoke(args, ctx)
}

fn emit_tool_call(ui: &mut dyn Ui, name: &str, summary: &str) {
    emit(
        ui,
        RenderEvent::ToolCall {
            name: name.to_string(),
            summary: crate::tools::truncate_chars(summary, 200),
        },
    );
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// 信号触发的接收收尾：放弃接收（drop Receiver）后判定方向。
/// 收尾间隙可能再收到一次 Ctrl-C（cancel 升级为 quit），以最终状态为准。
fn signal_outcome(
    ui: &mut dyn Ui,
    sig: SignalState,
    cancel: &Cancel,
    cancelled_msg: &str,
) -> StreamOutcome {
    if sig == SignalState::Quit || cancel.is_quit() {
        return StreamOutcome::Quit;
    }
    emit(ui, RenderEvent::Error(cancelled_msg.into()));
    StreamOutcome::Cancelled
}

/// 渲染事件转发（agent loop → ui.print）。
fn emit(ui: &mut dyn Ui, ev: RenderEvent) {
    ui.print(&ev);
}
