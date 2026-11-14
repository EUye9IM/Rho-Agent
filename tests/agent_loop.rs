//! agent loop 分支测试：`ScriptedLauncher` 注入预编排 `StreamEvent` +
//! `ToolRegistry` 预设结果驱动真实 `Session::turn`，无网络无进程。
//! 覆盖：取消/退出各粒度、占位补齐、错误路径与配对不变式。

use rho::chat::{AssistantMsg, Msg, Session, StreamLauncher, TokenUsage, ToolCall, TurnEnd};
use rho::completion::CompletionProvider;
use rho::config::{self, Provider};
use rho::provider::{self, ProviderAdapter, StreamEvent};
use rho::term::Cancel;
use rho::tools::{Tool, ToolContext, ToolRegistry, ToolResult, ToolSpec, ToolStatus};
use rho::ui::{ReadOutcome, ScriptedUi, Ui};

use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CONFIG_TOML: &str = r#"
[providers.mock]
protocol = "openai_chat"
base_url = "http://127.0.0.1:1"

[models."mock/m"]
context_window = 1000
max_output = 100
"#;

/// launcher 不发真实请求，base_url 仅需通过校验。
fn session() -> Session {
    let cfg = config::load_str(CONFIG_TOML, "<测试配置>").unwrap();
    let selection = config::resolve_selection(&cfg, None, None).unwrap();
    let adapter = provider::adapter_for(&selection.provider).unwrap();
    Session::new(cfg, selection, adapter)
}

fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.into(),
    }
}

fn assistant_with_calls(calls: Vec<ToolCall>) -> AssistantMsg {
    AssistantMsg {
        text: None,
        reasoning: None,
        tool_calls: calls,
    }
}

fn text_final(t: &str) -> StreamEvent {
    StreamEvent::Final(Box::new(AssistantMsg {
        text: Some(t.into()),
        reasoning: None,
        tool_calls: vec![],
    }))
}

fn tool_result_content(history: &[Msg], i: usize) -> &str {
    match &history[i] {
        Msg::Tool { content, .. } => content,
        other => panic!("unexpected {other:?}"),
    }
}

/// 每次按序弹出一组事件；耗尽即说明出现了测试预期外的额外请求。
struct ScriptedLauncher {
    scripts: Mutex<Vec<Vec<StreamEvent>>>,
}

impl ScriptedLauncher {
    fn new(scripts: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            scripts: Mutex::new(scripts),
        }
    }
}

impl StreamLauncher for ScriptedLauncher {
    fn launch(
        &self,
        _adapter: Arc<dyn ProviderAdapter>,
        _provider: Provider,
        _body: Value,
        _headers: Vec<(String, String)>,
        _cancelled: Arc<AtomicBool>,
    ) -> Receiver<StreamEvent> {
        // 在主线程取脚本：耗尽时直接 panic 在测试线程，失败可见。
        let script = self.scripts.lock().unwrap().remove(0);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for ev in script {
                if tx.send(ev).is_err() {
                    return;
                }
            }
        });
        rx
    }
}

/// 记录请求体并按脚本返回事件的启动器：压缩测试要检查发给模型的 body 形状。
#[derive(Clone)]
struct RecordingLauncher {
    bodies: Arc<Mutex<Vec<Value>>>,
    scripts: Arc<Mutex<Vec<Vec<StreamEvent>>>>,
}

impl RecordingLauncher {
    fn new(scripts: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            bodies: Arc::default(),
            scripts: Arc::new(Mutex::new(scripts)),
        }
    }

    fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().unwrap().clone()
    }
}

impl StreamLauncher for RecordingLauncher {
    fn launch(
        &self,
        _adapter: Arc<dyn ProviderAdapter>,
        _provider: Provider,
        body: Value,
        _headers: Vec<(String, String)>,
        _cancelled: Arc<AtomicBool>,
    ) -> Receiver<StreamEvent> {
        self.bodies.lock().unwrap().push(body);
        let script = self.scripts.lock().unwrap().remove(0);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for ev in script {
                if tx.send(ev).is_err() {
                    return;
                }
            }
        });
        rx
    }
}

