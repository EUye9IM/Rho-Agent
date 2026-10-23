//! 用户交互抽象：Ui trait + 生产实现 TermUi + 测试实现 ScriptedUi。
//! Ui 不承载退出语义（退出由 REPL 层判定）。见 architecture.md 三节。

use crate::render::RenderEvent;
use std::collections::VecDeque;

/// 读取一行输入的结果。
#[derive(Debug)]
pub enum ReadOutcome {
    /// 普通输入或斜杠命令（Ui 不识别命令）
    Line(String),
    /// Ctrl-C：编辑中（false）清空输入；空行（true）退出
    /// —— 走 rustyline 按键路径，不经信号记录
    Interrupted { line_empty: bool },
    /// Ctrl-D
    Eof,
}

/// bash 确认结果。
#[derive(Debug)]
pub enum ConfirmOutcome {
    /// 执行
    Yes,
    /// 拒绝（y/N 默认 N），喂回模型
    No,
    /// Ctrl-C：取消当前回合（走 SIGINT 信号路径，计数由信号线程记录）
    Cancelled,
}

pub trait Ui {
    fn read_line(&mut self) -> ReadOutcome;
    fn confirm(&mut self, cwd: &str, cmd: &str) -> ConfirmOutcome;
    fn print(&mut self, ev: &RenderEvent);
}

/// 生产实现：rustyline 后端。
///
/// - `stdin 非 TTY 拒绝启动`的检查在 `TermUi::new` 里做
/// - `confirm` 保持 ISIG 的可中断读取：Ctrl-C 产生 SIGINT 唤醒读取返回
///   `Cancelled`（记录由专用信号线程追加，此处不自行追加，无双重计数）
/// - REPL 输入态 Ctrl-C 由 rustyline 返回 Interrupted（其原始模式清 ISIG，
///   与 confirm 的信号路径互斥，不会双重计数）
pub struct TermUi {
    // TODO: rustyline Editor 句柄
    _priv: (),
}

impl TermUi {
    pub fn new() -> Result<Self, String> {
        // TODO: 检查 stdin 是 TTY（非 TTY → Err，main 映射 exit 2）
        // TODO: rustyline Editor 初始化（含 Ctrl-C/Ctrl-D 行为配置）
        todo!()
    }
}

impl Ui for TermUi {
    fn read_line(&mut self) -> ReadOutcome {
        // TODO: editor.readline() → Ok(line) = Line；
        //       Err(Interrupted) → Interrupted{line_empty: 行内容是否为空}；
        //       Err(Eof) → Eof
        todo!()
    }

    fn confirm(&mut self, cwd: &str, cmd: &str) -> ConfirmOutcome {
        // TODO: 显示会话 cwd 与完整命令，提示 [y/N]（默认 N）
        // TODO: 保持 ISIG 的按键级可中断读取：y/Y → Yes；n/N/回车 → No；
        //       SIGINT 唤醒 → Cancelled；第二次 Ctrl-C 的 quit 由信号计数承接
        //       （返回 Cancelled，调用方收尾时发现 quit 走退出）
        let _ = (cwd, cmd);
        todo!()
    }

    fn print(&mut self, ev: &RenderEvent) {
        // TODO: 按事件类型输出（dim 转义直接写 stdout；行缓冲刷新）
        let _ = ev;
        todo!()
    }
}

/// 测试实现：预排输入 + 预排确认 + 收集渲染事件。
/// 驱动 repl → agent loop → tools 全链路，无网络无 TTY。
pub struct ScriptedUi {
    pub inputs: VecDeque<ReadOutcome>,
    pub confirms: VecDeque<ConfirmOutcome>,
    pub rendered: Vec<RenderEvent>,
}

impl ScriptedUi {
    pub fn new(inputs: Vec<ReadOutcome>, confirms: Vec<ConfirmOutcome>) -> Self {
        Self { inputs: inputs.into(), confirms: confirms.into(), rendered: Vec::new() }
    }
}

impl Ui for ScriptedUi {
    fn read_line(&mut self) -> ReadOutcome {
        self.inputs.pop_front().unwrap_or(ReadOutcome::Eof)
    }

    fn confirm(&mut self, _cwd: &str, _cmd: &str) -> ConfirmOutcome {
        self.confirms.pop_front().unwrap_or(ConfirmOutcome::No)
    }

    fn print(&mut self, ev: &RenderEvent) {
        self.rendered.push(match ev {
            RenderEvent::ReasoningDelta(s) => RenderEvent::ReasoningDelta(s.clone()),
            RenderEvent::TextDelta(s) => RenderEvent::TextDelta(s.clone()),
            RenderEvent::ToolCall(s) => RenderEvent::ToolCall(s.clone()),
            RenderEvent::ToolResult(s) => RenderEvent::ToolResult(s.clone()),
            RenderEvent::Error(s) => RenderEvent::Error(s.clone()),
        });
    }
}
