//! 一轮回合：流接收、逐块取消判定与回合终态。
//! 工具批次的执行与占位补齐在 `batch`；这里只负责流与回合控制流。

use super::batch::ToolPhase;
use super::{emit, ChatResult, Quit, Session, TurnEnd};
use crate::message::{AssistantMsg, Msg, TokenUsage};
use crate::provider::StreamEvent;
use crate::render::{RenderEvent, StatusLine};
use crate::term::{Cancel, SignalState};
use crate::ui::Ui;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

/// 流式接收被取消时展示的提示。
const TURN_CANCELLED_MSG: &str = "已取消本轮响应";

/// 一次流式请求的接收结果（`Session::receive_stream` 的返回值）。
#[derive(Debug)]
pub(super) enum StreamOutcome {
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

impl Session {
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
        // 用户输入已入历史：先落盘，后面即使请求失败也能恢复到这一轮。
        self.save(ui);

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
                // 回合终态：历史里没有未配对的 tool_calls。
                self.save(ui);
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

            let phase = self.run_tool_calls(ui, cancel, &mut steps, &tool_calls);
            // 一批工具调用返回时，所有 tool_calls 都已配上 Tool 消息（含占位），
            // 历史回到合法边界：这是唯一写盘点，不在单个结果提交后写。
            self.save(ui);

            match phase {
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
    pub(super) fn drive_stream(
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
}

/// 一次流式请求的主线程侧句柄：接收器与取消标志绑定在一起。
/// `abandon` 是唯一的放弃路径：置位取消标志（worker 轮询它即静默退出）并 drop
/// Receiver；正常收尾只需让 handle 离开作用域。
struct StreamHandle {
    rx: Receiver<StreamEvent>,
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
