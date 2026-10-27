//! agent loop 分支测试：`ScriptedLauncher` 注入预编排 `StreamEvent` +
//! `ToolRunner` 预设结果驱动真实 `Session::turn`，无网络无进程。
//! 覆盖：取消/退出各粒度、占位补齐、错误路径与配对不变式。

use rho::chat::{AssistantMsg, Msg, Session, StreamLauncher, ToolCall, ToolRunner, TurnEnd};
use rho::config::{self, BashToolConfig, Provider};
use rho::provider::{self, ProviderAdapter, StreamEvent};
use rho::term::Cancel;
use rho::tools::{BashEnd, BashOutput};
use rho::ui::{ConfirmOutcome, ReadOutcome, ScriptedUi, Ui};

use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 测试用临时文件名的进程内序号：`SystemTime` 分辨率不足以保证并行测试不撞名。
static TMP_SEQ: AtomicUsize = AtomicUsize::new(0);

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
    let path = std::env::temp_dir().join(format!(
        "rho-agentloop-{}-{}.toml",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::write(&path, CONFIG_TOML).unwrap();
    let cfg = config::load(&Some(path.display().to_string())).unwrap();
    let _ = std::fs::remove_file(path);
    let selection = config::resolve_selection(&cfg, None, None).unwrap();
    let adapter = provider::adapter_for(&selection.provider).unwrap();
    Session::new(cfg, selection, adapter, false)
}

fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
    ToolCall { id: id.into(), name: name.into(), arguments: arguments.into() }
}

