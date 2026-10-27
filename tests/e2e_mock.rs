//! 端到端集成测试：本地 mock SSE 服务驱动真实 HTTP 请求 + 工具循环。

use rho::chat::{Msg, Session, TurnEnd};
use rho::config::{self, BashToolConfig};
use rho::provider;
use rho::term::Cancel;
use rho::tools;
use rho::render::RenderEvent;
use rho::repl;
use rho::ui::{ConfirmOutcome, ReadOutcome, ScriptedUi};

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut buf).expect("read request");
        if n == 0 {
            return Vec::new();
        }
        data.extend_from_slice(&buf[..n]);
        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&data[..header_end]).to_ascii_lowercase();
    let content_len = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while data.len() < header_end + content_len {
        let n = stream.read(&mut buf).expect("read body");
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    data[header_end..(header_end + content_len).min(data.len())].to_vec()
}

/// mock 服务器共享状态：请求体收集、已服务连接计数、停止标志。
#[derive(Default)]
struct ServerShared {
    bodies: Mutex<Vec<String>>,
    served: Mutex<usize>,
    ready: Condvar,
    stop: AtomicBool,
}

impl ServerShared {
    fn push_body(&self, body: Vec<u8>) {
        self.bodies
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(&body).into_owned());
    }

    fn finish_connection(&self) {
        let mut served = self.served.lock().unwrap();
        *served += 1;
        drop(served);
        self.ready.notify_all();
    }
}

/// 本地 mock SSE 服务器句柄：可等待已服务连接数、按需读取捕获的请求体；
/// Drop 时停止（accept 轮询线程随后退出，之后连接被拒绝），
/// 请求数不足时测试也能结束服务线程。
struct MockServer {
    base_url: String,
    shared: Arc<ServerShared>,
}

impl MockServer {
    fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 第 i 个请求体（clone）。
    fn body(&self, i: usize) -> String {
        self.shared.bodies.lock().unwrap()[i].clone()
    }

    fn request_count(&self) -> usize {
        self.shared.bodies.lock().unwrap().len()
    }

    /// 有界等待至少 n 个连接服务完成。
    fn wait_served(&self, n: usize, timeout: Duration) -> bool {
        let mut served = self.shared.served.lock().unwrap();
        let deadline = Instant::now() + timeout;
        while *served < n {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, _) = self
                .shared
                .ready
                .wait_timeout(served, deadline - now)
                .unwrap();
            served = guard;
        }
        true
    }

    /// 停止服务器：轮询线程在下个间隔退出，后续连接被拒绝。
    fn stop(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 等待一个新连接；停止标志置位或 listener 失效时返回 None。
/// listener 保持非阻塞，靠短间隔轮询让 stop 能及时生效。
fn accept_next(listener: &TcpListener, shared: &ServerShared) -> Option<TcpStream> {
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return None;
        }
        match listener.accept() {
            Ok((stream, _)) => return Some(stream),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
}

/// 读请求、捕获请求体、执行自定义响应、计数并广播。
fn serve_one(stream: &mut TcpStream, shared: &ServerShared, respond: impl FnOnce(&mut TcpStream)) {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let body = read_request(stream);
    shared.push_body(body);
    respond(stream);
    shared.finish_connection();
}

fn start_server(responses: Vec<String>) -> MockServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    // macOS 上 accept 出的 socket 会继承 O_NONBLOCK，显式恢复阻塞再读写。
    listener.set_nonblocking(true).unwrap();
    let shared: Arc<ServerShared> = Arc::new(Default::default());
    let thread_shared = shared.clone();
    std::thread::spawn(move || {
        for response in responses {
            let Some(mut stream) = accept_next(&listener, &thread_shared) else { return };
            stream.set_nonblocking(false).ok();
            serve_one(&mut stream, &thread_shared, |s| {
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                );
                let _ = s.write_all(response.as_bytes());
                let _ = s.flush();
                let _ = s.shutdown(Shutdown::Both);
            });
        }
    });
    MockServer { base_url, shared }
}

