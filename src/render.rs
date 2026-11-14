//! 渲染：思考暗淡色、正文原样、提示符/输入着色、工具行与结果状态色。颜色固定。
//!
//! `Renderer` 是有状态格式化器：跨事件维护当前样式块，把「进入块」与「离开块」
//! 各自只输出一次 ANSI（思考块一次 `\x1b[2m` … 一次 `\x1b[0m`），不随每个 delta
//! 重复包裹。它只返回要写的文本，不直接写终端；去向由 `ui` 决定。

use crate::tools::ToolStatus;

/// 渲染事件（agent loop → ui.print 的载荷；具体形态实现期定）。
#[derive(Debug)]
pub enum RenderEvent {
    /// 思考内容增量（暗淡色实时输出）
    ReasoningDelta(String),
    /// 正文增量（原样输出，不渲染 markdown）
    TextDelta(String),
    /// 确保行首的折行（回合结束时由 `repl` 补，避免正文块贴住提示符）
    LineBreak,
    /// 工具调用行：`→ <工具名>: <摘要>`
    ToolCall { name: String, summary: String },
    /// 工具结果：display 为 None 时不展示结果体；status 决定样式。
    ToolResult {
        name: String,
        display: Option<String>,
        status: ToolStatus,
    },
    /// 错误展示（轮内错误，回 REPL）
    Error(String),
}

/// ANSI 样式（固定转义，无颜色开关；终态由 Ui/term 决定是否解释）。
const RESET: &str = "\x1b[0m";
const DIM_OPEN: &str = "\x1b[2m";
const PROMPT_OPEN: &str = "\x1b[1;36m";
const INPUT_OPEN: &str = "\x1b[36m";
const TOOL_CALL_OPEN: &str = "\x1b[35m";
const SUCCESS_OPEN: &str = "\x1b[32m";
const FAILURE_OPEN: &str = "\x1b[31m";
const INTERRUPTED_OPEN: &str = "\x1b[33m";

/// ANSI 暗淡色包裹单个片段（`ui/helper.rs` 的灰提示使用）。
pub fn dim(s: &str) -> String {
    format!("{DIM_OPEN}{s}{RESET}")
}

/// 提示符：粗体青色。
pub fn prompt(s: &str) -> String {
    format!("{PROMPT_OPEN}{s}{RESET}")
}

/// 用户输入：青色。
pub fn input(s: &str) -> String {
    format!("{INPUT_OPEN}{s}{RESET}")
}

/// 工具调用单行：`→ <工具名>: <摘要>`，品红色。
pub fn tool_call_line(name: &str, summary: &str) -> String {
    format!("{TOOL_CALL_OPEN}→ {name}: {summary}{RESET}")
}

