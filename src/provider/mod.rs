//! 供应商适配层：trait、按 Provider 构造（注入 compat）并分发、流式请求工作线程。

mod openai;

pub use openai::OpenAiChatAdapter;

use std::sync::Arc;

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

/// 适配器需要跨线程进 worker：Arc 共享（每轮 clone 按值传入），要求 Send + Sync。
pub trait ProviderAdapter: Send + Sync {
    fn endpoint(&self, p: &Provider) -> String;
    /// `model_id` 是全局唯一 id（如 deepseek/deepseek-v4-pro）：上游 model 字段
    /// 默认取其第一个 '/' 之后的部分，`Model::api_model` 可覆盖。
    fn build_body(
        &self,
        hist: &[Msg],
        model: &Model,
        model_id: &str,
        thinking: Option<&Value>,
    ) -> Value;
    fn headers(&self, p: &Provider) -> Vec<(String, String)>;
    fn parse_chunk(&self, chunk: &Value, acc: &mut TurnAcc) -> Vec<StreamEvent>;
}

/// 按 Provider 构造适配器：compat 开关（echo_reasoning 等）在构造时注入；
/// protocol 不认识则报"不支持"（可展示错误，非 panic）。v1 只实现 openai_chat。
pub fn adapter_for(provider: &Provider) -> Result<Arc<dyn ProviderAdapter>, String> {
    match provider.protocol.as_str() {
        "openai_chat" => Ok(Arc::new(OpenAiChatAdapter::new(provider.compat.echo_reasoning))),
        // Responses / Anthropic Messages：接口预留，v1 报不支持（exit 2 由调用方映射）
        other => Err(format!("不支持的协议族：{other}（v1 只实现 openai_chat）")),
    }
}

/// 流式请求工作线程入口：持 TurnAcc 逐 chunk 解析，事件发 channel。
/// 契约：任何结束路径必以 Final 或 Error 收尾后才退出（主线程收到
/// Disconnected 即防御性按 Error 收尾）。
pub fn stream_request(
    adapter: Arc<dyn ProviderAdapter>,
    provider: Provider,
    body: Value,
    headers: Vec<(String, String)>,
    tx: std::sync::mpsc::Sender<StreamEvent>,
) {
    // TODO: ureq Agent：自建 rustls::ClientConfig（webpki-roots + SSL_CERT_FILE 覆盖），
    //       AgentBuilder::tls_config；代理自行读 HTTP(S)_PROXY/ALL_PROXY（大小写均认）
    //       并按 NO_PROXY 分流（ureq 无 NO_PROXY 支持，不依赖 proxy-from-env feature）
    // TODO: 超时：连接 10s（timeout_connect）；首字节 60s 与流式空闲 120s 在读取循环
    //       自行计时（ureq 只有 connect/per-read 两档，无法直接区分首字节与空闲）
    // TODO: POST → 逐行读 SSE → sse::parse → adapter.parse_chunk(acc) → 事件 send
    // TODO: [DONE] → finish_reason 合格则 acc.into_msg() 发 Final，否则 Error
    // TODO: 任何 HTTP/解析/断流错误 → Error 收尾后 return（不 panic）
    // TODO: send 报错（主线程已 drop Receiver）→ 直接退出，无需排空
    let _ = (adapter, provider, body, headers, tx);
    todo!()
}