/// 单连接自定义响应（时序类测试用：慢响应头、分片中停顿等）。
fn start_custom(respond: impl Fn(&mut TcpStream) + Send + 'static) -> MockServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let shared: Arc<ServerShared> = Arc::new(Default::default());
    let thread_shared = shared.clone();
    std::thread::spawn(move || {
        let Some(mut stream) = accept_next(&listener, &thread_shared) else { return };
        stream.set_nonblocking(false).ok();
        serve_one(&mut stream, &thread_shared, respond);
    });
    MockServer { base_url, shared }
}

fn sse(value: &Value) -> String {
    format!("data: {value}\n\n")
}

fn tool_call_response() -> String {
    let mut out = String::new();
    out.push_str(&sse(&json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "function": {
                        "name": "bash",
                        "arguments": "{\"command\":\"echo hi\"}"
                    }
                }]
            }
        }]
    })));
    out.push_str(&sse(&json!({
        "choices": [{ "delta": {}, "finish_reason": "tool_calls" }]
    })));
    out.push_str("data: [DONE]\n\n");
    out
}

fn text_response(text: &str) -> String {
    let mut out = String::new();
    out.push_str(&sse(&json!({
        "choices": [{ "delta": { "content": text } }]
    })));
    out.push_str(&sse(&json!({
        "choices": [{ "delta": {}, "finish_reason": "stop" }]
    })));
    out.push_str("data: [DONE]\n\n");
    out
}

fn write_config(base_url: &str) -> String {
    write_config_extra(base_url, "")
}

fn write_config_extra(base_url: &str, extra: &str) -> String {
    write_config_full(base_url, extra, "")
}

/// 测试用临时文件名的进程内序号：`SystemTime` 分辨率不足以保证并行测试不撞名。
static TMP_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn tmp_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "rho-{tag}-{}-{}.toml",
        std::process::id(),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ))
}

fn write_config_full(base_url: &str, top_extra: &str, provider_extra: &str) -> String {
    let path = tmp_path("integration");
    let toml = format!(
        r#"
{top_extra}
[providers.mock]
protocol = "openai_chat"
base_url = "{base_url}"
{provider_extra}

[models."mock/m"]
context_window = 10000
max_output = 1000

[tools.bash]
allow = []
timeout_seconds = 5
max_output_bytes = 1024
"#
    );
    std::fs::write(&path, toml).unwrap();
    path.display().to_string()
}

