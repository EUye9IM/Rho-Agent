//! 供应商适配层：trait、按 Provider 构造（注入 compat）并分发、流式请求工作线程。
//! TLS/代理/Agent 缓存等 HTTP 基础设施在 `http` 子模块。

mod http;
mod openai;

pub use openai::OpenAiChatAdapter;

use std::io::{ErrorKind, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::Duration;

use crate::chat::{Msg, ToolCall};
use crate::config::{Model, Provider};
use crate::sse::SseParser;
use serde_json::Value;

use http::{agent_for, shorten, MAX_ERROR_BODY_BYTES, MAX_RESPONSE_BYTES};

/// 流事件（worker → 主线程，经无界 mpsc）。
#[derive(Debug)]
pub enum StreamEvent {
    /// 渲染用增量（正文/思考文本）
    Delta {
        text: Option<String>,
        reasoning: Option<String>,
    },
    /// [DONE] 触发；此时 worker 已校验 finish_reason（合法才继续发 Final）
    Done,
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
    /// 最后一个 chunk 的 finish_reason；[DONE] 时据此判断提交资格
    pub finish_reason: Option<String>,
}

impl TurnAcc {
    /// 流正常结束时组装为完整 Assistant 消息（tool_calls 按 index 排序）。
    pub fn into_msg(self) -> Msg {
        let non_empty = |s: Option<String>| s.filter(|v| !v.is_empty());
        let tool_calls = self
            .tool_calls
            .into_values()
            .map(|(id, name, arguments)| ToolCall {
                id: id.unwrap_or_default(),
                name: name.unwrap_or_default(),
                arguments,
            })
            .collect();
        Msg::Assistant {
            text: non_empty(self.text),
            reasoning: non_empty(self.reasoning),
            tool_calls,
        }
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
    /// 解析单个 chunk：text/reasoning/tool_calls/finish_reason 累积进 `acc`；
    /// 有渲染增量（text/reasoning）时返回 `Some(Delta)`，否则 `None`。
    /// tool_calls 的组装只进 acc，不产生事件。
    fn parse_chunk(&self, chunk: &Value, acc: &mut TurnAcc) -> Option<StreamEvent>;
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

fn send(tx: &std::sync::mpsc::Sender<StreamEvent>, ev: StreamEvent) -> bool {
    tx.send(ev).is_ok()
}

fn send_error(tx: &std::sync::mpsc::Sender<StreamEvent>, msg: impl Into<String>) -> bool {
    send(tx, StreamEvent::Error(msg.into()))
}

enum PayloadOutcome {
    Continue,
    Done,
    Fatal,
}

fn handle_payload(
    adapter: &Arc<dyn ProviderAdapter>,
    acc: &mut TurnAcc,
    tx: &std::sync::mpsc::Sender<StreamEvent>,
    payload: &str,
) -> PayloadOutcome {
    let payload = payload.trim();
    if payload.is_empty() {
        return PayloadOutcome::Continue;
    }
    if payload == "[DONE]" {
        return PayloadOutcome::Done;
    }
    let chunk: Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(e) => {
            send_error(tx, format!("响应 JSON 解析失败：{e}"));
            return PayloadOutcome::Fatal;
        }
    };
    // 上游可能在 SSE 中发送 {"error": {...}}，不要静默丢弃。
    if let Some(error) = chunk.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string());
        send_error(tx, format!("上游返回错误：{message}"));
        return PayloadOutcome::Fatal;
    }
    if let Some(ev) = adapter.parse_chunk(&chunk, acc) {
        if !send(tx, ev) {
            return PayloadOutcome::Fatal;
        }
    }
    PayloadOutcome::Continue
}

/// 逐条处理载荷；见到 [DONE] 或发送失败即提前返回（剩余载荷丢弃）。
fn drain_payloads(
    adapter: &Arc<dyn ProviderAdapter>,
    acc: &mut TurnAcc,
    tx: &std::sync::mpsc::Sender<StreamEvent>,
    payloads: Vec<String>,
) -> PayloadOutcome {
    for payload in payloads {
        match handle_payload(adapter, acc, tx, &payload) {
            PayloadOutcome::Continue => {}
            early => return early,
        }
    }
    PayloadOutcome::Continue
}