/// 时序受控的启动器：等测试注入信号后再发一个 Delta，随后阻塞到取消标志置位。
/// 用于流式期间取消/退出的确定性触发（不发终态，tx 在主线程放弃接收前不 drop）。
struct ParkingLauncher {
    /// launch 后立即通知测试“已就绪”。
    ready: mpsc::Sender<()>,
    /// 测试注入完成后放行（单次使用）。
    released: Mutex<Option<mpsc::Receiver<()>>>,
}

impl StreamLauncher for ParkingLauncher {
    fn launch(
        &self,
        _adapter: Arc<dyn ProviderAdapter>,
        _provider: Provider,
        _body: Value,
        _headers: Vec<(String, String)>,
        cancelled: Arc<AtomicBool>,
    ) -> Receiver<StreamEvent> {
        let (tx, rx) = mpsc::channel();
        let ready = self.ready.clone();
        let released = self.released.lock().unwrap().take().expect("单次使用");
        std::thread::spawn(move || {
            let _ = ready.send(());
            let _ = released.recv();
            let _ = tx.send(StreamEvent::Delta {
                text: Some("partial".into()),
                reasoning: None,
            });
            while !cancelled.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        rx
    }
}

fn test_spec() -> ToolSpec {
    ToolSpec {
        name: "bash".into(),
        description: "测试工具".into(),
        parameters: json!({"type": "object"}),
    }
}

fn registry_with(tool: impl Tool + 'static) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(tool);
    registry
}

/// 预设结果工具：固定输出，不启动任何进程。
struct PresetTool {
    output: String,
}

impl Tool for PresetTool {
    fn spec(&self) -> ToolSpec {
        test_spec()
    }

    fn invoke(&self, _args: Value, _ctx: &ToolContext<'_>) -> ToolResult {
        ToolResult {
            output: self.output.clone(),
            display: None,
            status: ToolStatus::Success,
        }
    }
}

/// 预设结果工具：invoke 期间注入指定次数的 Ctrl-C（模拟执行中被取消/升级 quit）。
struct InjectingTool {
    injects: usize,
}

impl Tool for InjectingTool {
    fn spec(&self) -> ToolSpec {
        test_spec()
    }

    fn invoke(&self, _args: Value, ctx: &ToolContext<'_>) -> ToolResult {
        for _ in 0..self.injects {
            ctx.cancel.inject();
        }
        ToolResult {
            output: "partial\n[Command cancelled: 已部分执行（被取消，副作用已发生）]\n".into(),
            display: None,
            status: ToolStatus::Interrupted,
        }
    }
}

/// 注入器线程：等 launcher 就绪 → 注入 N 次 → 放行。
fn injector_after_ready(
    cancel: Arc<Cancel>,
    ready: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
    injects: usize,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        ready.recv().expect("launcher 就绪");
        for _ in 0..injects {
            cancel.inject();
        }
        release.send(()).expect("放行 launcher");
    })
}

const CMD: &str = r#"{"command":"echo hi"}"#;

/// 前 N 次 print 正常，之后每次 print 都置位 fatal（模拟渲染 I/O 失败，如 EPIPE）。
struct FailingUi {
    prints_left: usize,
    fatal: Option<String>,
}

impl FailingUi {
    fn new(prints_left: usize) -> Self {
        Self {
            prints_left,
            fatal: None,
        }
    }
}

impl Ui for FailingUi {
    fn read_line(&mut self, _completer: Arc<dyn CompletionProvider>) -> ReadOutcome {
        ReadOutcome::Eof
    }

    fn print(&mut self, _ev: &rho::render::RenderEvent) {
        if self.prints_left == 0 {
            self.fatal = Some("渲染输出失败：模拟 EPIPE".into());
        } else {
            self.prints_left -= 1;
        }
    }

    fn take_fatal(&mut self) -> Option<String> {
        self.fatal.take()
    }
}