#[test]
fn full_tool_loop_with_mock_sse() {
    let server = start_server(vec![tool_call_response(), text_response("done")]);
    let path = write_config(server.base_url());
    let cfg = config::load(&Some(path.clone())).unwrap();
    let _ = std::fs::remove_file(path);
    let selection = config::resolve_selection(&cfg, Some("mock"), None).unwrap();
    let adapter = provider::adapter_for(&selection.provider).unwrap();
    let mut session = Session::new(cfg, selection, adapter, false);
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(vec![], vec![ConfirmOutcome::No]);

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    assert!(matches!(end, TurnEnd::Done));
    assert!(server.wait_served(2, Duration::from_secs(5)));

    assert_eq!(session.history.len(), 4);
    assert!(matches!(&session.history[0], Msg::User(u) if u == "go"));
    match &session.history[1] {
        Msg::Assistant { tool_calls, .. } => {
            assert_eq!(tool_calls.len(), 1);
            assert_eq!(tool_calls[0].id, "call_1");
            assert_eq!(tool_calls[0].name, "bash");
        }
        other => panic!("unexpected {other:?}"),
    }
    match &session.history[2] {
        Msg::Tool { tool_call_id, content } => {
            assert_eq!(tool_call_id, "call_1");
            assert_eq!(content, "用户拒绝执行");
        }
        other => panic!("unexpected {other:?}"),
    }
    match &session.history[3] {
        Msg::Assistant { text, .. } => assert_eq!(text.as_deref(), Some("done")),
        other => panic!("unexpected {other:?}"),
    }

    let rendered = &ui.rendered;
    assert!(rendered
        .iter()
        .any(|e| matches!(e, RenderEvent::ToolCall(c) if c == "echo hi")));
    assert!(rendered
        .iter()
        .any(|e| matches!(e, RenderEvent::ToolResult(c) if c == "用户拒绝执行")));
    assert!(rendered
        .iter()
        .any(|e| matches!(e, RenderEvent::TextDelta(t) if t == "done")));

    let bodies = server.request_count();
    assert_eq!(bodies, 2);
    let second: Value = serde_json::from_str(&server.body(1)).unwrap();
    let messages = second["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["tool_calls"][0]["id"], "call_1");
    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(messages[2]["tool_call_id"], "call_1");
    assert!(second["tools"].is_array());
    assert_eq!(second["stream"], true);
}

/// 上游“思考”一段时间才回响应头是常态，不能因读超时被当成失败。
/// 回归护栏：ureq 的读超时同时作用于响应头与响应体，却只由 `timeout_read` 一个旋钮控制，
/// 把它调到秒级以下会让正常请求报 “timed out reading response”。
#[test]
fn slow_response_headers_are_not_an_error() {
    let server = start_custom(|stream| {
        // 先“思考” 1.5s 再发响应头。
        std::thread::sleep(Duration::from_millis(1500));
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        );
        let _ = stream.write_all(
            sse(&json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}))
                .as_bytes(),
        );
        let _ = stream.write_all(b"data: [DONE]\n\n");
        let _ = stream.flush();
        let _ = stream.shutdown(Shutdown::Both);
    });

    let cfg_path = write_config(server.base_url());
    let cfg = config::load(&Some(cfg_path.clone())).unwrap();
    let _ = std::fs::remove_file(&cfg_path);
    let selection = config::resolve_selection(&cfg, Some("mock"), None).unwrap();
    let adapter = provider::adapter_for(&selection.provider).unwrap();
    let mut session = Session::new(cfg, selection, adapter, false);
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(vec![], vec![]);

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    assert!(matches!(end, TurnEnd::Done), "end was {end:?}");
}

