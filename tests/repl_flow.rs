//! REPL 退出语义、斜杠命令与供应商/模型切换规则（无网络、无 TTY）。

use rho::chat::Session;
use rho::config;
use rho::provider;
use rho::render::RenderEvent;
use rho::repl::{self, ReplExit};
use rho::term::Cancel;
use rho::ui::{ConfirmOutcome, ReadOutcome, ScriptedUi, Ui};

use std::time::{SystemTime, UNIX_EPOCH};

fn session_from(toml: &str) -> Session {
    let path = std::env::temp_dir().join(format!(
        "rho-repl-{}-{}.toml",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::write(&path, toml).unwrap();
    let cfg = config::load(&Some(path.display().to_string())).unwrap();
    let _ = std::fs::remove_file(path);
    let selection = config::resolve_selection(&cfg, None, None).unwrap();
    let adapter = provider::adapter_for(&selection.provider).unwrap();
    Session::new(cfg, selection, adapter, false)
}

fn session() -> Session {
    session_from(
        r#"
[providers.mock]
protocol = "openai_chat"
base_url = "http://127.0.0.1:1"

[models."mock/m"]
context_window = 1000
max_output = 100
"#,
    )
}

const TWO_PROVIDERS: &str = r#"
[providers.mock]
protocol = "openai_chat"
base_url = "http://127.0.0.1:1"

[providers.other]
protocol = "openai_chat"
base_url = "http://127.0.0.1:2"

[models."mock/m"]
context_window = 1000
max_output = 100
thinking_default = "high"

[models."mock/m".thinking]
low = { thinking = { type = "enabled" } }
high = { thinking = { type = "enabled" } }

[models."mock/n"]
context_window = 1000
max_output = 100
thinking_default = "high"

[models."mock/n".thinking]
low = { thinking = { type = "enabled" } }
high = { thinking = { type = "enabled" } }

[models."other/m2"]
context_window = 1000
max_output = 100
"#;

#[test]
fn slash_commands_then_exit() {
    let mut ui = ScriptedUi::new(
        vec![
            ReadOutcome::Line("/help".into()),
            ReadOutcome::Line("/model".into()),
            ReadOutcome::Line("/model mock/m".into()),
            ReadOutcome::Line("/provider".into()),
            ReadOutcome::Line("/thinking".into()),
            ReadOutcome::Line("/clear".into()),
            ReadOutcome::Line("/unknown".into()),
            ReadOutcome::Line("   ".into()),
            ReadOutcome::Line("/exit".into()),
        ],
        vec![],
    );
    let cancel = Cancel::new();
    repl::run(&mut ui, session(), &cancel).unwrap();
    assert!(ui
        .rendered
        .iter()
        .any(|e| matches!(e, RenderEvent::TextDelta(t) if t.contains("可用模型"))));
    assert!(ui
        .rendered
        .iter()
        .any(|e| matches!(e, RenderEvent::Error(t) if t.contains("未知命令"))));
}

#[test]
fn help_and_model_list_follow_current_provider() {
    let mut ui = ScriptedUi::new(
        vec![
            ReadOutcome::Line("/help".into()),
            ReadOutcome::Line("/model".into()),
            ReadOutcome::Line("/exit".into()),
        ],
        vec![],
    );
    let cancel = Cancel::new();
    repl::run(&mut ui, session_from(TWO_PROVIDERS), &cancel).unwrap();
    let text: String = ui
        .rendered
        .iter()
        .filter_map(|e| match e {
            RenderEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    // mock/m 声明了思考档位，help 应包含 /thinking。
    assert!(text.contains("/thinking"));
    // /model 只列当前 provider 的模型。
    assert!(text.contains("mock/m"));
    assert!(text.contains("mock/n"));
    assert!(!text.contains("other/m2"));
}

#[test]
fn help_hides_thinking_for_model_without_table() {
    let mut ui = ScriptedUi::new(
        vec![ReadOutcome::Line("/help".into()), ReadOutcome::Line("/exit".into())],
        vec![],
    );
    let cancel = Cancel::new();
    repl::run(&mut ui, session(), &cancel).unwrap();
    let text: String = ui
        .rendered
        .iter()
        .filter_map(|e| match e {
            RenderEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert!(!text.contains("/thinking"));
}

#[test]
fn provider_switch_same_provider_preserves_model_and_thinking() {
    let mut s = session_from(TWO_PROVIDERS);
    s.switch_model("mock/n").unwrap();
    s.switch_thinking("low").unwrap();
    s.switch_provider("mock").unwrap();
    assert_eq!(s.selection.model_id, "mock/n");
    assert_eq!(s.selection.thinking.as_deref(), Some("low"));

    s.switch_provider("other").unwrap();
    assert_eq!(s.selection.model_id, "other/m2");
    assert_eq!(s.selection.thinking, None);
}

#[test]
fn model_switch_across_provider_is_rejected_before_key_check() {
    let mut s = session_from(
        r#"
[providers.mock]
protocol = "openai_chat"
base_url = "http://127.0.0.1:1"

[providers.other]
protocol = "openai_chat"
base_url = "http://127.0.0.1:2"
[providers.other.auth]
headers = { Authorization = "Bearer ${RHO_MISSING_KEY_FOR_TEST}" }

[models."mock/m"]
context_window = 1000
max_output = 100

[models."other/m2"]
context_window = 1000
max_output = 100
"#,
    );
    let err = s.switch_model("other/m2").unwrap_err();
    assert!(err.contains("请先 /provider other"), "error was {err}");
}

#[test]
fn implicit_selection_skips_provider_without_models() {
    let mut s = session_from(
        r#"
[providers.empty]
protocol = "openai_chat"
base_url = "http://127.0.0.1:1"

[providers.mock]
protocol = "openai_chat"
base_url = "http://127.0.0.1:2"

[models."mock/m"]
context_window = 1000
max_output = 100
"#,
    );
    assert_eq!(s.selection.provider_id, "mock");
    assert_eq!(s.selection.model_id, "mock/m");
    s.switch_provider("empty").unwrap_err();
}

#[test]
fn default_model_wins_over_default_provider() {
    let s = session_from(
        r#"
default_provider = "a"
default_model = "mock/m"

[providers.a]
protocol = "openai_chat"
base_url = "http://127.0.0.1:1"

[providers.mock]
protocol = "openai_chat"
base_url = "http://127.0.0.1:2"

[models."a/m1"]
context_window = 1000
max_output = 100

[models."mock/m"]
context_window = 1000
max_output = 100
"#,
    );
    assert_eq!(s.selection.model_id, "mock/m");
    assert_eq!(s.selection.provider_id, "mock");
}

#[test]
fn unavailable_provider_is_marked_in_listing() {
    let mut ui = ScriptedUi::new(
        vec![ReadOutcome::Line("/provider".into()), ReadOutcome::Line("/exit".into())],
        vec![],
    );
    let cancel = Cancel::new();
    repl::run(
        &mut ui,
        session_from(
            r#"
[providers.mock]
protocol = "openai_chat"
base_url = "http://127.0.0.1:1"

[providers.other]
protocol = "openai_chat"
base_url = "http://127.0.0.1:2"
[providers.other.auth]
headers = { Authorization = "Bearer ${RHO_MISSING_KEY_FOR_TEST}" }

[models."mock/m"]
context_window = 1000
max_output = 100
"#,
        ),
        &cancel,
    )
    .unwrap();
    let text: String = ui
        .rendered
        .iter()
        .filter_map(|e| match e {
            RenderEvent::TextDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert!(text.contains("other"));
    assert!(text.contains("不可用"));
}

struct FatalUi;

impl Ui for FatalUi {
    fn read_line(&mut self) -> ReadOutcome {
        ReadOutcome::Eof
    }

    fn confirm(&mut self, _cwd: &str, _cmd: &str) -> ConfirmOutcome {
        ConfirmOutcome::No
    }

    fn print(&mut self, _ev: &RenderEvent) {}

    fn take_fatal(&mut self) -> Option<String> {
        Some("render boom".into())
    }
}

#[test]
fn ui_io_error_maps_to_exit_1() {
    let cancel = Cancel::new();
    match repl::run(&mut FatalUi, session(), &cancel) {
        Err(ReplExit::Fatal(e)) => assert_eq!(e, "render boom"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn eof_exits_normally() {
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(vec![ReadOutcome::Eof], vec![]);
    assert!(repl::run(&mut ui, session(), &cancel).is_ok());
}

#[test]
fn single_empty_interrupt_only_arms() {
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(
        vec![
            ReadOutcome::Interrupted { line_empty: true },
            ReadOutcome::Line("/help".into()),
        ],
        vec![],
    );
    assert!(repl::run(&mut ui, session(), &cancel).is_ok());
    // 没退出：/help 确实执行了。
    assert!(!ui.rendered.is_empty());
}

#[test]
fn double_empty_interrupt_exits() {
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(
        vec![
            ReadOutcome::Interrupted { line_empty: true },
            ReadOutcome::Interrupted { line_empty: true },
            ReadOutcome::Line("/help".into()),
        ],
        vec![],
    );
    assert!(repl::run(&mut ui, session(), &cancel).is_ok());
    // 第二次空行 Ctrl-C 已退出：后面的 /help 不该被执行。
    assert!(ui.rendered.is_empty());
}

#[test]
fn editing_interrupt_disarms_before_exit() {
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(
        vec![
            ReadOutcome::Interrupted { line_empty: true },
            // 编辑中（行非空）Ctrl-C：清行并撤销武装，回提示符。
            ReadOutcome::Interrupted { line_empty: false },
            ReadOutcome::Interrupted { line_empty: true },
            ReadOutcome::Line("/help".into()),
        ],
        vec![],
    );
    assert!(repl::run(&mut ui, session(), &cancel).is_ok());
    // 第三次 Ctrl-C 只是重新武装，不该退出。
    assert!(!ui.rendered.is_empty());
}

#[test]
fn submitted_line_clears_arm() {
    let cancel = Cancel::new();
    let mut ui = ScriptedUi::new(
        vec![
            ReadOutcome::Interrupted { line_empty: true },
            // 空行回车清掉武装。
            ReadOutcome::Line(String::new()),
            ReadOutcome::Interrupted { line_empty: true },
            ReadOutcome::Line("/help".into()),
        ],
        vec![],
    );
    assert!(repl::run(&mut ui, session(), &cancel).is_ok());
    assert!(!ui.rendered.is_empty());
}
