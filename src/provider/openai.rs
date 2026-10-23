//! OpenAI Chat Completions 适配器。见 design.md 六节。

use super::{ProviderAdapter, StreamEvent, TurnAcc};
use crate::chat::Msg;
use crate::config::{Model, Provider};
use serde_json::{json, Value};

pub struct OpenAiChatAdapter;

impl ProviderAdapter for OpenAiChatAdapter {
    fn endpoint(&self, p: &Provider) -> String {
        // TODO: base_url + "/chat/completions"
        let _ = p;
        todo!()
    }

    fn build_body(&self, hist: &[Msg], model: &Model, thinking: Option<&Value>) -> Value {
        // TODO: messages 映射（System/User/Assistant/Tool → 协议字段）
        // TODO: reasoning 默认不回传（丢弃 Assistant.reasoning），
        //       echo_reasoning=true 才随历史回传（compat 配置经 Provider 传入或闭包捕获）
        // TODO: 上游 model = api_model 或 id 第一个 '/' 之后
        // TODO: 始终发送 tools 定义（bash）；不设置 tool_choice
        // TODO: thinking 片段顶层合并进 body
        // TODO: stream: true
        let _ = (hist, model, thinking);
        todo!()
    }

    fn headers(&self, p: &Provider) -> Vec<(String, String)> {
        // TODO: Content-Type/Authorization 等（auth.headers 已在配置层展开）
        let _ = p;
        todo!()
    }

    fn parse_chunk(&self, chunk: &Value, acc: &mut TurnAcc) -> Vec<StreamEvent> {
        // TODO: delta.content / delta.reasoning_content → Delta
        // TODO: delta.tool_calls[i] → ToolDelta（index/id/name/arguments 分片）
        // TODO: usage → Usage（取自最后一个 chunk）
        // TODO: choices[0].finish_reason → Done（length/content_filter 在此处或上层转 Error）
        let _ = (chunk, acc);
        todo!()
    }
}

/// thinking 片段顶层合并（对象浅合并；片段须为对象，否则配置校验报错）。
#[allow(dead_code)]
fn merge_thinking(body: &mut Value, fragment: &Value) {
    // TODO: 若 fragment 为对象，逐键浅合并进 body 顶层
    let _ = (body, fragment);
    todo!()
}

#[allow(dead_code)]
fn _unused(_: &Value) {}
#[allow(dead_code)]
fn _unused2(_: &Value) {}
