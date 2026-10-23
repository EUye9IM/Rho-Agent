//! 供应商适配层：trait、按 Provider 构造（注入 compat）并分发、流式请求工作线程。

mod openai;

pub use openai::OpenAiChatAdapter;

use std::io::{BufReader, ErrorKind, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::Duration;

use crate::chat::{Msg, ToolCall};
use crate::config::{Model, Provider};
use crate::sse::SseParser;
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

fn send(tx: &std::sync::mpsc::Sender<StreamEvent>, ev: StreamEvent) -> bool {
    tx.send(ev).is_ok()
}

fn send_error(tx: &std::sync::mpsc::Sender<StreamEvent>, msg: impl Into<String>) -> bool {
    send(tx, StreamEvent::Error(msg.into()))
}

fn shorten(s: &str) -> String {
    const MAX: usize = 4000;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(MAX).collect();
        out.push_str("…（截断）");
        out
    }
}

/// 自建 TLS 配置：默认编入 webpki-roots；设置 SSL_CERT_FILE 时**覆盖**默认根。
fn tls_config() -> Result<rustls::ClientConfig, String> {
    let mut roots = rustls::RootCertStore::empty();
    match std::env::var("SSL_CERT_FILE") {
        Ok(path) => {
            let file = std::fs::File::open(&path)
                .map_err(|e| format!("无法读取 SSL_CERT_FILE={path}：{e}"))?;
            let mut reader = BufReader::new(file);
            let certs = rustls_pemfile::certs(&mut reader)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("解析 SSL_CERT_FILE={path} 失败：{e}"))?;
            if certs.is_empty() {
                return Err(format!("SSL_CERT_FILE={path} 中没有可用证书"));
            }
            for cert in certs {
                roots
                    .add(cert)
                    .map_err(|e| format!("加载 SSL_CERT_FILE={path} 的证书失败：{e}"))?;
            }
        }
        Err(_) => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }

    Ok(rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}

fn host_of_url(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit('@').next()?;
    // 去掉端口（IPv6 字面量不在支持范围内）。
    host_port.split(':').next()
}

fn no_proxy_match(host: &str) -> bool {
    let value = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .ok();
    let Some(value) = value else { return false };
    let host = host.to_ascii_lowercase();
    value.split(',').any(|raw| {
        let entry = raw.trim().trim_start_matches('.').to_ascii_lowercase();
        if entry.is_empty() {
            false
        } else if entry == "*" {
            true
        } else {
            host == entry || host.ends_with(&format!(".{entry}"))
        }
    })
}

fn proxy_for(url: &str) -> Result<Option<ureq::Proxy>, String> {
    let Some(host) = host_of_url(url) else { return Ok(None) };
    if no_proxy_match(host) {
        return Ok(None);
    }
    let names: &[&str] = if url.starts_with("https://") {
        &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
    } else {
        &["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
    };
    let Some(value) = names
        .iter()
        .find_map(|n| std::env::var(n).ok().filter(|v| !v.trim().is_empty()))
    else {
        return Ok(None);
    };
    ureq::Proxy::new(value.trim())
        .map(Some)
        .map_err(|e| format!("代理配置无效（{}={}）：{e}", names[0], value.trim()))
}

fn build_agent(base_url: &str) -> Result<ureq::Agent, String> {
    let tls = tls_config()?;
    let mut builder = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        // ureq 只有 per-read 超时；首字节 60s 由 stream_request 的看门狗实现，
        // 这里保留流式空闲 120s（持续有数据时不受总存活时间限制）。
        .timeout_read(Duration::from_secs(120))
        .tls_config(Arc::new(tls));
    if let Some(proxy) = proxy_for(base_url)? {
        builder = builder.proxy(proxy);
    }
    Ok(builder.build())
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
    for ev in adapter.parse_chunk(&chunk, acc) {
        if !send(tx, ev) {
            return PayloadOutcome::Fatal;
        }
    }
    PayloadOutcome::Continue
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
    let url = adapter.endpoint(&provider);
    let agent = match build_agent(&provider.base_url) {
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
    // 流式空闲 120s 仍由 ureq 的 timeout_read 负责。
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
            let body = response.into_string().unwrap_or_default();
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

    'stream: loop {
        if aborted.load(Ordering::SeqCst) {
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
            Err(e) => {
                if aborted.load(Ordering::SeqCst) {
                    return;
                }
                send_error(&tx, format!("读取响应流失败：{e}"));
                return;
            }
        };
        if aborted.load(Ordering::SeqCst) {
            return;
        }
        for payload in parser.feed(&buf[..n]) {
            match handle_payload(&adapter, &mut acc, &tx, &payload) {
                PayloadOutcome::Continue => {}
                PayloadOutcome::Done => {
                    saw_done = true;
                    break 'stream;
                }
                PayloadOutcome::Fatal => return,
            }
        }
    }

    if aborted.load(Ordering::SeqCst) {
        return;
    }
    if !saw_done {
        // EOF 前没有换行结尾的最后一条 data: 也要处理。
        for payload in parser.finish() {
            match handle_payload(&adapter, &mut acc, &tx, &payload) {
                PayloadOutcome::Continue => {}
                PayloadOutcome::Done => {
                    saw_done = true;
                    break;
                }
                PayloadOutcome::Fatal => return,
            }
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
    if !send(
        &tx,
        StreamEvent::Done { finish_reason: acc.finish_reason.clone() },
    ) {
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