fn assistant_with_calls(calls: Vec<ToolCall>) -> AssistantMsg {
    AssistantMsg { text: None, reasoning: None, tool_calls: calls }
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
        Self { scripts: Mutex::new(scripts) }
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
        let script = self
            .scripts
            .lock()
            .unwrap()
            .remove(0);
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

/// 预设结果 runner：固定输出，不启动任何进程。
struct PresetRunner {
    output: String,
}

impl ToolRunner for PresetRunner {
    fn run(&self, _command: &str, _cfg: &BashToolConfig, _cancel: &Cancel) -> BashOutput {
        BashOutput { content: self.output.clone(), end: BashEnd::Exited(0) }
    }
}

/// 预设结果 runner：run 期间注入指定次数的 Ctrl-C（模拟执行中被取消/升级 quit）。
struct InjectingRunner {
    injects: usize,
}

impl ToolRunner for InjectingRunner {
    fn run(&self, _command: &str, _cfg: &BashToolConfig, cancel: &Cancel) -> BashOutput {
        for _ in 0..self.injects {
            cancel.inject();
        }
        BashOutput {
            content: "partial\n[Command cancelled: 已部分执行（被取消，副作用已发生）]\n".into(),
            end: BashEnd::Cancelled,
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
    confirms: std::collections::VecDeque<ConfirmOutcome>,
    fatal: Option<String>,
}

impl FailingUi {
    fn new(prints_left: usize, confirms: Vec<ConfirmOutcome>) -> Self {
        Self { prints_left, confirms: confirms.into(), fatal: None }
    }
}

impl Ui for FailingUi {
    fn read_line(&mut self) -> ReadOutcome {
        ReadOutcome::Eof
    }

    fn confirm(&mut self, _cwd: &str, _cmd: &str) -> ConfirmOutcome {
        self.confirms.pop_front().unwrap_or(ConfirmOutcome::No)
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
    // 未知工具/参数错误不进 gate；只有 c3 弹确认（拒绝，不执行）。
    let mut ui = ScriptedUi::new(vec![], vec![ConfirmOutcome::No]);
    let cancel = Cancel::new();

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    assert!(matches!(end, TurnEnd::Done), "end was {end:?}");
    // User + Assistant(3 calls) + 3×Tool + Assistant(done)
    assert_eq!(session.history.len(), 6);
    assert!(tool_result_content(&session.history, 2).contains("未知工具：nope"));
    assert!(tool_result_content(&session.history, 3).contains("参数解析失败"));
    assert_eq!(tool_result_content(&session.history, 4), "用户拒绝执行");
    match &session.history[5] {
        Msg::Assistant(a) => {
            assert_eq!(a.text.as_deref(), Some("done"));
            assert!(a.tool_calls.is_empty());
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn confirm_cancel_places_placeholders_for_remaining() {
    let mut session = session();
    session.launcher = Arc::new(ScriptedLauncher::new(vec![vec![
        StreamEvent::Final(Box::new(assistant_with_calls(vec![
            call("c1", "bash", CMD),
            call("c2", "bash", CMD),
        ]))),
    ]]));
    let mut ui = ScriptedUi::new(vec![], vec![ConfirmOutcome::Cancelled]);
    let cancel = Cancel::new();

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    assert!(matches!(end, TurnEnd::Cancelled), "end was {end:?}");
    assert_eq!(session.history.len(), 4);
    assert_eq!(tool_result_content(&session.history, 2), "用户取消执行");
    assert_eq!(tool_result_content(&session.history, 3), "用户取消执行");
}

#[test]
fn execution_cancel_keeps_partial_result() {
    let mut session = session();
    session.launcher = Arc::new(ScriptedLauncher::new(vec![vec![
        StreamEvent::Final(Box::new(assistant_with_calls(vec![
            call("c1", "bash", CMD),
            call("c2", "bash", CMD),
        ]))),
    ]]));
    session.tool_runner = Arc::new(InjectingRunner { injects: 1 });
    let mut ui = ScriptedUi::new(vec![], vec![ConfirmOutcome::Yes]);
    let cancel = Cancel::new();

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    assert!(matches!(end, TurnEnd::Cancelled), "end was {end:?}");
    assert_eq!(session.history.len(), 4);
    // 当前调用保留部分执行结果，剩余调用补取消占位。
    assert!(tool_result_content(&session.history, 2).contains("已部分执行"));
    assert_eq!(tool_result_content(&session.history, 3), "用户取消执行");
}

#[test]
fn quit_during_cleanup_exits_after_placeholders() {
    let mut session = session();
    session.launcher = Arc::new(ScriptedLauncher::new(vec![vec![
        StreamEvent::Final(Box::new(assistant_with_calls(vec![
            call("c1", "bash", CMD),
            call("c2", "bash", CMD),
        ]))),
    ]]));
    // 执行中一次取消 + 清理期间升级第二次：收尾完成后退出。
    session.tool_runner = Arc::new(InjectingRunner { injects: 2 });
    let mut ui = ScriptedUi::new(vec![], vec![ConfirmOutcome::Yes]);
    let cancel = Cancel::new();

    assert!(session.turn(&mut ui, &cancel, "go").is_err());
    assert_eq!(session.history.len(), 4);
    assert!(tool_result_content(&session.history, 2).contains("已部分执行"));
    assert_eq!(tool_result_content(&session.history, 3), "用户取消执行");
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
    let mut ui = ScriptedUi::new(vec![], vec![]);
    let cancel = Cancel::new();
    let injector = injector_after_ready(cancel.clone(), ready_rx, release_tx, 1);

    let end = session.turn(&mut ui, &cancel, "go").unwrap();
    injector.join().unwrap();
    assert!(matches!(end, TurnEnd::Cancelled), "end was {end:?}");
    // 未完成的 assistant 响应不进入历史。
    assert_eq!(session.history.len(), 1);
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
    let mut ui = ScriptedUi::new(vec![], vec![]);
    let cancel = Cancel::new();
    let injector = injector_after_ready(cancel.clone(), ready_rx, release_tx, 2);

    assert!(session.turn(&mut ui, &cancel, "go").is_err());
    injector.join().unwrap();
    assert_eq!(session.history.len(), 1);
}

#[test]
fn disconnect_without_terminal_is_error() {
    let mut session = session();
    // 空脚本：事件线程立即结束，通道断开且无终态 → 防御性按 Error 收尾。
    session.launcher = Arc::new(ScriptedLauncher::new(vec![vec![]]));
    let mut ui = ScriptedUi::new(vec![], vec![]);
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
        StreamEvent::Delta { text: Some("partial".into()), reasoning: None },
        text_final("done"),
    ]]));
    // 首个 Delta 渲染即失败：流收尾后不提交 Assistant，不执行工具。
    let mut ui = FailingUi::new(0, vec![]);
    let cancel = Cancel::new();

    match session.turn(&mut ui, &cancel, "go").unwrap() {
        rho::chat::TurnEnd::Fatal(e) => assert!(e.contains("EPIPE"), "error was {e}"),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(session.history.len(), 1);
}

#[test]
fn render_failure_mid_batch_placeholders_remaining_then_fatal() {
    let mut session = session();
    session.launcher = Arc::new(ScriptedLauncher::new(vec![vec![
        StreamEvent::Final(Box::new(assistant_with_calls(vec![
            call("c1", "bash", CMD),
            call("c2", "bash", CMD),
        ]))),
    ]]));
    // 第一次 print（ToolCall 行）成功，第二个调用的结果渲染开始失败。
    let mut ui = FailingUi::new(1, vec![ConfirmOutcome::Yes]);
    session.tool_runner = Arc::new(PresetRunner {
        output: "hi\n[Command finished with exit code 0]".into(),
    });
    let cancel = Cancel::new();

    match session.turn(&mut ui, &cancel, "go").unwrap() {
        rho::chat::TurnEnd::Fatal(e) => assert!(e.contains("EPIPE"), "error was {e}"),
        other => panic!("unexpected {other:?}"),
    }
    // 配对不变式：Assistant 提交后每个调用都有 Tool 结果。
    assert_eq!(session.history.len(), 4);
    assert!(tool_result_content(&session.history, 2).contains("hi"));
    assert_eq!(tool_result_content(&session.history, 3), "终端输出失败，未执行");
}

#[test]
fn invalid_tool_call_ids_are_rejected_before_submit() {
    for calls in [
        vec![call("", "bash", CMD), call("c2", "bash", CMD)],
        vec![call("c1", "bash", CMD), call("c1", "bash", CMD)],
    ] {
        let mut session = session();
        session.launcher = Arc::new(ScriptedLauncher::new(vec![vec![
            StreamEvent::Final(Box::new(assistant_with_calls(calls))),
        ]]));
        let mut ui = ScriptedUi::new(vec![], vec![]);
        let cancel = Cancel::new();

        match session.turn(&mut ui, &cancel, "go").unwrap() {
            TurnEnd::Error(e) => assert!(e.contains("id 非法或重复"), "error was {e}"),
            other => panic!("unexpected {other:?}"),
        }
        // 非法 Assistant 不提交：历史只有用户输入。
        assert_eq!(session.history.len(), 1);
    }
}
