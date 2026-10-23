//! 内部消息模型与 agent 工具循环。见 architecture.md 三/四节。

use crate::config::{Config, Selection};
use crate::provider::ProviderAdapter;
use crate::render::RenderEvent;
use crate::term::Cancel;
use crate::ui::Ui;
use std::sync::Arc;

/// 与协议解耦的消息模型（适配层负责映射）。
#[derive(Debug, Clone)]
pub enum Msg {
    System(String),
    User(String),
    Assistant {
        text: Option<String>,
        reasoning: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

/// arguments 保持原始 JSON 字符串。
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// 会话状态：历史 + 当前选择 + 适配器 + 工具步数。
/// adapter 为 Arc 共享：会话长期持有，每轮 clone 一份按值传给 worker
/// （thread::spawn + detach 取消语义需 'static，不能借引用）。
/// thinking 唯一真值源是 selection.thinking（switch_* 同步更新）。
pub struct Session {
    pub cfg: Config,
    pub selection: Selection,
    pub adapter: Arc<dyn ProviderAdapter>,
    pub yes: bool,
    pub history: Vec<Msg>,
    /// 本轮工具步数（design §7：每次用户输入触发的一轮对话计数，turn 开头重置）
    pub steps: u32,
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
}

impl Session {
    /// thinking 已在 main 层校验并写入 selection.thinking（resolve_thinking，非法 exit 2）。
    /// history 以 cfg.system_prompt 播种（Msg::System；省略则不含 system 消息）。
    pub fn new(
        cfg: Config,
        selection: Selection,
        adapter: Arc<dyn ProviderAdapter>,
        yes: bool,
    ) -> Self {
        let history = match &cfg.system_prompt {
            Some(p) => vec![Msg::System(p.clone())],
            None => Vec::new(),
        };
        Self { cfg, selection, adapter, yes, history, steps: 0 }
    }

    /// `/clear`：历史重置为仅含 system 消息（播种语义同 new）；steps 归零。
    pub fn clear(&mut self) {
        // TODO: history = cfg.system_prompt 播种（或空）
        todo!()
    }

    /// agent 工具循环（architecture.md 四节）。
    pub fn turn(&mut self, ui: &mut dyn Ui, cancel: &Cancel, input: &str) -> ChatResult<TurnEnd> {
        // TODO: steps = 0（per-turn 限额，design §7）→ push User msg → loop {
        //   build_body（history, model, model_id, thinking 片段）→ worker 线程流式请求
        //   （mpsc 事件；adapter clone 按值传入 worker）
        //   主线程 recv_timeout 短轮询，逐事件先查 cancel 再处理：
        //     Delta → 渲染；Done → 记录提交资格；Final → 退出接收
        //     Error / Disconnected → 展示，回合结束（保留已完成历史）
        //   流式期间 Ctrl-C → drop Receiver，回 REPL（若 quit 则占位补齐后退出）
        //   Final 后：无 tool_calls → 提交，回合结束
        //   有 tool_calls → 提交前校验 tool_call_id 唯一非空（非法整条按 Error）
        //     → 逐个执行（每调用前查 steps < max_tool_steps 与取消）：
        //       超限 → 当前及剩余调用补“超限未执行”Tool 结果，回合结束
        //         （后续轮次重新计数）
        //       参数解析失败/未知工具名/spawn 失败 → 真实错误文本 Tool 结果，
        //         steps += 1，剩余调用继续逐个判定
        //       判定（allowlist/元字符/确认/--yes）：
        //         Yes → 执行 bash（前台，超时升级，Ctrl-C 升级）
        //         No → “用户拒绝执行”，steps += 1
        //         Cancelled → 当前及剩余补“用户取消执行”，回 REPL
        //           （若该 Ctrl-C 为当前轮第二次，收尾后退出）
        //       执行中超时 → “已部分执行（超时）”，steps += 1，继续剩余调用
        //       执行中被取消 → 当前补“已部分执行（被取消，副作用已发生）”，
        //         剩余补“用户取消执行”，清理进程组后回 REPL（若 quit 则退出）
        //       正常完成 → 真实结果，steps += 1
        //   全部执行完 → continue（带上 tool 结果再请求）
        // }
        let _ = (ui, cancel, input);
        todo!()
    }

    /// 切换供应商（REPL /provider）：联动重置模型与 thinking，并按新 provider
    /// 重建适配器（compat / protocol 可能不同）。
    pub fn switch_provider(&mut self, id: &str) -> Result<(), String> {
        // TODO: 新 provider 声明顺序第一个模型 + thinking_default；
        //       key 不齐全 → 拒绝命令列缺失变量，状态不变；
        //       成功时 selection 更新 + adapter = provider::adapter_for(new)?
        let _ = id;
        todo!()
    }

    /// 切换模型（REPL /model）：跨 provider 拒绝；thinking 重置。
    pub fn switch_model(&mut self, id: &str) -> Result<(), String> {
        // TODO
        todo!()
    }

    /// 切换思考档位（REPL /thinking）：无效档位拒绝；更新 selection.thinking。
    pub fn switch_thinking(&mut self, level: &str) -> Result<(), String> {
        // TODO: 校验属当前模型 thinking 表；selection.thinking = Some(level)
        let _ = level;
        todo!()
    }
}

/// 执行单个工具调用（在 turn 的逐个循环中被调用）。
/// 返回该调用的 Tool 消息内容（真实结果 / 错误文本 / 占位文本）。
enum CallOutcome {
    Result(String),      // 正常完成 / 真实错误文本 / 占位文本
    Cancelled(String),   // 取消：当前"已部分执行/未执行"文本；剩余调用补占位
}

#[allow(dead_code)]
fn dispatch_tool_call(
    session: &mut Session,
    ui: &mut dyn Ui,
    cancel: &Cancel,
    call: &ToolCall,
) -> ChatResult<CallOutcome> {
    // TODO: 参数解析失败/未知工具名/spawn 失败 → 真实错误文本 Result
    //       （steps += 1，继续剩余；spawn 失败对应 BashEnd::SpawnFailed）
    // TODO: allowlist/元字符判定（tools::allowed）→ 自动执行或确认：
    //   ConfirmOutcome::Yes → tools::run_bash（超时升级 / Ctrl-C 升级）
    //   No → "用户拒绝执行"
    //   Cancelled → Cancelled（若 quit 则收尾后退出）
    // TODO: 执行中超时 → "已部分执行（超时）"，继续剩余调用
    // TODO: 执行中被取消 → Cancelled("已部分执行（被取消，副作用已发生）")
    // TODO: 执行中 quit → BashEnd::QuitRequested → 当前补占位，剩余补占位，退出程序
    let _ = (session, ui, cancel, call);
    todo!()
}

/// 渲染事件转发（agent loop → ui.print）。
#[allow(dead_code)]
fn emit(ui: &mut dyn Ui, ev: RenderEvent) {
    ui.print(&ev);
}

/// 空适配器占位（provider 分发在 provider/mod.rs）。
#[allow(dead_code)]
fn adapter_for(provider: &crate::config::Provider) -> Result<Arc<dyn ProviderAdapter>, String> {
    crate::provider::adapter_for(provider)
}