/// 流中间停顿跨越预期的读粒度时，流必须继续而不是被判失败。
#[test]
fn slow_stream_between_chunks_is_not_an_error() {
    let server = start_custom(|stream| {
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        );
        let _ = stream
            .write_all(sse(&json!({"choices":[{"delta":{"content":"a"}}]})).as_bytes());
        let _ = stream.flush();
        // 超过一秒的空白间隔：worker 应继续读而不是报错。
        std::thread::sleep(Duration::from_millis(1200));
        let _ = stream.write_all(
            sse(&json!({"choices":[{"delta":{"content":"b"},"finish_reason":"stop"}]}))
                .as_bytes(),
        );
        let _ = stream.write_all(b"data: [DONE]\n\n");
        let _ = stream.flush();
        let _ = stream.shutdown(Shutdown::Both);
    });

    let cfg_path = write_config(server.base_url());
    let cfg = config::load(&Some(cfg_path.clone())).unwrap();
    let _ = std::fs::remove_file(&cfg_path);
    let selection = config::resolve_selection(&cfg, Some("mock"), None).unwrap();
    let adapter = provider::adapter_for(&selection.provider).unwrap();
    let mut session = Session::new(cfg, selection, adapter, false);
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(vec![], vec![]);

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    assert!(matches!(end, TurnEnd::Done), "end was {end:?}");
    match session.history.last() {
        Some(Msg::Assistant { text, .. }) => assert_eq!(text.as_deref(), Some("ab")),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn config_errors_are_collected() {
    let path = tmp_path("bad");
    let toml = r#"
default_provider = "missing"
default_model = "noprefix"
[providers.x]
protocol = "openai_chat"
base_url = "http://localhost"
[models.bad]
context_window = 1
max_output = 1
thinking_default = "high"
[models.bad.thinking]
low = { thinking = { type = "enabled" } }
"#;
    std::fs::write(&path, toml).unwrap();
    let err = config::load(&Some(path.display().to_string())).unwrap_err();
    let _ = std::fs::remove_file(path);
    assert!(err.contains("default_provider"));
    assert!(err.contains("default_model"));
    assert!(err.contains("thinking_default"));
    assert!(err.contains("provider 前缀"));
}

#[test]
fn bash_captures_stdout_stderr_and_exit_code() {
    let cfg = BashToolConfig {
        allow: vec![],
        timeout_seconds: 5,
        max_output_bytes: 1024,
    };
    let out = tools::run_bash("printf hi; printf err >&2; exit 3", &cfg, &Cancel::new());
    assert!(matches!(out.end, tools::BashEnd::Exited(3)));
    assert!(out.content.contains("hi"));
    assert!(out.content.contains("[stderr]"));
    assert!(out.content.contains("err"));
    assert!(out.content.contains("exit code 3"));
}

#[test]
fn bash_timeout_terminates_group() {
    let cfg = BashToolConfig {
        allow: vec![],
        timeout_seconds: 1,
        max_output_bytes: 1024,
    };
    let started = std::time::Instant::now();
    let out = tools::run_bash("sleep 30", &cfg, &Cancel::new());
    assert!(matches!(out.end, tools::BashEnd::TimedOut));
    assert!(out.content.contains("timed out"));
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn ctrl_c_during_stream_cancels_and_discards_partial() {
    let first = sse(&json!({
        "choices": [{ "delta": { "content": "partial" } }]
    }));
    let rest = format!(
        "{}{}data: [DONE]\n\n",
        sse(&json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] })),
        sse(&json!({ "choices": [{ "delta": { "content": "late" } }] }))
    );
    // 服务端在首块后停顿 1s 再发剩余，保证取消注入落在流式期间。
    let server = start_custom(move |stream| {
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        );
        let _ = stream.write_all(first.as_bytes());
        let _ = stream.flush();
        std::thread::sleep(Duration::from_secs(1));
        let _ = stream.write_all(rest.as_bytes());
        let _ = stream.flush();
        let _ = stream.shutdown(Shutdown::Both);
    });
    let path = write_config(server.base_url());
    let cfg = config::load(&Some(path.clone())).unwrap();
    let _ = std::fs::remove_file(path);
    let selection = config::resolve_selection(&cfg, Some("mock"), None).unwrap();
    let adapter = provider::adapter_for(&selection.provider).unwrap();
    let mut session = Session::new(cfg, selection, adapter, false);
    let cancel = Cancel::new();
    let injector = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        injector.inject();
    });
    let mut ui = ScriptedUi::new(vec![], vec![]);
    let started = std::time::Instant::now();
    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    assert!(matches!(end, TurnEnd::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(2));
    // 未完成的 assistant 响应不进入历史，只保留用户输入。
    assert_eq!(session.history.len(), 1);
    assert!(matches!(&session.history[0], Msg::User(u) if u == "go"));
}

/// mock 服务器生命周期：stop 后不再接受连接（测试提前失败时也能结束服务线程）。
#[test]
fn mock_server_stop_closes_listener() {
    let server = start_server(vec![text_response("x")]);
    server.stop();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match TcpStream::connect(server.base_url()) {
            Err(_) => break,
            Ok(_) if Instant::now() >= deadline => {
                panic!("server still accepting connections after stop")
            }
            Ok(_) => {}
        }
    }
}


