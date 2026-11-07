//! OpenAI Responses API 适配器：请求体组装、流式事件解析、最终状态归一化。

use super::common::{default_headers, merge_thinking, model_name};
use super::{ChunkOutcome, FinishReason, ProviderAdapter, TurnAcc};
use crate::config::{Model, Provider};
use crate::message::Msg;
use crate::tool_spec;
use serde_json::{json, Value};

pub struct OpenAiResponsesAdapter;

impl OpenAiResponsesAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for OpenAiResponsesAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderAdapter for OpenAiResponsesAdapter {
    fn endpoint(&self, p: &Provider) -> String {
        format!("{}/responses", p.base_url.trim_end_matches('/'))
    }

    fn build_body(
        &self,
        hist: &[Msg],
        model: &Model,
        model_id: &str,
        thinking: Option<&Value>,
    ) -> Value {
        let mut instructions: Vec<&str> = Vec::new();
        let mut input: Vec<Value> = Vec::new();

        for msg in hist {
            match msg {
                Msg::System(text) => instructions.push(text),
                Msg::User(text) => input.push(json!({ "role": "user", "content": text })),
                Msg::Assistant(a) => {
                    if let Some(text) = &a.text {
                        input.push(json!({ "role": "assistant", "content": text }));
                    }
                    // Responses API 的 function_call 是独立 input item，call_id 用于配对结果。
                    for call in &a.tool_calls {
                        input.push(json!({
                            "type": "function_call",
                            "call_id": call.id,
                            "name": call.name,
                            "arguments": call.arguments,
                        }));
                    }
                }
                Msg::Tool {
                    tool_call_id,
                    content,
                } => input.push(json!({
                    "type": "function_call_output",
                    "call_id": tool_call_id,
                    "output": content,
                })),
            }
        }

        let spec = tool_spec::bash();
        let mut body = json!({
            "model": model_name(model, model_id),
            "input": input,
            "stream": true,
            // 模型声明的输出上限直接编进请求，避免上游超长回复。
            "max_output_tokens": model.max_output,
            "tools": [{
                "type": "function",
                "name": spec.name,
                "description": spec.description,
                "parameters": spec.parameters,
            }]
        });
        if !instructions.is_empty() {
            body["instructions"] = json!(instructions.join("\n\n"));
        }
        if let Some(fragment) = thinking {
            merge_thinking(&mut body, fragment);
        }
        body
    }

    fn headers(&self, p: &Provider) -> Vec<(String, String)> {
        default_headers(p)
    }

    fn is_stream_end(&self, payload: &str) -> bool {
        payload.trim() == "[DONE]"
    }

    fn parse_chunk(&self, chunk: &Value, acc: &mut TurnAcc) -> ChunkOutcome {
        match chunk.get("type").and_then(Value::as_str).unwrap_or("") {
            "response.output_text.delta" | "response.refusal.delta" => {
                delta_event(chunk, acc, DeltaKind::Text)
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                delta_event(chunk, acc, DeltaKind::Reasoning)
            }
            "response.output_text.done" | "response.refusal.done" => {
                if let Some(text) = chunk
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    acc.text = Some(text.to_string());
                }
                ChunkOutcome::Continue
            }
            "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {
                if let Some(text) = chunk
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    acc.reasoning = Some(text.to_string());
                }
                ChunkOutcome::Continue
            }
            "response.output_item.added" => {
                record_function_call_item(chunk, acc);
                ChunkOutcome::Continue
            }
            "response.function_call_arguments.delta" => {
                let index = output_index(chunk);
                if let Some(delta) = chunk.get("delta").and_then(Value::as_str) {
                    acc.tool_calls
                        .entry(index)
                        .or_default()
                        .arguments
                        .push_str(delta);
                }
                ChunkOutcome::Continue
            }
            // 最终事件可能携带完整 output；先吸收再结束，避免只认字符串标记。
            "response.completed" | "response.incomplete" => {
                if let Some(error) = response_error(chunk) {
                    return ChunkOutcome::Error(error);
                }
                finalize_response(chunk, acc);
                ChunkOutcome::Done
            }
            "response.failed" => match response_error(chunk) {
                Some(error) => ChunkOutcome::Error(error),
                None => ChunkOutcome::Error("上游返回失败状态".to_string()),
            },
            "error" => {
                let message = chunk
                    .get("message")
                    .and_then(Value::as_str)
                    .or_else(|| chunk.pointer("/error/message").and_then(Value::as_str))
                    .unwrap_or("上游返回错误");
                ChunkOutcome::Error(format!("上游返回错误：{message}"))
            }
            _ => ChunkOutcome::Continue,
        }
    }
}

enum DeltaKind {
    Text,
    Reasoning,
}

