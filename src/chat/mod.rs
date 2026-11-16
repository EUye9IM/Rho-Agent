//! 会话状态与 agent 工具循环的编排。
//! 一轮回合的流接收循环与取消判定在 `turn` 子模块；一批 tool_calls 的执行、
//! 结果提交与占位补齐在 `batch` 子模块。供应商/模型/思考档位的切换规则是
//! 配置域纯函数（`config::switch_*_selection`），这里只持有结果并按需重建适配器。

mod batch;
mod turn;

pub use crate::message::{AssistantMsg, Msg, TokenUsage, ToolCall};

use crate::config::{self, Config, Provider, Selection};
use crate::context;
use crate::provider::{self, ProviderAdapter, StreamEvent};
use crate::render::RenderEvent;
use crate::term::Cancel;
use crate::tools::ToolRegistry;
use crate::ui::Ui;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;

/// 压缩被取消时展示的提示。
const COMPACT_CANCELLED_MSG: &str = "已取消压缩";
/// 没有可压缩的轮次时展示的提示。
const COMPACT_EMPTY_MSG: &str = "历史不足以压缩（可压缩的轮次不足）";
/// 摘要请求没有返回正文时展示的提示。
const COMPACT_EMPTY_RESULT_MSG: &str = "压缩返回空内容，历史未改动";

fn system_history(prompt: Option<&str>) -> Vec<Msg> {
    prompt
        .map(|p| vec![Msg::System(p.to_string())])
        .unwrap_or_default()
}

/// 一次流式请求的启动边界：生产实现起 worker 线程跑真实 HTTP；
/// 测试实现注入预编排事件序列（无网络）。
pub trait StreamLauncher: Send + Sync {
    fn launch(
        &self,
        adapter: Arc<dyn ProviderAdapter>,
        provider: Provider,
        body: Value,
        headers: Vec<(String, String)>,
        cancelled: Arc<AtomicBool>,
    ) -> Receiver<StreamEvent>;
}

/// 生产实现：每轮请求一个工作线程。
struct WorkerLauncher;

impl StreamLauncher for WorkerLauncher {
    fn launch(
        &self,
        adapter: Arc<dyn ProviderAdapter>,
        provider: Provider,
        body: Value,
        headers: Vec<(String, String)>,
        cancelled: Arc<AtomicBool>,
    ) -> Receiver<StreamEvent> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            provider::stream_request(adapter, provider, body, headers, tx, cancelled);
        });
        rx
    }
}

/// 会话状态：历史 + 当前选择 + 适配器 + 工具注册表。
/// adapter 为 Arc 共享：会话长期持有，每轮 clone 一份按值传给 worker
/// （thread::spawn + detach 取消语义需 'static，不能借引用）；切换供应商时按新
/// protocol/compat 重建，与 selection 保持一致由本类型的方法维护，字段不对外暴露。
/// thinking 唯一真值源是 selection.thinking（switch_* 同步更新）。
/// launcher 与 tools 是测试注入边界：`with_launcher` / `with_tools` 替换，
/// 配置与选择经 `cfg()` / `selection()` 只读访问。
pub struct Session {
    cfg: Config,
    selection: Selection,
    adapter: Arc<dyn ProviderAdapter>,
    launcher: Arc<dyn StreamLauncher>,
    tools: ToolRegistry,
    /// rho 启动目录，用于解析工具的相对路径。
    cwd: PathBuf,
    /// 只由 agent loop 写入；对外只读，保证 tool_calls 与 Tool 结果的配对不变式。
    history: Vec<Msg>,
}

/// REPL → agent loop 的内部错误通道。
pub type ChatResult<T> = Result<T, Quit>;

/// 退出请求（REPL 层判定后传入；占位补齐与清理完成后上抛）。
#[derive(Debug)]
pub struct Quit;

/// 一轮回合的终态。
#[derive(Debug)]
pub enum TurnEnd {
    /// 正常完成（最终 assistant 无 tool_calls）
    Done,
    /// 轮内错误（已展示，保留已完成历史，回 REPL）
    Error(String),
    /// 用户取消（占位已补齐，回 REPL）
    Cancelled,
    /// 终端 I/O 失败（REPL 无法继续，exit 1；占位已补齐）
    Fatal(String),
}

impl Session {
    /// thinking 已在 main 层校验并写入 selection.thinking（resolve_thinking，非法 exit 2）。
    /// history 以 cfg.system_prompt 播种（Msg::System；省略则不含 system 消息）。
    pub fn new(cfg: Config, selection: Selection, adapter: Arc<dyn ProviderAdapter>) -> Self {
        let history = system_history(cfg.system_prompt.as_deref());
        Self {
            cfg,
            selection,
            adapter,
            launcher: Arc::new(WorkerLauncher),
            tools: ToolRegistry::builtin(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            history,
        }
    }

    /// 只读历史（测试与 REPL 检查用）；写入路径留在 agent loop 内。
    pub fn history(&self) -> &[Msg] {
        &self.history
    }

    /// 当前配置（命令层与补全快照读取）。
    pub fn cfg(&self) -> &Config {
        &self.cfg
    }

    /// 当前选择：供应商、模型与思考档位（命令层与补全快照读取）。
    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    /// 测试注入边界：替换流式请求启动器（生产为每轮起 worker 线程的 `WorkerLauncher`）。
    pub fn with_launcher(mut self, launcher: Arc<dyn StreamLauncher>) -> Self {
        self.launcher = launcher;
        self
    }

    /// 测试注入边界：替换工具注册表（默认为内置 bash / read / write / edit）。
    pub fn with_tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = tools;
        self
    }

