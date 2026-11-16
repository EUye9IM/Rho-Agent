//! 一批 tool_calls 的执行：限额/取消/终端失败前置检查、单调用结果提交与占位补齐。
//! 历史配对不变式在这里维护：Assistant 提交后，每个 tool_call 都有对应 Tool 消息。

use super::{emit, Session, ToolCall};
use crate::message::Msg;
use crate::render::RenderEvent;
use crate::term::{Cancel, SignalState};
use crate::text::truncate_chars;
use crate::tools::{ToolContext, ToolRegistry, ToolResult, ToolStatus};
use crate::ui::Ui;
use serde_json::Value;

const DEFAULT_MAX_TOOL_STEPS: u32 = 100;

/// 取消/退出时未执行调用的占位文本。
const CANCELLED_PLACEHOLDER: &str = "用户取消执行";
/// 步数超限时未执行调用的占位文本。
const LIMIT_PLACEHOLDER: &str = "工具调用次数已达上限，未执行";
/// 终端输出失败时未执行调用的占位文本（exit 1 路径）。
const FATAL_PLACEHOLDER: &str = "终端输出失败，未执行";

/// 一批 tool_calls 的执行阶段结果（`Session::run_tool_calls` 的返回值）。
/// Cancelled/Quit/Limit/Fatal 的占位补齐已在 run_tool_calls 内完成。
#[derive(Debug)]
pub(super) enum ToolPhase {
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
    /// 执行一批 tool_calls：每个执行前先做前置检查；全部执行完返回 Done。
    /// 任何返回路径都保证剩余调用已补占位：Assistant 的每个 tool_call 都有对应 Tool 消息。
    /// 结果写入与计数集中在 commit_call_result：单调用分支再多也不会漏掉历史配对。
    pub(super) fn run_tool_calls(
        &mut self,
        ui: &mut dyn Ui,
        cancel: &Cancel,
        steps: &mut u32,
        tool_calls: &[ToolCall],
    ) -> ToolPhase {
        let max = self.cfg.max_tool_steps.unwrap_or(DEFAULT_MAX_TOOL_STEPS);
        let mut i = 0;

        while i < tool_calls.len() {
            // 执行前三查：超限、取消/退出、终端写失败。命中即补占位并结束批次。
            if let Some(phase) = self.check_before_call(ui, cancel, *steps, max, &tool_calls[i..]) {
                return phase;
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

            // 工具被取消（当前结果已记录）或执行/清理期间升级为 quit：
            // 剩余调用统一补占位后判定方向。
            if interrupted || cancel.is_quit() {
                return self.cancel_remaining(ui, cancel, &tool_calls[i + 1..]);
            }
            i += 1;
        }

        // 全部工具执行完。
        ToolPhase::Done
    }

    /// 单个调用执行前的前置检查：超限、取消/退出、终端写失败任一命中时，
    /// 剩余调用（含当前）补对应占位并返回该阶段结果；全部通过返回 None。
    fn check_before_call(
        &mut self,
        ui: &mut dyn Ui,
        cancel: &Cancel,
        steps: u32,
        max: u32,
        remaining: &[ToolCall],
    ) -> Option<ToolPhase> {
        // 限额逐个前置检查：剩余调用补超限占位。
        if steps >= max {
            self.placeholders(ui, remaining, LIMIT_PLACEHOLDER, ToolStatus::Failure);
            return Some(ToolPhase::Limit(max));
        }

        if let Some(sig) = cancel.pending() {
            self.placeholders(
                ui,
                remaining,
                CANCELLED_PLACEHOLDER,
                ToolStatus::Interrupted,
            );
            return Some(match sig {
                SignalState::Quit => ToolPhase::Quit,
                _ => ToolPhase::Cancelled,
            });
        }

        // 终端写失败：剩余调用补占位后上抛（exit 1；占位写入可能同样失败，
        // 但历史配对仍由本函数保证）。
        if let Some(e) = ui.take_fatal() {
            self.placeholders(ui, remaining, FATAL_PLACEHOLDER, ToolStatus::Failure);
            return Some(ToolPhase::Fatal(e));
        }
        None
    }

    /// 执行/清理期间发现取消或退出后的唯一收尾：剩余调用补取消占位，
    /// 再按当前信号状态判定 Cancelled / Quit。
    fn cancel_remaining(
        &mut self,
        ui: &mut dyn Ui,
        cancel: &Cancel,
        remaining: &[ToolCall],
    ) -> ToolPhase {
        self.placeholders(
            ui,
            remaining,
            CANCELLED_PLACEHOLDER,
            ToolStatus::Interrupted,
        );
        if cancel.is_quit() {
            ToolPhase::Quit
        } else {
            ToolPhase::Cancelled
        }
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
            summary: truncate_chars(summary, 200),
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
