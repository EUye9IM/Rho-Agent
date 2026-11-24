//! 正文 markdown 渲染：流式输入，按行识别块结构，行内标记交给 pulldown-cmark。
//!
//! 块级识别只覆盖终端里成立的子集：标题、无序/有序列表（按缩进分层）、引用、
//! 逗号分隔的代码块、分隔线。表格、图片、HTML、脚注按原文输出。
//! 流式切分以行为界：完整行立即渲染；未完成的行只有在确认不含行内标记时
//! 才原样输出，其余留到换行或 `flush`。

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const BOLD_ON: &str = "\x1b[1m";
const BOLD_OFF: &str = "\x1b[22m";
const ITALIC_ON: &str = "\x1b[3m";
const ITALIC_OFF: &str = "\x1b[23m";
const STRIKE_ON: &str = "\x1b[9m";
const STRIKE_OFF: &str = "\x1b[29m";
const CODE_ON: &str = "\x1b[36m";
const CODE_OFF: &str = "\x1b[39m";
const HEADING_ON: &str = "\x1b[1;36m";

/// 分隔线的固定宽度（不读终端宽度，避免 render 依赖终端）。
const RULE_WIDTH: usize = 40;
/// 列表每层缩进的空格数。
const LIST_INDENT: usize = 2;
/// 代码块内容的缩进（视觉上区别于正文）。
const CODE_INDENT: &str = "    ";

/// 打开围栏代码块的标记。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fence {
    ch: char,
    len: usize,
}

/// 流式 markdown 渲染器。
#[derive(Debug, Default)]
pub struct Markdown {
    /// 已收到但尚未换行的当前行。
    pending: String,
    /// 代码块状态：Some 表示当前行在围栏内。
    fence: Option<Fence>,
}

impl Markdown {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加流式正文，返回本次可以写终端的文本。
    /// 完整行立即渲染；未完成行只有确认不含行内标记时才原样输出。
    pub fn push(&mut self, text: &str) -> String {
        let mut out = String::new();
        self.pending.push_str(text);
        while let Some(pos) = self.pending.find('\n') {
            let line: String = self.pending[..pos].to_string();
            self.pending.drain(..=pos);
            self.render_line(&line, true, &mut out);
        }
        // 未完成行：把行内标记都已闭合的部分立刻渲染出来，剩下的等换行。
        if !self.pending.is_empty() && !self.in_open_block() {
            let stable = stable_prefix_len(&self.pending);
            if stable > 0 {
                let part: String = self.pending[..stable].to_string();
                // pulldown-cmark 会吃掉段落尾随空白；逐段输出时要原样补回，
                // 否则下一次到达的文本会和这里粘在一起。
                let body = part.trim_end();
                render_inline(body, &mut out);
                out.push_str(&part[body.len()..]);
                self.pending.drain(..stable);
            }
        }
        out
    }

    /// 把剩余内容全部渲染出来（回合结束、切到工具结果、退出前）。
    /// 不补行尾换行：调用方随后会写 `\n` 或状态栏。
    pub fn flush(&mut self) -> String {
        let mut out = String::new();
        if self.pending.is_empty() {
            return out;
        }
        let line = std::mem::take(&mut self.pending);
        self.render_line(&line, false, &mut out);
        out
    }

    /// 是否处于围栏代码块内。
    fn in_open_block(&self) -> bool {
        self.fence.is_some()
    }