#[test]
fn first_call_failures_do_not_block_rest() {
    let mut session = session();
    session.launcher = Arc::new(ScriptedLauncher::new(vec![
        vec![StreamEvent::Final(Box::new(assistant_with_calls(vec![
            call("c1", "nope", "{}"),
            call("c2", "bash", "{not json"),
            call("c3", "bash", CMD),
        ])))],
        vec![text_final("done")],
    ]));
    // 未知工具/参数错误不影响后续调用；c3 正常执行。
    session.tools = registry_with(PresetTool {
        output: "hi".into(),
    });
    let mut ui = ScriptedUi::new(vec![]);
    let cancel = Cancel::new();

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    assert!(matches!(end, TurnEnd::Done), "end was {end:?}");
    // User + Assistant(3 calls) + 3×Tool + Assistant(done)
    assert_eq!(session.history().len(), 6);
    assert!(tool_result_content(session.history(), 2).contains("未知工具：nope"));
    assert!(tool_result_content(session.history(), 3).contains("参数解析失败"));
    assert!(tool_result_content(session.history(), 4).contains("hi"));
    match &session.history()[5] {
        Msg::Assistant(a) => {
            assert_eq!(a.text.as_deref(), Some("done"));
            assert!(a.tool_calls.is_empty());
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn execution_cancel_keeps_partial_result() {
    let mut session = session();
    session.launcher = Arc::new(ScriptedLauncher::new(vec![vec![StreamEvent::Final(
        Box::new(assistant_with_calls(vec![
            call("c1", "bash", CMD),
            call("c2", "bash", CMD),
        ])),
    )]]));
    session.tools = registry_with(InjectingTool { injects: 1 });
    let mut ui = ScriptedUi::new(vec![]);
    let cancel = Cancel::new();

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    assert!(matches!(end, TurnEnd::Cancelled), "end was {end:?}");
    assert_eq!(session.history().len(), 4);
    // 当前调用保留部分执行结果，剩余调用补取消占位。
    assert!(tool_result_content(session.history(), 2).contains("已部分执行"));
    assert_eq!(tool_result_content(session.history(), 3), "用户取消执行");
}

#[test]
fn quit_during_cleanup_exits_after_placeholders() {
    let mut session = session();
    session.launcher = Arc::new(ScriptedLauncher::new(vec![vec![StreamEvent::Final(
        Box::new(assistant_with_calls(vec![
            call("c1", "bash", CMD),
            call("c2", "bash", CMD),
        ])),
    )]]));
    // 执行中一次取消 + 清理期间升级第二次：收尾完成后退出。
    session.tools = registry_with(InjectingTool { injects: 2 });
    let mut ui = ScriptedUi::new(vec![]);
    let cancel = Cancel::new();

    assert!(session.turn(&mut ui, &cancel, "go").is_err());
    assert_eq!(session.history().len(), 4);
    assert!(tool_result_content(session.history(), 2).contains("已部分执行"));
    assert_eq!(tool_result_content(session.history(), 3), "用户取消执行");
}

#[test]
fn sustained_delta_is_cancellable() {
    let mut session = session();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    session.launcher = Arc::new(ParkingLauncher {
        ready: ready_tx,
        released: Mutex::new(Some(release_rx)),
    });
    let mut ui = ScriptedUi::new(vec![]);
    let cancel = Cancel::new();
    let injector = injector_after_ready(cancel.clone(), ready_rx, release_tx, 1);

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    injector.join().unwrap();
    assert!(matches!(end, TurnEnd::Cancelled), "end was {end:?}");
    // 未完成的 assistant 响应不进入历史。
    assert_eq!(session.history().len(), 1);
}

#[test]
fn quit_during_stream_returns_quit() {
    let mut session = session();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    session.launcher = Arc::new(ParkingLauncher {
        ready: ready_tx,
        released: Mutex::new(Some(release_rx)),
    });
    let mut ui = ScriptedUi::new(vec![]);
    let cancel = Cancel::new();
    let injector = injector_after_ready(cancel.clone(), ready_rx, release_tx, 2);

    assert!(session.turn(&mut ui, &cancel, "go").is_err());
    injector.join().unwrap();
    assert_eq!(session.history().len(), 1);
}

#[test]
fn disconnect_without_terminal_is_error() {
    let mut session = session();
    // 空脚本：事件线程立即结束，通道断开且无终态 → 防御性按 Error 收尾。
    session.launcher = Arc::new(ScriptedLauncher::new(vec![vec![]]));
    let mut ui = ScriptedUi::new(vec![]);
    let cancel = Cancel::new();

    match session.turn(&mut ui, &cancel, "go").unwrap() {
        TurnEnd::Error(e) => assert!(e.contains("异常断开"), "error was {e}"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn render_failure_during_stream_aborts_before_commit() {
    let mut session = session();
    session.launcher = Arc::new(ScriptedLauncher::new(vec![vec![
        StreamEvent::Delta {
            text: Some("partial".into()),
            reasoning: None,
        },
        text_final("done"),
    ]]));
    // 首个 Delta 渲染即失败：流收尾后不提交 Assistant，不执行工具。
    let mut ui = FailingUi::new(0);
    let cancel = Cancel::new();

    match session.turn(&mut ui, &cancel, "go").unwrap() {
        rho::chat::TurnEnd::Fatal(e) => assert!(e.contains("EPIPE"), "error was {e}"),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(session.history().len(), 1);
}

#[test]
fn render_failure_mid_batch_placeholders_remaining_then_fatal() {
    let mut session = session();
    session.launcher = Arc::new(ScriptedLauncher::new(vec![vec![StreamEvent::Final(
        Box::new(assistant_with_calls(vec![
            call("c1", "bash", CMD),
            call("c2", "bash", CMD),
        ])),
    )]]));
    // 第一次 print（ToolCall 行）成功，第二个调用的结果渲染开始失败。
    let mut ui = FailingUi::new(1);
    session.tools = registry_with(PresetTool {
        output: "hi".into(),
    });
    let cancel = Cancel::new();

    match session.turn(&mut ui, &cancel, "go").unwrap() {
        rho::chat::TurnEnd::Fatal(e) => assert!(e.contains("EPIPE"), "error was {e}"),
        other => panic!("unexpected {other:?}"),
    }
    // 配对不变式：Assistant 提交后每个调用都有 Tool 结果。
    assert_eq!(session.history().len(), 4);
    assert!(tool_result_content(session.history(), 2).contains("hi"));
    assert_eq!(
        tool_result_content(session.history(), 3),
        "终端输出失败，未执行"
    );
}

#[test]
fn invalid_tool_call_ids_are_rejected_before_submit() {
    for calls in [
        vec![call("", "bash", CMD), call("c2", "bash", CMD)],
        vec![call("c1", "bash", CMD), call("c1", "bash", CMD)],
    ] {
        let mut session = session();
        session.launcher = Arc::new(ScriptedLauncher::new(vec![vec![StreamEvent::Final(
            Box::new(assistant_with_calls(calls)),
        )]]));
        let mut ui = ScriptedUi::new(vec![]);
        let cancel = Cancel::new();

        match session.turn(&mut ui, &cancel, "go").unwrap() {
            TurnEnd::Error(e) => assert!(e.contains("id 非法或重复"), "error was {e}"),
            other => panic!("unexpected {other:?}"),
        }
        // 非法 Assistant 不提交：历史只有用户输入。
        assert_eq!(session.history().len(), 1);
    }
}

#[test]
fn non_object_and_invalid_tool_args_are_counted_failures() {
    let mut session = session();
    session.launcher = Arc::new(ScriptedLauncher::new(vec![
        vec![StreamEvent::Final(Box::new(assistant_with_calls(vec![
            call("c1", "bash", "[]"),
            call("c2", "bash", r#"{"command":"echo hi","extra":1}"#),
            call("c3", "bash", r#"{}"#),
        ])))],
        vec![text_final("done")],
    ]));
    let mut ui = ScriptedUi::new(vec![]);
    let cancel = Cancel::new();

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    assert!(matches!(end, TurnEnd::Done), "end was {end:?}");
    assert!(tool_result_content(session.history(), 2).contains("arguments 必须是 JSON object"));
    assert!(tool_result_content(session.history(), 3).contains("unknown field"));
    assert!(tool_result_content(session.history(), 4).contains("missing field"));
}

#[test]
fn builtin_write_read_edit_dispatch_through_registry() {
    let mut session = session();
    let path = std::env::temp_dir().join(format!(
        "rho-agent-tools-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&path);
    let path = path.display().to_string();
    let write_args = json!({"path": path, "content": "hello\nworld\n"}).to_string();
    let read_args = json!({"path": path}).to_string();
    let edit_args = json!({
        "path": path,
        "edits": [{"old_text": "world", "new_text": "rho"}]
    })
    .to_string();

    session.launcher = Arc::new(ScriptedLauncher::new(vec![
        vec![StreamEvent::Final(Box::new(assistant_with_calls(vec![
            call("c1", "write", &write_args),
            call("c2", "read", &read_args),
            call("c3", "edit", &edit_args),
        ])))],
        vec![text_final("done")],
    ]));
    let mut ui = ScriptedUi::new(vec![]);
    let cancel = Cancel::new();

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    assert!(matches!(end, TurnEnd::Done), "end was {end:?}");
    assert!(tool_result_content(session.history(), 2).contains("[action] created"));
    assert!(tool_result_content(session.history(), 3).contains("[status] success"));
    assert!(tool_result_content(session.history(), 4).contains("[replacements] 1"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\nrho\n");
    assert!(ui.rendered.iter().any(|e| matches!(
        e,
        rho::render::RenderEvent::ToolResult { name, display: None, .. } if name == "read"
    )));
    let _ = std::fs::remove_file(&path);
}

fn usage(input: u64, output: u64, total: u64, cached: Option<u64>) -> TokenUsage {
    TokenUsage {
        input,
        output,
        total,
        cached_input: cached,
    }
}

/// 最后一个状态栏事件（没有则 panic）。
fn last_status(ui: &ScriptedUi) -> rho::render::StatusLine {
    match ui.rendered.last() {
        Some(rho::render::RenderEvent::Status(status)) => status.clone(),
        other => panic!("unexpected last event: {other:?}"),
    }
}

#[test]
fn status_reports_the_last_completed_request_in_the_turn() {
    let mut session = session();
    session.launcher = Arc::new(ScriptedLauncher::new(vec![
        // 第一次请求：调用工具，带 usage。
        vec![
            StreamEvent::Usage(usage(120, 30, 150, Some(96))),
            StreamEvent::Final(Box::new(assistant_with_calls(vec![call(
                "c1", "bash", CMD,
            )]))),
        ],
        // 第二次请求：最终答复，usage 覆盖前一次。
        vec![
            StreamEvent::Usage(usage(200, 50, 250, None)),
            text_final("done"),
        ],
    ]));
    session.tools = registry_with(PresetTool {
        output: "hi".into(),
    });
    let mut ui = ScriptedUi::new(vec![]);
    let cancel = Cancel::new();

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    assert!(matches!(end, TurnEnd::Done), "end was {end:?}");
    // 一轮只有一个状态栏，且用量是最后一次成功请求的。
    assert_eq!(
        ui.rendered
            .iter()
            .filter(|e| matches!(e, rho::render::RenderEvent::Status(_)))
            .count(),
        1
    );
    let status = last_status(&ui);
    assert_eq!(status.turn, 1);
    assert_eq!(status.model, "mock/m");
    assert_eq!(status.usage, Some(usage(200, 50, 250, None)));
    assert_eq!(status.context_window, 1000);
}

#[test]
fn cancelled_turn_still_prints_status_with_earlier_usage() {
    let mut session = session();
    session.launcher = Arc::new(ScriptedLauncher::new(vec![vec![
        StreamEvent::Usage(usage(120, 30, 150, Some(96))),
        StreamEvent::Final(Box::new(assistant_with_calls(vec![call(
            "c1", "bash", CMD,
        )]))),
    ]]));
    session.tools = registry_with(InjectingTool { injects: 1 });
    let mut ui = ScriptedUi::new(vec![]);
    let cancel = Cancel::new();

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    assert!(matches!(end, TurnEnd::Cancelled), "end was {end:?}");
    // 工具被取消（结果标为打断），然后补状态栏。
    assert!(ui.rendered.iter().any(|e| matches!(
        e,
        rho::render::RenderEvent::ToolResult { status, .. } if *status == ToolStatus::Interrupted
    )));
    assert_eq!(last_status(&ui).usage, Some(usage(120, 30, 150, Some(96))));
}

#[test]
fn cancelled_first_request_reports_unknown_usage() {
    let mut session = session();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    session.launcher = Arc::new(ParkingLauncher {
        ready: ready_tx,
        released: Mutex::new(Some(release_rx)),
    });
    let mut ui = ScriptedUi::new(vec![]);
    let cancel = Cancel::new();
    let injector = injector_after_ready(cancel.clone(), ready_rx, release_tx, 1);

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    injector.join().unwrap();
    assert!(matches!(end, TurnEnd::Cancelled), "end was {end:?}");
    assert_eq!(last_status(&ui).usage, None);
}

#[test]
fn quit_paths_do_not_print_status() {
    let mut session = session();
    session.launcher = Arc::new(ScriptedLauncher::new(vec![vec![StreamEvent::Final(
        Box::new(assistant_with_calls(vec![call("c1", "bash", CMD)])),
    )]]));
    // 执行中两次注入：升级为 quit，回合以 Err 上抛。
    session.tools = registry_with(InjectingTool { injects: 2 });
    let mut ui = ScriptedUi::new(vec![]);
    let cancel = Cancel::new();

    assert!(session.turn(&mut ui, &cancel, "go").is_err());
    assert!(!ui
        .rendered
        .iter()
        .any(|e| matches!(e, rho::render::RenderEvent::Status(_))));
}

#[test]
fn status_turn_counts_user_inputs_and_resets_on_clear() {
    let mut session = session();
    session.launcher = Arc::new(ScriptedLauncher::new(vec![
        vec![text_final("one")],
        vec![text_final("two")],
        vec![text_final("three")],
    ]));
    let cancel = Cancel::new();

    let mut first = ScriptedUi::new(vec![]);
    session.turn(&mut first, &cancel, "go").unwrap();
    assert_eq!(last_status(&first).turn, 1);

    let mut second = ScriptedUi::new(vec![]);
    session.turn(&mut second, &cancel, "go").unwrap();
    assert_eq!(last_status(&second).turn, 2);

    // /clear 重置历史：轮次归零，下一次输入重新算作第 1 轮。
    session.clear();
    let mut third = ScriptedUi::new(vec![]);
    session.turn(&mut third, &cancel, "go").unwrap();
    assert_eq!(last_status(&third).turn, 1);
}

/// 自定义顶层配置的会话（压缩参数覆盖用）。
fn session_with(top_extra: &str) -> Session {
    let toml = format!(
        r#"
{top_extra}

[providers.mock]
protocol = "openai_chat"
base_url = "http://127.0.0.1:1"

[models."mock/m"]
context_window = 1000
max_output = 100
"#
    );
    let cfg = config::load_str(&toml, "<测试配置>").unwrap();
    let selection = config::resolve_selection(&cfg, None, None).unwrap();
    let adapter = provider::adapter_for(&selection.provider).unwrap();
    Session::new(cfg, selection, adapter)
}

/// 跑满 n 轮对话，为压缩准备历史。
fn fill_turns(session: &mut Session, ui: &mut ScriptedUi, cancel: &Cancel, n: usize) {
    for i in 1..=n {
        session.turn(ui, cancel, &format!("u{i}")).unwrap();
    }
}

#[test]
fn compact_replaces_older_turns_with_a_summary() {
    let launcher = RecordingLauncher::new(vec![
        vec![text_final("a1")],
        vec![text_final("a2")],
        vec![text_final("a3")],
        vec![text_final("CHECKPOINT")],
    ]);
    let mut session = session();
    session.launcher = Arc::new(launcher.clone());
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(vec![]);
    fill_turns(&mut session, &mut ui, &cancel, 3);
    ui.rendered.clear();

    session.compact(&mut ui, &cancel).unwrap();

    // 请求体：被压掉的那一轮原文 + 末尾一条指令，tools 与普通回合同形。
    let bodies = launcher.bodies();
    let body = &bodies[3];
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["content"], "u1");
    assert_eq!(messages[1]["content"], "a1");
    assert_eq!(messages[2]["role"], "user");
    assert_eq!(messages[2]["content"], rho::context::DEFAULT_COMPACT_PROMPT);
    assert_eq!(body["tools"].as_array().unwrap().len(), 4);
    // thinking 片段不发送。
    assert!(body.get("reasoning_effort").is_none());
    assert!(body.get("thinking").is_none());

    // 历史：摘要 + 保留的两轮。
    let history = session.history();
    assert!(matches!(&history[0], Msg::Summary(s) if s == "CHECKPOINT"));
    assert!(matches!(&history[1], Msg::User(u) if u == "u2"));
    assert_eq!(history.len(), 5);

    // 一行结果，且不产生状态栏（压缩不是对话回合）。
    assert!(ui.rendered.iter().any(
        |e| matches!(e, rho::render::RenderEvent::TextDelta(t) if t.contains("已压缩 2 条消息"))
    ));
    assert!(!ui
        .rendered
        .iter()
        .any(|e| matches!(e, rho::render::RenderEvent::Status(_))));
}

#[test]
fn compact_uses_configured_prompt_and_keep_turns() {
    let launcher = RecordingLauncher::new(vec![
        vec![text_final("a1")],
        vec![text_final("a2")],
        vec![text_final("CHECKPOINT")],
    ]);
    let mut session = session_with("[compact]\nprompt = \"SUM\"\nkeep_turns = 1");
    session.launcher = Arc::new(launcher.clone());
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(vec![]);
    fill_turns(&mut session, &mut ui, &cancel, 2);

    session.compact(&mut ui, &cancel).unwrap();

    let bodies = launcher.bodies();
    let messages = bodies[2]["messages"].as_array().unwrap();
    assert_eq!(messages.last().unwrap()["content"], "SUM");
    // keep_turns = 1：只留最后一轮。
    let history = session.history();
    assert!(matches!(&history[0], Msg::Summary(s) if s == "CHECKPOINT"));
    assert!(matches!(&history[1], Msg::User(u) if u == "u2"));
    assert_eq!(history.len(), 3);
}

#[test]
fn compact_without_enough_turns_reports_and_sends_nothing() {
    let launcher = RecordingLauncher::new(vec![vec![text_final("a1")]]);
    let mut session = session();
    session.launcher = Arc::new(launcher.clone());
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(vec![]);
    fill_turns(&mut session, &mut ui, &cancel, 1);
    ui.rendered.clear();

    session.compact(&mut ui, &cancel).unwrap();

    assert_eq!(launcher.bodies().len(), 1);
    assert!(ui
        .rendered
        .iter()
        .any(|e| matches!(e, rho::render::RenderEvent::Error(t) if t.contains("历史不足以压缩"))));
}

#[test]
fn compact_failure_leaves_history_unchanged() {
    let launcher = RecordingLauncher::new(vec![
        vec![text_final("a1")],
        vec![text_final("a2")],
        vec![text_final("a3")],
        vec![StreamEvent::Error("上游 400".into())],
    ]);
    let mut session = session();
    session.launcher = Arc::new(launcher.clone());
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(vec![]);
    fill_turns(&mut session, &mut ui, &cancel, 3);
    let before = session.history().len();
    ui.rendered.clear();

    session.compact(&mut ui, &cancel).unwrap();

    assert_eq!(session.history().len(), before);
    assert!(session
        .history()
        .iter()
        .all(|m| !matches!(m, Msg::Summary(_))));
    assert!(ui
        .rendered
        .iter()
        .any(|e| matches!(e, rho::render::RenderEvent::Error(t) if t.contains("上游 400"))));
}

#[test]
fn compact_with_empty_reply_keeps_history() {
    let launcher = RecordingLauncher::new(vec![
        vec![text_final("a1")],
        vec![text_final("a2")],
        vec![text_final("a3")],
        vec![StreamEvent::Final(Box::new(AssistantMsg {
            text: None,
            reasoning: Some("只有思维链".into()),
            tool_calls: vec![],
        }))],
    ]);
    let mut session = session();
    session.launcher = Arc::new(launcher.clone());
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(vec![]);
    fill_turns(&mut session, &mut ui, &cancel, 3);
    let before = session.history().len();
    ui.rendered.clear();

    session.compact(&mut ui, &cancel).unwrap();

    assert_eq!(session.history().len(), before);
    assert!(ui
        .rendered
        .iter()
        .any(|e| matches!(e, rho::render::RenderEvent::Error(t) if t.contains("返回空内容"))));
}