    /// `/clear`：历史重置为仅含 system 消息（播种语义同 new）。
    pub fn clear(&mut self) {
        self.history = system_history(self.cfg.system_prompt.as_deref());
    }

    /// `/compact`：把较早的若干轮换成一条 `Msg::Summary`。
    ///
    /// 请求形状与普通回合同形（真实历史前缀的副本 + 末尾一条 user 指令，
    /// 同样的 tools 与输出上限），只是收流不渲染。摘要正文原样使用、不再裁；
    /// 失败、空回复与取消都不改动历史。
    pub fn compact(&mut self, ui: &mut dyn Ui, cancel: &Cancel) -> ChatResult<()> {
        let keep = self
            .cfg
            .compact
            .keep_turns
            .unwrap_or(context::DEFAULT_KEEP_TURNS);
        let Some(plan) = context::plan(
            &self.history,
            keep,
            self.selection.model.context_window,
            self.selection.model.max_output,
        ) else {
            emit(ui, RenderEvent::Error(COMPACT_EMPTY_MSG.into()));
            return Ok(());
        };
        let (start, end, dropped) = (plan.start, plan.end, plan.dropped());
        let prompt = self
            .cfg
            .compact
            .prompt
            .as_deref()
            .unwrap_or(context::DEFAULT_COMPACT_PROMPT);

        let mut head = plan.head;
        head.push(Msg::User(prompt.to_string()));
        // thinking 传 None：摘要不需要思维链，省预算也更快。
        let body = self.adapter.build_body(
            &head,
            &self.selection.model,
            &self.selection.model_id,
            None,
            &self.tools.specs(),
        );

        match self.drive_stream(ui, cancel, body, false, COMPACT_CANCELLED_MSG) {
            turn::StreamOutcome::Final { msg, usage: _ } => {
                let Some(summary) = msg
                    .text
                    .map(|text| text.trim().to_string())
                    .filter(|text| !text.is_empty())
                else {
                    emit(ui, RenderEvent::Error(COMPACT_EMPTY_RESULT_MSG.into()));
                    return Ok(());
                };
                let before = crate::render::compact_tokens(context::estimate_tokens(
                    &self.history[start..end],
                ));
                let after = crate::render::compact_tokens(context::estimate_text(&summary));
                self.history.splice(start..end, [Msg::Summary(summary)]);
                emit(
                    ui,
                    RenderEvent::TextDelta(format!(
                        "已压缩 {dropped} 条消息 → 摘要（约 {before} → {after} tokens）\n"
                    )),
                );
                Ok(())
            }
            // 错误与取消已由 drive_stream 渲染。
            turn::StreamOutcome::Error(_) | turn::StreamOutcome::Cancelled => Ok(()),
            turn::StreamOutcome::Quit => Err(Quit),
        }
    }

    fn build_body(&self) -> serde_json::Value {
        let thinking = self
            .selection
            .thinking
            .as_ref()
            .and_then(|level| self.selection.model.thinking.get(level));
        let tools = self.tools.specs();
        self.adapter.build_body(
            &self.history,
            &self.selection.model,
            &self.selection.model_id,
            thinking,
            &tools,
        )
    }

    /// 切换供应商（REPL /provider）：联动重置模型与 thinking，并按新 provider
    /// 重建适配器（compat / protocol 可能不同）。规则在 config::switch_provider_selection。
    pub fn switch_provider(&mut self, id: &str) -> Result<(), String> {
        let selection = config::switch_provider_selection(&self.cfg, &self.selection, id)?;
        let adapter = provider::adapter_for(&selection.provider)?;
        self.selection = selection;
        self.adapter = adapter;
        Ok(())
    }

    /// 切换模型（REPL /model）：跨供应商拒绝；thinking 重置。
    /// 规则在 config::switch_model_selection。
    pub fn switch_model(&mut self, id: &str) -> Result<(), String> {
        let selection = config::switch_model_selection(&self.cfg, &self.selection.provider_id, id)?;
        self.selection = selection;
        Ok(())
    }

    /// 切换思考档位（REPL /thinking）：无效档位拒绝；更新 selection.thinking。
    /// 规则在 config::switch_thinking_selection。
    pub fn switch_thinking(&mut self, level: &str) -> Result<(), String> {
        let selection = config::switch_thinking_selection(&self.selection, level)?;
        self.selection = selection;
        Ok(())
    }
}

/// 渲染事件转发（agent loop → ui.print）。
fn emit(ui: &mut dyn Ui, ev: RenderEvent) {
    ui.print(&ev);
}