fn two_tool_calls_response() -> String {
    let mut out = String::new();
    out.push_str(&sse(&json!({
        "choices": [{
            "delta": {
                "tool_calls": [
                    {
                        "index": 0,
                        "id": "call_a",
                        "function": { "name": "bash", "arguments": "{\"command\":\"echo a\"}" }
                    },
                    {
                        "index": 1,
                        "id": "call_b",
                        "function": { "name": "bash", "arguments": "{\"command\":\"echo b\"}" }
                    }
                ]
            }
        }]
    })));
    out.push_str(&sse(&json!({
        "choices": [{ "delta": {}, "finish_reason": "tool_calls" }]
    })));
    out.push_str("data: [DONE]\n\n");
    out
}

#[test]
fn multiple_tool_calls_are_paired_in_order() {
    let server = start_server(vec![two_tool_calls_response(), text_response("ok")]);
    let path = write_config(server.base_url());
    let cfg = config::load(&Some(path.clone())).unwrap();
    let _ = std::fs::remove_file(path);
    let selection = config::resolve_selection(&cfg, Some("mock"), None).unwrap();
    let adapter = provider::adapter_for(&selection.provider).unwrap();
    let mut session = Session::new(cfg, selection, adapter, false);
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(
        vec![],
        vec![ConfirmOutcome::No, ConfirmOutcome::No],
    );

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    assert!(matches!(end, TurnEnd::Done));
    assert_eq!(session.history.len(), 5);
    match (&session.history[2], &session.history[3]) {
        (
            Msg::Tool { tool_call_id: a, content: ca },
            Msg::Tool { tool_call_id: b, content: cb },
        ) => {
            assert_eq!(a, "call_a");
            assert_eq!(b, "call_b");
            assert_eq!(ca, "用户拒绝执行");
            assert_eq!(cb, "用户拒绝执行");
        }
        other => panic!("unexpected {other:?}"),
    }
    let second: Value = serde_json::from_str(&server.body(1)).unwrap();
    let messages = second["messages"].as_array().unwrap();
    assert_eq!(messages[2]["tool_call_id"], "call_a");
    assert_eq!(messages[3]["tool_call_id"], "call_b");
}