/// 工具结果带左边栏显示，空白行不补尾随空格；成功绿色、失败红色、打断黄色。
pub fn tool_result_block(content: &str, status: ToolStatus) -> String {
    let body = if content.is_empty() {
        "  │ （无输出）".to_string()
    } else {
        content
            .lines()
            .map(|line| {
                if line.is_empty() {
                    "  │".to_string()
                } else {
                    format!("  │ {line}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    match status {
        ToolStatus::Success => format!("{SUCCESS_OPEN}{body}{RESET}"),
        ToolStatus::Failure => format!("{FAILURE_OPEN}{body}{RESET}"),
        ToolStatus::Interrupted => format!("{INTERRUPTED_OPEN}{body}{RESET}"),
    }
}

/// 一次事件产生的输出，按流向分开：`stdout` 走渲染流，`stderr` 走错误流。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Rendered {
    pub stdout: String,
    pub stderr: String,
}

/// 当前样式块。思考块带暗淡色，需要成对开关；正文无样式，仅用于分隔相邻块。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Block {
    #[default]
    Idle,
    Reasoning,
    Text,
}

/// 有状态渲染器：跨事件维护当前块，样式在进入块时开一次、离开块时关一次。
#[derive(Debug, Default)]
pub struct Renderer {
    block: Block,
}

impl Renderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 消费一个事件，返回该写入终端的文本。
    /// 进入与当前不同的块时先收尾当前块；相邻可流式块之间补折行分隔。
    pub fn event(&mut self, ev: &RenderEvent) -> Rendered {
        let mut out = Rendered::default();
        match ev {
            RenderEvent::ReasoningDelta(s) => {
                match self.block {
                    Block::Idle => out.stdout.push_str(DIM_OPEN),
                    Block::Text => {
                        out.stdout.push('\n');
                        out.stdout.push_str(DIM_OPEN);
                    }
                    Block::Reasoning => {}
                }
                self.block = Block::Reasoning;
                out.stdout.push_str(s);
            }
            RenderEvent::TextDelta(s) => {
                if self.block == Block::Reasoning {
                    out.stdout.push_str(RESET);
                    out.stdout.push('\n');
                }
                self.block = Block::Text;
                out.stdout.push_str(s);
            }
            RenderEvent::LineBreak => {
                out.stdout.push('\n');
            }
            RenderEvent::ToolCall { name, summary } => {
                self.close(&mut out.stdout);
                out.stdout.push('\n');
                out.stdout.push_str(&tool_call_line(name, summary));
                out.stdout.push('\n');
            }
            RenderEvent::ToolResult {
                name: _,
                display,
                status,
            } => {
                self.close(&mut out.stdout);
                if let Some(content) = display {
                    out.stdout.push_str(&tool_result_block(content, *status));
                    out.stdout.push('\n');
                }
            }
            RenderEvent::Error(msg) => {
                self.close(&mut out.stdout);
                out.stderr.push_str("错误：");
                out.stderr.push_str(msg);
                out.stderr.push('\n');
            }
        }
        out
    }

    /// 退出前复位：若思考块仍开着则返回关闭序列，否则返回空串。
    /// 用于把终端交还给提示符之前（`read_line` 开头）以及 Ctrl-C 中途退出等
    /// 没有后续事件的路径，避免暗淡色残留到提示符或退出后的终端。
    pub fn reset(&mut self) -> &'static str {
        if self.block == Block::Reasoning {
            self.block = Block::Idle;
            RESET
        } else {
            ""
        }
    }

    /// 收尾当前块：思考块补关闭序列，其余无输出。块回到 Idle。
    fn close(&mut self, stdout: &mut String) {
        if self.block == Block::Reasoning {
            stdout.push_str(RESET);
        }
        self.block = Block::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dim_wraps_with_ansi() {
        assert_eq!(dim("x"), "\x1b[2mx\x1b[0m");
    }

    #[test]
    fn tool_call_line_format() {
        assert_eq!(tool_call_line("bash", "ls"), "\x1b[35m→ bash: ls\x1b[0m");
    }

    #[test]
    fn prompt_and_input_have_distinct_styles() {
        assert_eq!(prompt("rho> "), "\x1b[1;36mrho> \x1b[0m");
        assert_eq!(input("ls"), "\x1b[36mls\x1b[0m");
    }

    #[test]
    fn result_block_uses_left_rail_and_green_success() {
        assert_eq!(
            tool_result_block("a\nb", ToolStatus::Success),
            "\x1b[32m  │ a\n  │ b\x1b[0m"
        );
    }

    #[test]
    fn result_block_handles_empty_output() {
        assert_eq!(
            tool_result_block("", ToolStatus::Success),
            "\x1b[32m  │ （无输出）\x1b[0m"
        );
    }

    #[test]
    fn result_block_keeps_blank_lines() {
        assert_eq!(
            tool_result_block("a\n\nb", ToolStatus::Success),
            "\x1b[32m  │ a\n  │\n  │ b\x1b[0m"
        );
    }

    #[test]
    fn failure_result_is_red() {
        assert_eq!(
            tool_result_block("x", ToolStatus::Failure),
            "\x1b[31m  │ x\x1b[0m"
        );
    }

    #[test]
    fn interrupted_result_is_yellow() {
        assert_eq!(
            tool_result_block("x", ToolStatus::Interrupted),
            "\x1b[33m  │ x\x1b[0m"
        );
    }

    fn stdout(r: &Rendered) -> &str {
        &r.stdout
    }

    #[test]
    fn reasoning_opens_dim_once_and_block_closes_on_reset() {
        let mut r = Renderer::new();
        assert_eq!(
            stdout(&r.event(&RenderEvent::ReasoningDelta("a".into()))),
            "\x1b[2ma"
        );
        assert_eq!(
            stdout(&r.event(&RenderEvent::ReasoningDelta("b".into()))),
            "b"
        );
        // LineBreak 只折行，不关块；收尾交给 reset。
        assert_eq!(stdout(&r.event(&RenderEvent::LineBreak)), "\n");
        assert_eq!(r.reset(), "\x1b[0m");
    }

    #[test]
    fn reasoning_then_text_closes_dim_and_breaks_line() {
        let mut r = Renderer::new();
        r.event(&RenderEvent::ReasoningDelta("想".into()));
        let out = r.event(&RenderEvent::TextDelta("答".into()));
        assert_eq!(stdout(&out), "\x1b[0m\n答");
    }

    #[test]
    fn plain_text_has_no_decoration() {
        let mut r = Renderer::new();
        assert_eq!(stdout(&r.event(&RenderEvent::TextDelta("hi".into()))), "hi");
    }

    #[test]
    fn tool_call_closes_reasoning_first() {
        let mut r = Renderer::new();
        r.event(&RenderEvent::ReasoningDelta("想".into()));
        let out = r.event(&RenderEvent::ToolCall {
            name: "bash".into(),
            summary: "ls".into(),
        });
        assert_eq!(stdout(&out), "\x1b[0m\n\x1b[35m→ bash: ls\x1b[0m\n");
    }

    #[test]
    fn tool_result_uses_left_rail_and_newline() {
        let mut r = Renderer::new();
        let out = r.event(&RenderEvent::ToolResult {
            name: "bash".into(),
            display: Some("a\nb".into()),
            status: ToolStatus::Success,
        });
        assert_eq!(stdout(&out), "\x1b[32m  │ a\n  │ b\x1b[0m\n");
    }

    #[test]
    fn error_goes_to_stderr_and_closes_reasoning_on_stdout() {
        let mut r = Renderer::new();
        r.event(&RenderEvent::ReasoningDelta("想".into()));
        let out = r.event(&RenderEvent::Error("boom".into()));
        assert_eq!(stdout(&out), "\x1b[0m");
        assert_eq!(out.stderr, "错误：boom\n");
    }

    #[test]
    fn reset_closes_open_reasoning_block() {
        let mut r = Renderer::new();
        r.event(&RenderEvent::ReasoningDelta("想".into()));
        assert_eq!(r.reset(), "\x1b[0m");
        // 复位后块已回 Idle，再复位无输出。
        assert_eq!(r.reset(), "");
    }

    #[test]
    fn reset_is_noop_when_idle() {
        let mut r = Renderer::new();
        r.event(&RenderEvent::TextDelta("hi".into()));
        assert_eq!(r.reset(), "");
    }
}