    fn render_line(&mut self, line: &str, newline: bool, out: &mut String) {
        // 代码块内：内容原样，不做行内解析。
        if let Some(fence) = self.fence {
            if closes_fence(line, fence) {
                self.fence = None;
                return;
            }
            out.push_str(DIM);
            out.push_str(CODE_INDENT);
            out.push_str(line);
            out.push_str(RESET);
            if newline {
                out.push('\n');
            }
            return;
        }

        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();

        // 引用前缀：剥掉行首的 `>`，剩余部分继续按列表/标题识别。
        let mut rest = trimmed;
        let mut quote = 0;
        while let Some(after) = rest.strip_prefix('>') {
            quote += 1;
            rest = after.strip_prefix(' ').unwrap_or(after);
        }
        let prefix = raw_quote_prefix(quote);

        if rest.trim().is_empty() {
            out.push_str(&prefix);
            if newline {
                out.push('\n');
            }
            return;
        }

        if let Some(fence) = fence_start(rest) {
            self.fence = Some(fence);
            return;
        }

        if is_rule(rest) {
            out.push_str(&prefix);
            out.push_str(DIM);
            out.push_str(&"─".repeat(RULE_WIDTH));
            out.push_str(RESET);
            if newline {
                out.push('\n');
            }
            return;
        }

        if let Some((_, text)) = heading(rest) {
            out.push_str(&prefix);
            out.push_str(HEADING_ON);
            render_inline(text, out);
            out.push_str(RESET);
            if newline {
                out.push('\n');
            }
            return;
        }

        if let Some((marker, text)) = list_marker(rest) {
            let level = indent / LIST_INDENT;
            out.push_str(&prefix);
            for _ in 0..level {
                out.push_str(&" ".repeat(LIST_INDENT));
            }
            out.push_str(marker);
            out.push(' ');
            render_inline(text, out);
            if newline {
                out.push('\n');
            }
            return;
        }

        out.push_str(&prefix);
        render_inline(rest, out);
        if newline {
            out.push('\n');
        }
    }
}

/// 引用前缀：每层一个竖线加空格，暗淡色。
fn raw_quote_prefix(depth: usize) -> String {
    if depth == 0 {
        return String::new();
    }
    format!("{DIM}{}{RESET}", "│ ".repeat(depth))
}

/// 行内标记 → ANSI。完整行调用时标记已确定成对，未闭合的按字面量输出。
fn render_inline(text: &str, out: &mut String) {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    let mut link: Option<String> = None;

    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(Tag::Strong) => out.push_str(BOLD_ON),
            Event::End(TagEnd::Strong) => out.push_str(BOLD_OFF),
            Event::Start(Tag::Emphasis) => out.push_str(ITALIC_ON),
            Event::End(TagEnd::Emphasis) => out.push_str(ITALIC_OFF),
            Event::Start(Tag::Strikethrough) => out.push_str(STRIKE_ON),
            Event::End(TagEnd::Strikethrough) => out.push_str(STRIKE_OFF),
            Event::Start(Tag::Link { dest_url, .. }) => link = Some(dest_url.to_string()),
            Event::End(TagEnd::Link) => {
                if let Some(url) = link.take() {
                    out.push_str(DIM);
                    out.push('(');
                    out.push_str(&url);
                    out.push(')');
                    out.push_str(RESET);
                }
            }
            Event::Code(code) => {
                out.push_str(CODE_ON);
                out.push_str(&code);
                out.push_str(CODE_OFF);
            }
            Event::Text(text) => out.push_str(&text),
            Event::SoftBreak | Event::HardBreak => out.push(' '),
            Event::Html(html) | Event::InlineHtml(html) => out.push_str(&html),
            _ => {}
        }
    }
}