#[test]
fn max_tool_steps_placeholders_remaining_calls() {
    let server = start_server(vec![two_tool_calls_response()]);
    let path = write_config_extra(server.base_url(), "max_tool_steps = 1");
    let cfg = config::load(&Some(path.clone())).unwrap();
    let _ = std::fs::remove_file(path);
    let selection = config::resolve_selection(&cfg, Some("mock"), None).unwrap();
    let adapter = provider::adapter_for(&selection.provider).unwrap();
    let mut session = Session::new(cfg, selection, adapter, false);
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(vec![], vec![ConfirmOutcome::No]);

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    assert!(matches!(end, TurnEnd::Error(_)));
    assert_eq!(session.history.len(), 4);
    match (&session.history[2], &session.history[3]) {
        (
            Msg::Tool { tool_call_id: a, content: ca },
            Msg::Tool { tool_call_id: b, content: cb },
        ) => {
            assert_eq!(a, "call_a");
            assert_eq!(b, "call_b");
            assert_eq!(ca, "用户拒绝执行");
            assert!(cb.contains("上限"));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(server.request_count(), 1);
}


fn tool_call_response_with_reasoning() -> String {
    let mut out = String::new();
    out.push_str(&sse(&json!({
        "choices": [{
            "delta": {
                "reasoning_content": "think",
                "tool_calls": [{
                    "index": 0,
                    "id": "call_r",
                    "function": { "name": "bash", "arguments": "{\"command\":\"echo hi\"}" }
                }]
            }
        }]
    })));
    out.push_str(&sse(&json!({
        "choices": [{ "delta": {}, "finish_reason": "tool_calls" }]
    })));
    out.push_str("data: [DONE]\n\n");
    out
}

#[test]
fn echo_reasoning_is_serialized_on_next_request() {
    let server = start_server(vec![
        tool_call_response_with_reasoning(),
        text_response("ok"),
    ]);
    let path = write_config_full(server.base_url(), "", "[providers.mock.compat]\necho_reasoning = true");
    let cfg = config::load(&Some(path.clone())).unwrap();
    let _ = std::fs::remove_file(path);
    let selection = config::resolve_selection(&cfg, Some("mock"), None).unwrap();
    let adapter = provider::adapter_for(&selection.provider).unwrap();
    let mut session = Session::new(cfg, selection, adapter, false);
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(vec![], vec![ConfirmOutcome::No]);

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    assert!(matches!(end, TurnEnd::Done));
    let second: Value = serde_json::from_str(&server.body(1)).unwrap();
    assert_eq!(second["messages"][1]["reasoning_content"], "think");
}

#[test]
fn top_level_error_chunk_is_surfaced() {
    let response = format!(
        "{}{}",
        sse(&json!({ "error": { "message": "boom" } })),
        "data: [DONE]\n\n"
    );
    let server = start_server(vec![response]);
    let path = write_config(server.base_url());
    let cfg = config::load(&Some(path.clone())).unwrap();
    let _ = std::fs::remove_file(path);
    let selection = config::resolve_selection(&cfg, Some("mock"), None).unwrap();
    let adapter = provider::adapter_for(&selection.provider).unwrap();
    let mut session = Session::new(cfg, selection, adapter, false);
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(vec![], vec![]);

    match session.turn(&mut ui, &cancel, "go").unwrap() {
        TurnEnd::Error(e) => assert!(e.contains("boom"), "error was {e}"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn truncated_finish_reason_is_error() {
    let mut response = String::new();
    response.push_str(&sse(&json!({ "choices": [{ "delta": { "content": "part" } }] })));
    response.push_str(&sse(&json!({
        "choices": [{ "delta": {}, "finish_reason": "length" }]
    })));
    response.push_str("data: [DONE]\n\n");
    let server = start_server(vec![response]);
    let path = write_config(server.base_url());
    let cfg = config::load(&Some(path.clone())).unwrap();
    let _ = std::fs::remove_file(path);
    let selection = config::resolve_selection(&cfg, Some("mock"), None).unwrap();
    let adapter = provider::adapter_for(&selection.provider).unwrap();
    let mut session = Session::new(cfg, selection, adapter, false);
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(vec![], vec![]);

    match session.turn(&mut ui, &cancel, "go").unwrap() {
        TurnEnd::Error(e) => assert!(e.contains("length"), "error was {e}"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn done_without_trailing_newline_is_accepted() {
    let response = format!(
        "{}{}data: [DONE]",
        sse(&json!({ "choices": [{ "delta": { "content": "ok" } }] })),
        sse(&json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] }))
    );
    let server = start_server(vec![response]);
    let path = write_config(server.base_url());
    let cfg = config::load(&Some(path.clone())).unwrap();
    let _ = std::fs::remove_file(path);
    let selection = config::resolve_selection(&cfg, Some("mock"), None).unwrap();
    let adapter = provider::adapter_for(&selection.provider).unwrap();
    let mut session = Session::new(cfg, selection, adapter, false);
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(vec![], vec![]);

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    assert!(matches!(end, TurnEnd::Done));
}

fn unknown_tool_response() -> String {
    let mut out = String::new();
    out.push_str(&sse(&json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_x",
                    "function": { "name": "nope", "arguments": "{}" }
                }]
            }
        }]
    })));
    out.push_str(&sse(&json!({
        "choices": [{ "delta": {}, "finish_reason": "tool_calls" }]
    })));
    out.push_str("data: [DONE]\n\n");
    out
}

