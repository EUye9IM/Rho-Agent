//! OpenAI Chat Completions 适配器。见 docs/architecture.md 四节。

use super::{ProviderAdapter, StreamEvent, TurnAcc};
use crate::chat::Msg;
use crate::config::{Model, Provider};
use serde_json::{json, Map, Value};

pub struct OpenAiChatAdapter {
    /// compat 开关在构造时注入（echo_reasoning 等）
    pub echo_reasoning: bool,
}

impl OpenAiChatAdapter {
    /// compat 开关注入：echo_reasoning=true 时 reasoning_content 随历史回传。
    pub fn new(echo_reasoning: bool) -> Self {
        Self { echo_reasoning }
    }
}

impl ProviderAdapter for OpenAiChatAdapter {
    fn endpoint(&self, p: &Provider) -> String {
        format!("{}/chat/completions", p.base_url.trim_end_matches('/'))
    }

    fn build_body(
        &self,
        hist: &[Msg],
        model: &Model,
        model_id: &str,
        thinking: Option<&Value>,
    ) -> Value {
        let messages: Vec<Value> = hist
            .iter()
            .map(|m| match m {
                Msg::System(text) => json!({ "role": "system", "content": text }),
                Msg::User(text) => json!({ "role": "user", "content": text }),
                Msg::Assistant { text, reasoning, tool_calls } => {
                    let mut obj = Map::new();
                    obj.insert("role".into(), json!("assistant"));
                    obj.insert(
                        "content".into(),
                        text.as_ref().map(|t| json!(t)).unwrap_or(Value::Null),
                    );
                    if self.echo_reasoning {
                        if let Some(r) = reasoning {
                            obj.insert("reasoning_content".into(), json!(r));
                        }
                    }
                    if !tool_calls.is_empty() {
                        let calls: Vec<Value> = tool_calls
                            .iter()
                            .map(|c| {
                                json!({
                                    "id": c.id,
                                    "type": "function",
                                    "function": {
                                        "name": c.name,
                                        "arguments": c.arguments,
                                    }
                                })
                            })
                            .collect();
                        obj.insert("tool_calls".into(), Value::Array(calls));
                    }
                    Value::Object(obj)
                }
                Msg::Tool { tool_call_id, content } => {
                    json!({ "role": "tool", "tool_call_id": tool_call_id, "content": content })
                }
            })
            .collect();

        let model_name = model
            .api_model
            .clone()
            .unwrap_or_else(|| model_id.split_once('/').map(|(_, m)| m).unwrap_or(model_id).to_string());

        let mut body = json!({
            "model": model_name,
            "messages": messages,
            "stream": true,
            // 模型声明的输出上限直接编进请求，避免上游超长回复。
            "max_tokens": model.max_output,
            "tools": [{
                "type": "function",
                "function": {
                    "name": "bash",
                    "description": "在用户机器上执行一条 bash 命令（无持久 shell，每次调用相互独立）。",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "command": {
                                "type": "string",
                                "description": "要执行的完整 bash 命令"
                            }
                        },
                        "required": ["command"]
                    }
                }
            }]
        });

        if let Some(fragment) = thinking {
            merge_thinking(&mut body, fragment);
        }
        body
    }

    fn headers(&self, p: &Provider) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = p
            .auth
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let has_content_type = out.iter().any(|(k, _)| k.eq_ignore_ascii_case("Content-Type"));
        let has_accept = out.iter().any(|(k, _)| k.eq_ignore_ascii_case("Accept"));
        if !has_content_type {
            out.push(("Content-Type".into(), "application/json".into()));
        }
        if !has_accept {
            out.push(("Accept".into(), "text/event-stream".into()));
        }
        out
    }

    /// 解析单个 chunk：累积进 acc；有 text/reasoning 增量时返回 Delta 事件。
    /// tool_calls 的组装只进 acc（不产生事件）；请求从不设置 n，choices 实际只有一条。
    fn parse_chunk(&self, chunk: &Value, acc: &mut TurnAcc) -> Option<StreamEvent> {
        let choices = chunk.get("choices")?.as_array()?;
        let mut delta_event = None;

        for (pos, choice) in choices.iter().enumerate() {
            if let Some(delta) = choice.get("delta").and_then(Value::as_object) {
                let text = delta
                    .get("content")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                let reasoning = delta
                    .get("reasoning_content")
                    .or_else(|| delta.get("reasoning"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                if text.is_some() || reasoning.is_some() {
                    if let Some(t) = &text {
                        acc.text.get_or_insert_with(String::new).push_str(t);
                    }
                    if let Some(r) = &reasoning {
                        acc.reasoning.get_or_insert_with(String::new).push_str(r);
                    }
                    delta_event = Some(StreamEvent::Delta { text, reasoning });
                }

                if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    for (fallback, call) in calls.iter().enumerate() {
                        let index = call
                            .get("index")
                            .and_then(Value::as_u64)
                            .map(|v| v as usize)
                            .unwrap_or_else(|| pos + fallback);
                        let id = call
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string);
                        let func = call.get("function");
                        let name = func
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string);
                        let args_frag = func
                            .and_then(|f| f.get("arguments"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let slot = acc.tool_calls.entry(index).or_default();
                        if let Some(id) = &id {
                            slot.0 = Some(id.clone());
                        }
                        if let Some(name) = &name {
                            slot.1 = Some(name.clone());
                        }
                        slot.2.push_str(args_frag);
                    }
                }
            }

            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                acc.finish_reason = Some(reason.to_string());
            }
        }

        delta_event
    }
}

