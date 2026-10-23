//! 供应商适配层：trait、按 protocol 分发、流式请求工作线程。

mod openai;

pub use openai::OpenAiChatAdapter;

use crate::chat::Msg;
use crate::config::{Model, Provider};
use serde_json::Value;

/// 流事件（worker → 主线程，经无界 mpsc）。
#[derive(Debug)]
pub enum StreamEvent {
    /// 渲染用增量
    Delta {
        text: Option<String>,
        reasoning: Option<String>,
    },
    ToolDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        args_frag: String,
    },
    Usage {
        prompt: u64,
        completion: u64,
    },
    /// [DONE] 触发；仅记录提交资格（stop|tool_calls|None），不退出接收
    Done {
        finish_reason: Option<String>,
    },
    /// worker 组装完成的完整 Assistant 消息；收到它才退出接收并提交
    Final(Box<Msg>),
    /// HTTP 错误 / 断流 / 非法 JSON / finish_reason=length|content_filter 等截断
    Error(String),
}

/// 每流累加器（归 worker 线程所有）：按 index 组装 tool_calls、累积 text/reasoning。
#[derive(Debug, Default)]
pub struct TurnAcc {
    pub text: Option<String>,
    pub reasoning: Option<String>,
    /// (index → 组装中的调用)
    pub tool_calls: std::collections::BTreeMap<usize, (Option<String>, Option<String>, String)>,
}

impl TurnAcc {
    /// 流正常结束时组装为完整 Assistant 消息（tool_calls 按 index 排序）。
    pub fn into_msg(self) -> Msg {
        // TODO: text/reasoning None 化空；tool_calls 组装 Vec<ToolCall>（id 非空校验在主线程提交前做）
        todo!()
    }
}

pub trait ProviderAdapter {
    fn endpoint(&self, p: &Provider) -> String;
    fn build_body(&self, hist: &[Msg], model: &Model, thinking: Option<&Value>) -> Value;
    fn headers(&self, p: &Provider) -> Vec<(String, String)>;
    fn parse_chunk(&self, chunk: &Value, acc: &mut TurnAcc) -> Vec<StreamEvent>;
}

/// 按 protocol 分发；v1 只实现 openai_chat，其余报"不支持"。
pub fn adapter_for_protocol(protocol: &str) -> Box<dyn ProviderAdapter> {
    match protocol {
        "openai_chat" => Box::new(OpenAiChatAdapter),
        // Responses / Anthropic Messages：接口预留，v1 遇到即报不支持
        other => unimplemented!("不支持的协议族：{other}"),
    }
}

/// 流式请求工作线程入口：持 TurnAcc 逐 chunk 解析，事件发 channel。
/// 契约：任何结束路径必以 Final 或 Error 收尾后才退出（主线程收到
/// Disconnected 即防御性按 Error 收尾）。
pub fn stream_request(
    adapter: Box<dyn ProviderAdapter>,
    provider: Provider,
    body: Value,
    headers: Vec<(String, String)>,
    tx: std::sync::mpsc::Sender<StreamEvent>,
) {
    // TODO: ureq POST（连接 10s / 首字节 60s / 流式空闲 120s；代理环境变量；SSL_CERT_FILE）
    // TODO: 逐行读 SSE → sse::parse → adapter.parse_chunk(acc) → 事件 send
    // TODO: [DONE] → finish_reason 合格则 acc.into_msg() 发 Final，否则 Error
    // TODO: 任何 HTTP/解析/断流错误 → Error 收尾后 return（不 panic）
    // TODO: send 报错（主线程已 drop Receiver）→ 直接退出，无需排空
    let _ = (adapter, provider, body, headers, tx);
    todo!()
}