#[test]
fn unknown_tool_result_is_rendered() {
    let server = start_server(vec![unknown_tool_response(), text_response("ok")]);
    let path = write_config(server.base_url());
    let cfg = config::load(&Some(path.clone())).unwrap();
    let _ = std::fs::remove_file(path);
    let selection = config::resolve_selection(&cfg, Some("mock"), None).unwrap();
    let adapter = provider::adapter_for(&selection.provider).unwrap();
    let mut session = Session::new(cfg, selection, adapter, false);
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(vec![], vec![]);

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    assert!(matches!(end, TurnEnd::Done));
    assert!(ui
        .rendered
        .iter()
        .any(|e| matches!(e, RenderEvent::ToolResult(c) if c.contains("未知工具：nope"))));
}

#[test]
fn bash_large_output_is_truncated_once_with_accurate_count() {
    let cfg = BashToolConfig {
        allow: vec![],
        timeout_seconds: 10,
        max_output_bytes: 200,
    };
    let out = tools::run_bash("yes x | head -c 100000", &cfg, &Cancel::new());
    assert!(matches!(out.end, tools::BashEnd::Exited(0)));
    assert!(out.content.contains("exit code 0"));
    assert_eq!(out.content.matches("省略").count(), 1);
    let omitted: usize = out
        .content
        .split("省略 ")
        .nth(1)
        .unwrap()
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(omitted > 99_000, "omitted was {omitted}");
}

#[test]
fn repl_adds_line_break_after_done_response() {
    let server = start_server(vec![text_response("one line")]);
    let path = write_config(server.base_url());
    let cfg = config::load(&Some(path.clone())).unwrap();
    let _ = std::fs::remove_file(path);
    let selection = config::resolve_selection(&cfg, Some("mock"), None).unwrap();
    let adapter = provider::adapter_for(&selection.provider).unwrap();
    let session = Session::new(cfg, selection, adapter, false);
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(
        vec![ReadOutcome::Line("go".into()), ReadOutcome::Line("/exit".into())],
        vec![],
    );

    repl::run(&mut ui, session, &cancel).unwrap();

    let text = ui
        .rendered
        .iter()
        .position(|e| matches!(e, RenderEvent::TextDelta(s) if s == "one line"))
        .unwrap();
    let line_break = ui
        .rendered
        .iter()
        .position(|e| matches!(e, RenderEvent::LineBreak))
        .unwrap();
    assert!(text < line_break);
    assert!(matches!(ui.rendered.last(), Some(RenderEvent::LineBreak)));
}

#[test]
fn line_break_is_rendered_between_reasoning_and_text() {
    let mut response = String::new();
    response.push_str(&sse(&json!({
        "choices": [{ "delta": { "reasoning_content": "think" } }]
    })));
    response.push_str(&sse(&json!({
        "choices": [{ "delta": { "content": "answer" } }]
    })));
    response.push_str(&sse(&json!({
        "choices": [{ "delta": {}, "finish_reason": "stop" }]
    })));
    response.push_str("data: [DONE]\n\n");

    let server = start_server(vec![response]);
    let path = write_config(server.base_url());
    let cfg = config::load(&Some(path.clone())).unwrap();
    let _ = std::fs::remove_file(path);
    let selection = config::resolve_selection(&cfg, Some("mock"), None).unwrap();
    let adapter = provider::adapter_for(&selection.provider).unwrap();
    let mut session = Session::new(cfg, selection, adapter, false);
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(vec![], vec![]);

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    assert!(matches!(end, TurnEnd::Done));

    let reasoning = ui
        .rendered
        .iter()
        .position(|e| matches!(e, RenderEvent::ReasoningDelta(s) if s == "think"))
        .unwrap();
    let line_break = ui
        .rendered
        .iter()
        .position(|e| matches!(e, RenderEvent::LineBreak))
        .unwrap();
    let text = ui
        .rendered
        .iter()
        .position(|e| matches!(e, RenderEvent::TextDelta(s) if s == "answer"))
        .unwrap();
    assert!(reasoning < line_break && line_break < text);
}