/// thinking 片段顶层合并（对象浅合并；片段须为对象，否则配置校验报错）。
fn merge_thinking(body: &mut Value, fragment: &Value) {
    if let (Some(body), Some(fragment)) = (body.as_object_mut(), fragment.as_object()) {
        for (k, v) in fragment {
            body.insert(k.clone(), v.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CompatConfig;

    fn provider() -> Provider {
        Provider {
            protocol: "openai_chat".into(),
            base_url: "https://api.example.com/".into(),
            auth: Default::default(),
            compat: CompatConfig::default(),
        }
    }

    #[test]
    fn endpoint_trims_slash() {
        let a = OpenAiChatAdapter::new(false);
        assert_eq!(a.endpoint(&provider()), "https://api.example.com/chat/completions");
    }

    #[test]
    fn body_maps_messages_and_thinking() {
        let a = OpenAiChatAdapter::new(true);
        let model = Model {
            api_model: None,
            context_window: 1,
            max_output: 1,
            thinking_default: None,
            thinking: Default::default(),
        };
        let hist = vec![
            Msg::System("s".into()),
            Msg::User("u".into()),
            Msg::Assistant {
                text: Some("t".into()),
                reasoning: Some("r".into()),
                tool_calls: vec![],
            },
            Msg::Tool { tool_call_id: "id".into(), content: "ok".into() },
        ];
        let thinking = json!({ "thinking": { "type": "enabled" } });
        let body = a.build_body(&hist, &model, "p/m", Some(&thinking));
        assert_eq!(body["model"], "m");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 1);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["messages"][2]["reasoning_content"], "r");
        assert!(body["tools"].is_array());
    }

    #[test]
    fn parses_text_and_tool_calls() {
        let a = OpenAiChatAdapter::new(false);
        let mut acc = TurnAcc::default();
        let chunk = json!({
            "choices": [{
                "delta": {
                    "content": "hi",
                    "reasoning_content": "think",
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "function": { "name": "bash", "arguments": "{\"command\":" }
                    }]
                }
            }]
        });
        let ev = a.parse_chunk(&chunk, &mut acc);
        assert!(matches!(ev, Some(StreamEvent::Delta { .. })));

        let next = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": { "arguments": "\"ls\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        // tool_calls 片段只累积进 acc，不产生事件。
        assert!(a.parse_chunk(&next, &mut acc).is_none());
        assert_eq!(acc.finish_reason.as_deref(), Some("tool_calls"));
        match acc.into_msg() {
            Msg::Assistant { text, reasoning, tool_calls } => {
                assert_eq!(text.as_deref(), Some("hi"));
                assert_eq!(reasoning.as_deref(), Some("think"));
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].arguments, "{\"command\":\"ls\"}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