fn delta_event(chunk: &Value, acc: &mut TurnAcc, kind: DeltaKind) -> ChunkOutcome {
    let Some(delta) = chunk
        .get("delta")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return ChunkOutcome::Continue;
    };
    match kind {
        DeltaKind::Text => {
            acc.text.get_or_insert_with(String::new).push_str(delta);
            ChunkOutcome::Delta {
                text: Some(delta.to_string()),
                reasoning: None,
            }
        }
        DeltaKind::Reasoning => {
            acc.reasoning
                .get_or_insert_with(String::new)
                .push_str(delta);
            ChunkOutcome::Delta {
                text: None,
                reasoning: Some(delta.to_string()),
            }
        }
    }
}

/// function_call 的元数据在 output_item.added 中；参数增量随后按 output_index 归位。
fn record_function_call_item(chunk: &Value, acc: &mut TurnAcc) {
    let Some(item) = chunk.get("item") else {
        return;
    };
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return;
    }
    let index = output_index(chunk);
    let slot = acc.tool_calls.entry(index).or_default();
    if let Some(id) = item
        .get("call_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        slot.id = Some(id.to_string());
    }
    if let Some(name) = item
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        slot.name = Some(name.to_string());
    }
    if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
        if !arguments.is_empty() && slot.arguments.is_empty() {
            slot.arguments.push_str(arguments);
        }
    }
}

/// 最终 response 对象可能携带完整 output；只补缺失，避免与增量重复。
fn finalize_response(chunk: &Value, acc: &mut TurnAcc) {
    let response = chunk.get("response").unwrap_or(chunk);
    if let Some(output) = response.get("output").and_then(Value::as_array) {
        for (fallback_index, item) in output.iter().enumerate() {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => fill_text(item, acc),
                Some("function_call") => fill_function_call(item, fallback_index, acc),
                Some("reasoning") => fill_reasoning(item, acc),
                _ => {}
            }
        }
    }

    let status = response.get("status").and_then(Value::as_str);
    let incomplete = response
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str);
    acc.finish_reason = Some(match status {
        Some("incomplete") => match incomplete {
            Some("max_output_tokens") => FinishReason::Length,
            Some("content_filter") => FinishReason::ContentFilter,
            Some(other) => FinishReason::Other(other.to_string()),
            None => FinishReason::Other("incomplete".to_string()),
        },
        Some("failed") => FinishReason::Other("failed".to_string()),
        _ if !acc.tool_calls.is_empty() => FinishReason::ToolCalls,
        _ => FinishReason::Stop,
    });
}

fn fill_text(item: &Value, acc: &mut TurnAcc) {
    if acc.text.is_some() {
        return;
    }
    let Some(parts) = item.get("content").and_then(Value::as_array) else {
        return;
    };
    let mut text = String::new();
    for part in parts {
        if part.get("type").and_then(Value::as_str) == Some("output_text") {
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                text.push_str(t);
            }
        }
    }
    if !text.is_empty() {
        acc.text = Some(text);
    }
}

fn fill_reasoning(item: &Value, acc: &mut TurnAcc) {
    if acc.reasoning.is_some() {
        return;
    }
    let Some(summary) = item.get("summary").and_then(Value::as_array) else {
        return;
    };
    let mut text = String::new();
    for part in summary {
        if let Some(t) = part.get("text").and_then(Value::as_str) {
            text.push_str(t);
        }
    }
    if !text.is_empty() {
        acc.reasoning = Some(text);
    }
}

fn fill_function_call(item: &Value, fallback_index: usize, acc: &mut TurnAcc) {
    let call_id = item.get("call_id").and_then(Value::as_str);
    // 最终 output 不一定带 output_index；优先按 call_id 找回增量阶段建立的槽位。
    let existing = call_id.and_then(|id| {
        acc.tool_calls
            .iter()
            .find(|(_, slot)| slot.id.as_deref() == Some(id))
            .map(|(&index, _)| index)
    });
    let index = existing
        .or_else(|| {
            item.get("output_index")
                .and_then(Value::as_u64)
                .map(|v| v as usize)
        })
        .unwrap_or(fallback_index);
    let slot = acc.tool_calls.entry(index).or_default();
    if slot.id.is_none() {
        if let Some(id) = item.get("call_id").and_then(Value::as_str) {
            slot.id = Some(id.to_string());
        }
    }
    if slot.name.is_none() {
        if let Some(name) = item.get("name").and_then(Value::as_str) {
            slot.name = Some(name.to_string());
        }
    }
    if slot.arguments.is_empty() {
        if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
            slot.arguments.push_str(arguments);
        }
    }
}

fn output_index(chunk: &Value) -> usize {
    chunk
        .get("output_index")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(0)
}

