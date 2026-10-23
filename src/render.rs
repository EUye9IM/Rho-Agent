//! 渲染：思考暗淡色、正文原样、工具行、截断标注。无颜色开关。

/// 渲染事件（agent loop → ui.print 的载荷；具体形态实现期定）。
#[derive(Debug)]
pub enum RenderEvent {
    /// 思考内容增量（暗淡色实时输出）
    ReasoningDelta(String),
    /// 正文增量（原样输出，不渲染 markdown）
    TextDelta(String),
    /// 思考结束、正文开始之间的折行
    LineBreak,
    /// 工具调用行：`→ bash: <命令>`
    ToolCall(String),
    /// 工具结果（缩进显示，截断处标注省略字节数）
    ToolResult(String),
    /// 错误展示（轮内错误，回 REPL）
    Error(String),
}

/// ANSI 暗淡色包裹（固定转义，无颜色开关；终态由 Ui/term 决定是否解释）。
pub fn dim(s: &str) -> String {
    format!("\x1b[2m{s}\x1b[0m")
}

/// 工具调用单行：`→ bash: <命令>`。
pub fn tool_call_line(cmd: &str) -> String {
    format!("→ bash: {cmd}")
}

/// 工具结果缩进显示。
pub fn tool_result_block(content: &str) -> String {
    if content.is_empty() {
        return "  （无输出）".to_string();
    }
    content
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
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
        assert_eq!(tool_call_line("ls"), "→ bash: ls");
    }

    #[test]
    fn result_block_indents_each_line() {
        assert_eq!(tool_result_block("a\nb"), "  a\n  b");
    }
}
