//! 测试实现：预排输入 + 预排确认 + 收集渲染事件。
//! 驱动 repl → agent loop → tools 全链路，无网络无 TTY。

use crate::render::RenderEvent;
use crate::ui::{ConfirmOutcome, ReadOutcome, Ui};
use std::collections::VecDeque;

pub struct ScriptedUi {
    pub inputs: VecDeque<ReadOutcome>,
    pub confirms: VecDeque<ConfirmOutcome>,
    pub rendered: Vec<RenderEvent>,
    /// 灰提示武装序列（`set_ctrl_c_hint` 的调用值，按顺序）。
    pub hints: Vec<bool>,
}

impl ScriptedUi {
    pub fn new(inputs: Vec<ReadOutcome>, confirms: Vec<ConfirmOutcome>) -> Self {
        Self {
            inputs: inputs.into(),
            confirms: confirms.into(),
            rendered: Vec::new(),
            hints: Vec::new(),
        }
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
            RenderEvent::LineBreak => RenderEvent::LineBreak,
            RenderEvent::ToolCall(s) => RenderEvent::ToolCall(s.clone()),
            RenderEvent::ToolResult(s) => RenderEvent::ToolResult(s.clone()),
            RenderEvent::Error(s) => RenderEvent::Error(s.clone()),
        });
    }

    fn set_ctrl_c_hint(&mut self, armed: bool) {
        self.hints.push(armed);
    }
}
