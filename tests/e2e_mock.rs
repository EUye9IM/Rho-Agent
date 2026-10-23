//! 端到端集成测试：本地 mock SSE 服务驱动真实 HTTP 请求 + 工具循环。

use rho::chat::{Msg, Session, TurnEnd};
use rho::config::{self, BashToolConfig};
use rho::provider;
use rho::term::Cancel;
use rho::tools;
use rho::render::RenderEvent;
use rho::ui::{ConfirmOutcome, ScriptedUi};

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

fn start_server(responses: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let captured = bodies.clone();
    std::thread::spawn(move || {
        for response in responses {
            let Ok((mut stream, _)) = listener.accept() else { return };
            stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
            let body = read_request(&mut stream);
            captured
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&body).into_owned());
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Both);
        }
    });
    (format!("http://{addr}"), bodies)
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

fn write_config_full(base_url: &str, top_extra: &str, provider_extra: &str) -> String {
    let path = std::env::temp_dir().join(format!(
        "rho-integration-{}-{}.toml",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
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
    let (base_url, bodies) = start_server(vec![tool_call_response(), text_response("done")]);
    let path = write_config(&base_url);
    let cfg = config::load(&Some(path.clone())).unwrap();
    let _ = std::fs::remove_file(path);
    let selection = config::resolve_selection(&cfg, Some("mock"), None).unwrap();
    let adapter = provider::adapter_for(&selection.provider).unwrap();
    let mut session = Session::new(cfg, selection, adapter, false);
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(vec![], vec![ConfirmOutcome::No]);

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    assert!(matches!(end, TurnEnd::Done));

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

    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    let second: Value = serde_json::from_str(&bodies[1]).unwrap();
    let messages = second["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["tool_calls"][0]["id"], "call_1");
    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(messages[2]["tool_call_id"], "call_1");
    assert!(second["tools"].is_array());
    assert_eq!(second["stream"], true);
}

#[test]
fn config_errors_are_collected() {
    let path = std::env::temp_dir().join(format!("rho-bad-{}.toml", std::process::id()));
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

fn start_partial_server(first: String, rest: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else { return };
        stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let _ = read_request(&mut stream);
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
    format!("http://{addr}")
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
    let base_url = start_partial_server(first, rest);
    let path = write_config(&base_url);
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
    let (base_url, bodies) = start_server(vec![two_tool_calls_response(), text_response("ok")]);
    let path = write_config(&base_url);
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
    let second: Value = serde_json::from_str(&bodies.lock().unwrap()[1]).unwrap();
    let messages = second["messages"].as_array().unwrap();
    assert_eq!(messages[2]["tool_call_id"], "call_a");
    assert_eq!(messages[3]["tool_call_id"], "call_b");
}

#[test]
fn max_tool_steps_placeholders_remaining_calls() {
    let (base_url, bodies) = start_server(vec![two_tool_calls_response()]);
    let path = write_config_extra(&base_url, "max_tool_steps = 1");
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
    assert_eq!(bodies.lock().unwrap().len(), 1);
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
    let (base_url, bodies) = start_server(vec![
        tool_call_response_with_reasoning(),
        text_response("ok"),
    ]);
    let path = write_config_full(&base_url, "", "[providers.mock.compat]\necho_reasoning = true");
    let cfg = config::load(&Some(path.clone())).unwrap();
    let _ = std::fs::remove_file(path);
    let selection = config::resolve_selection(&cfg, Some("mock"), None).unwrap();
    let adapter = provider::adapter_for(&selection.provider).unwrap();
    let mut session = Session::new(cfg, selection, adapter, false);
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(vec![], vec![ConfirmOutcome::No]);

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    assert!(matches!(end, TurnEnd::Done));
    let second: Value = serde_json::from_str(&bodies.lock().unwrap()[1]).unwrap();
    assert_eq!(second["messages"][1]["reasoning_content"], "think");
}

#[test]
fn top_level_error_chunk_is_surfaced() {
    let response = format!(
        "{}{}",
        sse(&json!({ "error": { "message": "boom" } })),
        "data: [DONE]\n\n"
    );
    let (base_url, _bodies) = start_server(vec![response]);
    let path = write_config(&base_url);
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
    let (base_url, _bodies) = start_server(vec![response]);
    let path = write_config(&base_url);
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
    let (base_url, _bodies) = start_server(vec![response]);
    let path = write_config(&base_url);
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
    let (base_url, _bodies) = start_server(vec![unknown_tool_response(), text_response("ok")]);
    let path = write_config(&base_url);
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

    let (base_url, _bodies) = start_server(vec![response]);
    let path = write_config(&base_url);
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