/// 流式请求工作线程入口：持 TurnAcc 逐 chunk 解析，事件发 channel。
/// 契约：任何结束路径必以 Final 或 Error 收尾后才退出（主线程收到
/// Disconnected 即防御性按 Error 收尾）；`cancelled` 置位时静默返回（主线程已放弃接收）。
pub fn stream_request(
    adapter: Arc<dyn ProviderAdapter>,
    provider: Provider,
    body: Value,
    headers: Vec<(String, String)>,
    tx: std::sync::mpsc::Sender<StreamEvent>,
    cancelled: Arc<AtomicBool>,
) {
    let url = adapter.endpoint(&provider);
    let agent = match agent_for(&provider.base_url) {
        Ok(a) => a,
        Err(e) => {
            send_error(&tx, format!("无法初始化 HTTP 客户端：{e}"));
            return;
        }
    };

    let mut request = agent.post(&url);
    for (k, v) in &headers {
        request = request.set(k, v);
    }

    // 首字节 60s 看门狗：从发请求前开始计时，收到首个响应体字节后退出。
    // 超时后先给一段宽限再判：避免与 worker 的 `first_seen` 置位竞争而误报。
    let (first_tx, first_rx) = std::sync::mpsc::channel::<()>();
    let first_seen = Arc::new(AtomicBool::new(false));
    let aborted = Arc::new(AtomicBool::new(false));
    {
        let tx_watch = tx.clone();
        let first_seen_w = first_seen.clone();
        let aborted_w = aborted.clone();
        std::thread::spawn(move || match first_rx.recv_timeout(Duration::from_secs(60)) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
            Err(RecvTimeoutError::Timeout) => {
                std::thread::sleep(Duration::from_millis(250));
                if !first_seen_w.load(Ordering::SeqCst) {
                    aborted_w.store(true, Ordering::SeqCst);
                    let _ = tx_watch.send(StreamEvent::Error(
                        "等待响应首字节超时（60s）".into(),
                    ));
                }
            }
        });
    }

    let response = match request.send_string(&body.to_string()) {
        Ok(r) => r,
        Err(ureq::Error::Status(code, response)) => {
            // 错误体只读有限字节：上游可能返回很大/无限的内容。
            let mut raw = Vec::new();
            let _ = response
                .into_reader()
                .take(MAX_ERROR_BODY_BYTES)
                .read_to_end(&mut raw);
            let body = String::from_utf8_lossy(&raw);
            send_error(&tx, format!("HTTP {code}：{}", shorten(body.trim())));
            return;
        }
        Err(e) => {
            send_error(&tx, format!("请求失败：{e}"));
            return;
        }
    };

    let mut reader = response.into_reader();
    let mut parser = SseParser::new();
    let mut acc = TurnAcc::default();
    let mut buf = [0u8; 8192];
    let mut saw_done = false;
    let mut total_bytes: u64 = 0;

    // 提前退出：主线程已放弃接收（取消/退出）。
    let stop = |cancelled: &AtomicBool| cancelled.load(Ordering::SeqCst);

    'stream: loop {
        if stop(&cancelled) || aborted.load(Ordering::SeqCst) {
            return;
        }
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if !first_seen.swap(true, Ordering::SeqCst) {
                    let _ = first_tx.send(());
                }
                n
            }
            // SIGINT 可能投递到 worker 线程；EINTR 不是流错误，继续读。
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            // 空闲超时（统一 120s）：先查取消，否则按流错误收尾。
            Err(ref e) if e.kind() == ErrorKind::TimedOut || e.kind() == ErrorKind::WouldBlock => {
                if stop(&cancelled) || aborted.load(Ordering::SeqCst) {
                    return;
                }
                send_error(&tx, "读取响应流超时（120s 无数据）");
                return;
            }
            Err(e) => {
                if stop(&cancelled) || aborted.load(Ordering::SeqCst) {
                    return;
                }
                send_error(&tx, format!("读取响应流失败：{e}"));
                return;
            }
        };
        total_bytes += n as u64;
        if total_bytes > MAX_RESPONSE_BYTES {
            send_error(
                &tx,
                format!("响应体超过上限（{} 字节），已终止", MAX_RESPONSE_BYTES),
            );
            return;
        }
        if stop(&cancelled) || aborted.load(Ordering::SeqCst) {
            return;
        }
        let payloads = match parser.feed(&buf[..n]) {
            Ok(payloads) => payloads,
            Err(e) => {
                send_error(&tx, e);
                return;
            }
        };
        match drain_payloads(&adapter, &mut acc, &tx, payloads) {
            PayloadOutcome::Continue => {}
            PayloadOutcome::Done => {
                saw_done = true;
                break 'stream;
            }
            PayloadOutcome::Fatal => return,
        }
    }

    if stop(&cancelled) || aborted.load(Ordering::SeqCst) {
        return;
    }
    if !saw_done {
        // EOF 前没有换行结尾的最后一条 data: 也要处理。
        match drain_payloads(&adapter, &mut acc, &tx, parser.finish()) {
            PayloadOutcome::Continue => {}
            PayloadOutcome::Done => saw_done = true,
            PayloadOutcome::Fatal => return,
        }
    }
    if !saw_done {
        send_error(&tx, "响应流意外结束（未收到 [DONE]）");
        return;
    }
    if let Some(reason) = acc.finish_reason.as_deref() {
        if reason != "stop" && reason != "tool_calls" {
            send_error(&tx, format!("上游以截断原因结束响应：{reason}"));
            return;
        }
    }
    if !send(&tx, StreamEvent::Done) {
        return;
    }
    send(&tx, StreamEvent::Final(Box::new(acc.into_msg())));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn acc_assembles_by_index() {
        let mut acc = TurnAcc { text: Some("hi".into()), ..Default::default() };
        acc.tool_calls.insert(1, (Some("b".into()), Some("bash".into()), "{\"command\":\"b\"}".into()));
        acc.tool_calls.insert(0, (Some("a".into()), Some("bash".into()), "{\"command\":\"a\"}".into()));
        match acc.into_msg() {
            Msg::Assistant { text, tool_calls, .. } => {
                assert_eq!(text.as_deref(), Some("hi"));
                assert_eq!(tool_calls[0].id, "a");
                assert_eq!(tool_calls[1].id, "b");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn acc_drops_empty() {
        let acc = TurnAcc::default();
        match acc.into_msg() {
            Msg::Assistant { text, reasoning, tool_calls } => {
                assert!(text.is_none());
                assert!(reasoning.is_none());
                assert!(tool_calls.is_empty());
            }
            other => panic!("unexpected {other:?}"),
        }
        let _ = json!({});
    }
}