/// 未完成行里可以立即渲染的字节长度。
/// 行首还可能是块标记时返回 0；否则返回第一个未闭合行内标记的位置。
/// 保守估算：宁可多缓冲，也不把半截标记写到终端上。
fn stable_prefix_len(line: &str) -> usize {
    if !head_settled(line) {
        return 0;
    }
    let bytes = line.as_bytes();
    let mut open: Vec<(usize, u8, usize)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'\\' => i = (i + 2).min(bytes.len()),
            b'`' => {
                let n = run_len(bytes, i, b'`');
                match open.last() {
                    Some(&(_, b'`', len)) if len == n => {
                        open.pop();
                    }
                    _ => open.push((i, b'`', n)),
                }
                i += n;
            }
            b'*' | b'_' => {
                let n = run_len(bytes, i, c).min(2);
                match open.iter().rposition(|&(_, ch, len)| ch == c && len == n) {
                    Some(pos) => {
                        open.remove(pos);
                    }
                    None => open.push((i, c, n)),
                }
                i += n;
            }
            b'~' => {
                // 只有双波浪线是删除线。
                if bytes.get(i + 1) == Some(&b'~') {
                    match open.iter().rposition(|&(_, ch, _)| ch == b'~') {
                        Some(pos) => {
                            open.remove(pos);
                        }
                        None => open.push((i, b'~', 2)),
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
            b'[' => {
                open.push((i, b'[', 1));
                i += 1;
            }
            b']' => {
                if let Some(pos) = open.iter().rposition(|&(_, ch, _)| ch == b'[') {
                    open.remove(pos);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    open.first().map(|&(pos, _, _)| pos).unwrap_or(bytes.len())
}

/// 行首是否已经确定不是块标记（标题、列表、引用、围栏、分隔线、表格）。
fn head_settled(line: &str) -> bool {
    let trimmed = line.trim_start();
    let Some(first) = trimmed.chars().next() else {
        return true;
    };
    if first.is_ascii_digit() {
        return false;
    }
    !matches!(first, '#' | '>' | '-' | '+' | '*' | '_' | '`' | '~' | '|')
}

/// 从 `start` 开始连续 `byte` 的个数。
fn run_len(bytes: &[u8], start: usize, byte: u8) -> usize {
    bytes[start..].iter().take_while(|&&c| c == byte).count()
}

/// 标题：`#` 到 `######` 后必须跟空格或行尾。
fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &line[level..];
    if rest.is_empty() {
        return Some((level, ""));
    }
    rest.strip_prefix(' ')
        .map(|text| (level, text.trim_start()))
}

/// 分隔线：去掉空白后是三个及以上同一种 `-` / `*` / `_`。
fn is_rule(line: &str) -> bool {
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() < 3 {
        return false;
    }
    let mut chars = compact.chars();
    let first = chars.next().unwrap();
    matches!(first, '-' | '*' | '_') && chars.all(|c| c == first)
}

/// 列表项：返回（原文标记，内容）。标记保留原文，只规范缩进。
fn list_marker(line: &str) -> Option<(&str, &str)> {
    let mut chars = line.chars();
    match chars.next()? {
        marker @ ('-' | '*' | '+') => {
            let rest = &line[marker.len_utf8()..];
            rest.strip_prefix(' ')
                .map(|text| (&line[..marker.len_utf8()], text))
        }
        c if c.is_ascii_digit() => {
            let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
            let after = &line[digits..];
            let punct = after.chars().next()?;
            if punct != '.' && punct != ')' {
                return None;
            }
            after[punct.len_utf8()..]
                .strip_prefix(' ')
                .map(|text| (&line[..digits + punct.len_utf8()], text))
        }
        _ => None,
    }
}

/// 围栏代码块的开始标记。
fn fence_start(line: &str) -> Option<Fence> {
    for ch in ['`', '~'] {
        let len = line.chars().take_while(|&c| c == ch).count();
        if len >= 3 {
            return Some(Fence { ch, len });
        }
    }
    None
}

/// 关闭围栏：同种字符、长度不短于开始，且除该字符外没有别的内容。
fn closes_fence(line: &str, fence: Fence) -> bool {
    let trimmed = line.trim();
    let len = trimmed.chars().take_while(|&c| c == fence.ch).count();
    len >= fence.len && trimmed.chars().all(|c| c == fence.ch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(text: &str) -> String {
        let mut md = Markdown::new();
        let mut out = md.push(text);
        out.push_str(&md.flush());
        out
    }

    fn render_chunked(parts: &[&str]) -> String {
        let mut md = Markdown::new();
        let mut out = String::new();
        for part in parts {
            out.push_str(&md.push(part));
        }
        out.push_str(&md.flush());
        out
    }

    #[test]
    fn inline_styles_are_wrapped_with_ansi() {
        assert_eq!(render("**加粗**"), "\x1b[1m加粗\x1b[22m");
        assert_eq!(render("*斜体*"), "\x1b[3m斜体\x1b[23m");
        assert_eq!(render("~~删除~~"), "\x1b[9m删除\x1b[29m");
        assert_eq!(render("`code`"), "\x1b[36mcode\x1b[39m");
    }

    #[test]
    fn link_shows_text_then_dimmed_url() {
        assert_eq!(
            render("[文档](https://example.com)"),
            "文档\x1b[2m(https://example.com)\x1b[0m"
        );
    }

    #[test]
    fn heading_drops_hashes_and_colors_the_text() {
        assert_eq!(render("## 标题\n"), "\x1b[1;36m标题\x1b[0m\n");
        // 没有空格的 `#tag` 不是标题。
        assert_eq!(render("#tag\n"), "#tag\n");
    }

    #[test]
    fn lists_keep_the_original_marker_and_indent_by_level() {
        assert_eq!(render("- a\n"), "- a\n");
        assert_eq!(render("1. a\n"), "1. a\n");
        assert_eq!(render("  - nested\n"), "  - nested\n");
        assert_eq!(render("  - deep\n"), "  - deep\n");
        // 缩进按层级重新规范化为两空格一层。
        assert_eq!(render("    - four\n"), "    - four\n");
    }

    #[test]
    fn blockquote_prefixes_each_line() {
        assert_eq!(render("> 引用\n"), format!("{DIM}│ {RESET}引用\n"));
        // 引用内的列表：前缀之后仍按列表渲染。
        assert_eq!(render("> - item\n"), format!("{DIM}│ {RESET}- item\n"));
    }

    #[test]
    fn fenced_code_keeps_inner_markdown_literal() {
        assert_eq!(
            render("```\n**not bold**\n```\n"),
            format!("{DIM}    **not bold**{RESET}\n")
        );
    }

    #[test]
    fn closing_fence_must_match_marker_and_length() {
        let mut md = Markdown::new();
        let first = md.push("````\ncode\n```\nstill code\n````\n");
        // 长度不足的 ``` 不关闭 ````。
        assert_eq!(
            first,
            format!("{DIM}    code{RESET}\n{DIM}    ```{RESET}\n{DIM}    still code{RESET}\n")
        );
    }

    #[test]
    fn rule_becomes_a_fixed_width_line() {
        assert_eq!(
            render("---\n"),
            format!("{DIM}{}{RESET}\n", "─".repeat(RULE_WIDTH))
        );
    }

    #[test]
    fn chunked_and_whole_input_render_the_same() {
        let whole = render("**加粗** 与 `code`\n- 列表\n> 引用\n");
        let chunked = render_chunked(&["**加", "粗** 与 ", "`code`\n- ", "列表\n> 引用\n"]);
        assert_eq!(whole, chunked);
    }

    #[test]
    fn plain_text_streams_before_the_line_ends() {
        let mut md = Markdown::new();
        assert_eq!(md.push("hello "), "hello ");
        assert_eq!(md.push("world"), "world");
        assert_eq!(md.flush(), "");
    }

    #[test]
    fn closed_inline_marks_stream_before_the_line_ends() {
        let mut md = Markdown::new();
        assert_eq!(md.push("这是 **重点**"), "这是 \x1b[1m重点\x1b[22m");
        assert_eq!(md.push("，后面继续"), "，后面继续");
    }

    #[test]
    fn unclosed_mark_buffers_only_its_tail() {
        let mut md = Markdown::new();
        assert_eq!(md.push("前面 **未闭合"), "前面 ");
        assert_eq!(md.push(" 结束**\n"), "\x1b[1m未闭合 结束\x1b[22m\n");
    }

    #[test]
    fn list_lines_wait_for_the_newline() {
        let mut md = Markdown::new();
        assert_eq!(md.push("- 项"), "");
        assert_eq!(md.push("目\n"), "- 项目\n");
    }

    #[test]
    fn marked_text_waits_for_the_newline() {
        let mut md = Markdown::new();
        assert_eq!(md.push("**bo"), "");
        assert_eq!(md.push("ld**"), "");
        assert_eq!(md.push("\n"), "\x1b[1mbold\x1b[22m\n");
    }

    #[test]
    fn flush_renders_the_unfinished_line_without_a_newline() {
        let mut md = Markdown::new();
        assert_eq!(md.push("tail"), "tail");
        assert_eq!(md.flush(), "");

        let mut md = Markdown::new();
        assert_eq!(md.push("**tail**"), "");
        assert_eq!(md.flush(), "\x1b[1mtail\x1b[22m");
    }

    #[test]
    fn table_rows_pass_through_as_text() {
        assert_eq!(render("| a | b |\n"), "| a | b |\n");
    }

    #[test]
    fn escaped_markers_stay_literal() {
        assert_eq!(render("\\*not italic\\*"), "*not italic*");
    }
}