fn response_error(chunk: &Value) -> Option<String> {
    let message = chunk
        .pointer("/response/error/message")
        .and_then(Value::as_str)
        .or_else(|| chunk.pointer("/error/message").and_then(Value::as_str))
        .or_else(|| chunk.get("message").and_then(Value::as_str));
    message.map(|m| format!("上游返回错误：{m}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CompatConfig;
    use crate::message::{AssistantMsg, ToolCall};

    fn provider() -> Provider {
        Provider {
            protocol: "openai_responses".into(),
            base_url: "https://api.example.com/".into(),
            auth: Default::default(),
            compat: CompatConfig::default(),
        }
    }

    fn model() -> Model {
        Model {
            api_model: None,
            context_window: 1,
            max_output: 1,
            thinking_default: None,
            thinking: Default::default(),
        }
    }

    #[test]
    fn endpoint_trims_slash() {
        let a = OpenAiResponsesAdapter::new();
        assert_eq!(a.endpoint(&provider()), "https://api.example.com/responses");
    }

    #[test]
    fn body_maps_history_and_thinking() {
        let a = OpenAiResponsesAdapter::new();
        let hist = vec![
            Msg::System("s".into()),
            Msg::User("u".into()),
            Msg::Assistant(AssistantMsg {
                text: Some("t".into()),
                reasoning: Some("r".into()),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "bash".into(),
                    arguments: "{\"command\":\"ls\"}".into(),
                }],
            }),
            Msg::Tool {
                tool_call_id: "call_1".into(),
                content: "ok".into(),
            },
        ];
        let thinking = json!({ "reasoning": { "effort": "low" } });
        let body = a.build_body(&hist, &model(), "p/m", Some(&thinking));
        assert_eq!(body["model"], "m");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_output_tokens"], 1);
        assert_eq!(body["instructions"], "s");
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][1]["role"], "assistant");
        assert_eq!(body["input"][2]["type"], "function_call");
        assert_eq!(body["input"][2]["call_id"], "call_1");
        assert_eq!(body["input"][3]["type"], "function_call_output");
        assert_eq!(body["tools"][0]["name"], "bash");
    }

    #[test]
    fn parses_text_and_reasoning_deltas() {
        let a = OpenAiResponsesAdapter::new();
        let mut acc = TurnAcc::default();
        let text = json!({ "type": "response.output_text.delta", "delta": "hi" });
        assert!(matches!(
            a.parse_chunk(&text, &mut acc),
            ChunkOutcome::Delta {
                text: Some(_),
                reasoning: None
            }
        ));
        let reasoning = json!({
            "type": "response.reasoning_summary_text.delta",
            "delta": "think"
        });
        assert!(matches!(
            a.parse_chunk(&reasoning, &mut acc),
            ChunkOutcome::Delta {
                text: None,
                reasoning: Some(_)
            }
        ));
        assert_eq!(acc.text.as_deref(), Some("hi"));
        assert_eq!(acc.reasoning.as_deref(), Some("think"));
    }

    #[test]
    fn assembles_function_call_from_events() {
        let a = OpenAiResponsesAdapter::new();
        let mut acc = TurnAcc::default();
        let added = json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "type": "function_call",
                "call_id": "call_1",
                "name": "bash",
                "arguments": ""
            }
        });
        assert!(matches!(
            a.parse_chunk(&added, &mut acc),
            ChunkOutcome::Continue
        ));
        let delta = json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "delta": "{\"command\":\"ls\"}"
        });
        assert!(matches!(
            a.parse_chunk(&delta, &mut acc),
            ChunkOutcome::Continue
        ));
        let done = json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [{
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "bash",
                    "arguments": "{\"command\":\"ls\"}"
                }]
            }
        });
        assert!(matches!(a.parse_chunk(&done, &mut acc), ChunkOutcome::Done));
        assert_eq!(acc.finish_reason, Some(FinishReason::ToolCalls));
        let msg = acc.into_msg();
        assert_eq!(msg.tool_calls.len(), 1);
        assert_eq!(msg.tool_calls[0].id, "call_1");
        assert_eq!(msg.tool_calls[0].arguments, "{\"command\":\"ls\"}");
    }

    #[test]
    fn final_output_fills_text_without_deltas() {
        let a = OpenAiResponsesAdapter::new();
        let mut acc = TurnAcc::default();
        let done = json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [{
                    "type": "message",
                    "content": [{ "type": "output_text", "text": "ok" }]
                }]
            }
        });
        assert!(matches!(a.parse_chunk(&done, &mut acc), ChunkOutcome::Done));
        assert_eq!(acc.finish_reason, Some(FinishReason::Stop));
        assert_eq!(acc.into_msg().text.as_deref(), Some("ok"));
    }

    #[test]
    fn incomplete_maps_to_length() {
        let a = OpenAiResponsesAdapter::new();
        let mut acc = TurnAcc::default();
        let done = json!({
            "type": "response.incomplete",
            "response": {
                "status": "incomplete",
                "incomplete_details": { "reason": "max_output_tokens" }
            }
        });
        assert!(matches!(a.parse_chunk(&done, &mut acc), ChunkOutcome::Done));
        assert_eq!(acc.finish_reason, Some(FinishReason::Length));
    }

    #[test]
    fn failed_maps_to_error() {
        let a = OpenAiResponsesAdapter::new();
        let mut acc = TurnAcc::default();
        let failed = json!({
            "type": "response.failed",
            "response": { "error": { "message": "boom" } }
        });
        assert!(matches!(
            a.parse_chunk(&failed, &mut acc),
            ChunkOutcome::Error(msg) if msg.contains("boom")
        ));
    }
}
