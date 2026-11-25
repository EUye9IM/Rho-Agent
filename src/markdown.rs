//! 正文 markdown 渲染：流式输入，按行识别块结构，行内标记交给 pulldown-cmark。
//!
//! 块级识别只覆盖终端里成立的子集：标题、无序/有序列表（按缩进分层）、引用、
//! 逗号分隔的代码块、分隔线。表格、图片、HTML、脚注按原文输出。
//! 流式切分以行为界：完整行立即渲染；未完成的行只有在确认不含行内标记时
//! 才原样输出，其余留到换行或 `flush`。

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use unicode_width::UnicodeWidthStr;

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

/// 表格收集状态。表格必须整块对齐，因此先缓冲再一次性输出。
#[derive(Debug)]
enum TableState {
    /// 首行以 `|` 开头，等待下一行确认是否为分隔行。
    Candidate(String),
    /// 已确认是表格：表头、分隔行与后续数据行。
    Collecting(Vec<String>),
}

/// 单元格对齐方式（来自分隔行的冒号位置）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Center,
    Right,
}

/// 流式 markdown 渲染器。
#[derive(Debug)]
pub struct Markdown {
    /// 关闭样式后要重新应用的基础样式（思考块的暗淡色）；正文为空串。
    base: &'static str,
    /// 已收到但尚未换行的当前行。
    pending: String,
    /// 代码块状态：Some 表示当前行在围栏内。
    fence: Option<Fence>,
    /// 表格缓冲。
    table: Option<TableState>,
    /// 块级公式：`$$ ... $$` 跨行时置位。
    display_math: bool,
    /// 当前行已经有内容写出：后续片段按续行处理，不再 trim 行首。
    line_started: bool,
}

impl Default for Markdown {
    fn default() -> Self {
        Self::with_base("")
    }
}

impl Markdown {
    pub fn new() -> Self {
        Self::default()
    }

    /// 带基础样式：每次关闭一段样式后重新应用它。
    /// 用于叠加暗淡色——SGR 22 会同时关闭 bold 与 dim。
    pub fn with_base(base: &'static str) -> Self {
        Self {
            base,
            pending: String::new(),
            fence: None,
            table: None,
            display_math: false,
            line_started: false,
        }
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
            self.line_started = false;
        }
        // 未完成行：把行内标记都已闭合的部分立刻渲染出来，剩下的等换行。
        if !self.pending.is_empty() && !self.in_open_block() && self.table.is_none() {
            let stable = stable_prefix_len(&self.pending);
            if stable > 0 {
                let part: String = self.pending[..stable].to_string();
                // pulldown-cmark 会吃掉段落首尾空白；逐段输出时要原样补回，
                // 否则相邻 delta 会粘在一起（"Hello" + " world"）。
                render_plain(&part, self.base, &mut out);
                self.pending.drain(..stable);
                self.line_started = true;
            }
        }
        out
    }

    /// 把剩余内容全部渲染出来（回合结束、切到工具结果、退出前）。
    /// 不补行尾换行：调用方随后会写 `\n` 或状态栏。
    pub fn flush(&mut self) -> String {
        let mut out = String::new();
        // 未完成的行先按正常流程走一遍：它可能是表格的分隔行或最后一行数据。
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.render_line(&line, false, &mut out);
        }
        // 表格可能仍在收集：整块输出。
        match self.table.take() {
            Some(TableState::Candidate(first)) => {
                render_inline(first.trim_start(), self.base, &mut out)
            }
            Some(TableState::Collecting(rows)) => render_table(&rows, self.base, &mut out),
            None => {}
        }
        self.line_started = false;
        out
    }

    /// 是否处于围栏代码块内。
    fn in_open_block(&self) -> bool {
        self.fence.is_some()
    }

    fn render_line(&mut self, line: &str, newline: bool, out: &mut String) {
        // 续行：这一行前半截已经写过，只能是行内内容的延续。
        if std::mem::take(&mut self.line_started) {
            render_plain(line, self.base, out);
            if newline {
                out.push('\n');
            }
            return;
        }

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
            out.push_str(self.base);
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
        let prefix = raw_quote_prefix(quote, self.base);

        // 块级公式：`$$ ... $$` 整块缩进，内容不再做行内解析。
        if self.display_math {
            if rest.trim() == "$$" {
                self.display_math = false;
                return;
            }
            render_math_line(rest, &prefix, self.base, newline, out);
            return;
        }
        if let Some(body) = single_display_math(rest) {
            render_math_line(body, &prefix, self.base, newline, out);
            return;
        }
        if rest.trim_start().starts_with("$$") {
            self.display_math = true;
            let body = rest.trim_start().trim_start_matches("$$");
            if !body.is_empty() {
                render_math_line(body, &prefix, self.base, newline, out);
            }
            return;
        }

        // 表格收集：整块对齐后才能输出，先缓冲。行内容已剥掉引用前缀。
        match self.table.take() {
            Some(TableState::Candidate(first)) => {
                if is_table_delimiter(rest) {
                    self.table = Some(TableState::Collecting(vec![first, rest.to_string()]));
                } else {
                    // 不是表格：首行按段落渲染，当前行按正常流程继续。
                    render_inline(first.trim_start(), self.base, out);
                    if newline {
                        out.push('\n');
                    }
                    self.render_line(line, newline, out);
                }
                return;
            }
            Some(TableState::Collecting(mut rows)) => {
                if is_table_row(rest) {
                    rows.push(rest.to_string());
                    self.table = Some(TableState::Collecting(rows));
                } else {
                    render_table(&rows, self.base, out);
                    self.render_line(line, newline, out);
                }
                return;
            }
            None => {}
        }

        if rest.trim().is_empty() {
            out.push_str(&prefix);
            if newline {
                out.push('\n');
            }
            return;
        }

        // 以 `|` 开头的行先记为表格候选；下一行是分隔行才确认。
        if is_table_row(rest) {
            self.table = Some(TableState::Candidate(rest.to_string()));
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
            out.push_str(self.base);
            if newline {
                out.push('\n');
            }
            return;
        }

        if let Some((level, text)) = heading(rest) {
            out.push_str(&prefix);
            // 保留原始 `#` 数量：纯靠颜色分不出六级标题。
            out.push_str(&"#".repeat(level));
            out.push(' ');
            out.push_str(HEADING_ON);
            render_inline(text, self.base, out);
            out.push_str(RESET);
            out.push_str(self.base);
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
            render_inline(text, self.base, out);
            if newline {
                out.push('\n');
            }
            return;
        }

        out.push_str(&prefix);
        render_inline(rest, self.base, out);
        if newline {
            out.push('\n');
        }
    }
}

/// 引用前缀：每层一个竖线加空格，暗淡色。
fn raw_quote_prefix(depth: usize, base: &str) -> String {
    if depth == 0 {
        return String::new();
    }
    format!("{DIM}{}{RESET}{base}", "│ ".repeat(depth))
}

/// 行内渲染并保留首尾空白：pulldown-cmark 会把段落两端的空白当作缩进吃掉，
/// 而流式片段之间的空白正是相邻单词的分隔符。
fn render_plain(text: &str, base: &str, out: &mut String) {
    if text.trim().is_empty() {
        out.push_str(text);
        return;
    }
    let start = text.len() - text.trim_start().len();
    let end = text.trim_end().len();
    out.push_str(&text[..start]);
    render_inline(&text[start..end], base, out);
    out.push_str(&text[end..]);
}

/// 关闭一段样式后重新应用基础样式。
fn close_style(out: &mut String, sequence: &str, base: &str) {
    out.push_str(sequence);
    out.push_str(base);
}

/// 行内标记 → ANSI。完整行调用时标记已确定成对，未闭合的按字面量输出。
fn render_inline(text: &str, base: &str, out: &mut String) {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_MATH);
    let mut link: Option<String> = None;

    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(Tag::Strong) => out.push_str(BOLD_ON),
            Event::End(TagEnd::Strong) => close_style(out, BOLD_OFF, base),
            Event::Start(Tag::Emphasis) => out.push_str(ITALIC_ON),
            Event::End(TagEnd::Emphasis) => close_style(out, ITALIC_OFF, base),
            Event::Start(Tag::Strikethrough) => out.push_str(STRIKE_ON),
            Event::End(TagEnd::Strikethrough) => close_style(out, STRIKE_OFF, base),
            Event::Start(Tag::Link { dest_url, .. }) => link = Some(dest_url.to_string()),
            Event::End(TagEnd::Link) => {
                if let Some(url) = link.take() {
                    out.push_str(DIM);
                    out.push('(');
                    out.push_str(&url);
                    out.push(')');
                    close_style(out, RESET, base);
                }
            }
            Event::Code(code) => {
                out.push_str(CODE_ON);
                out.push_str(&code);
                close_style(out, CODE_OFF, base);
            }
            Event::Text(text) => out.push_str(&text),
            // 公式只去掉定界符：\frac、^、_ 这类仍是字面量，终端无法排版。
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                out.push_str(ITALIC_ON);
                out.push_str(DIM);
                out.push_str(&math);
                close_style(out, RESET, base);
            }
            Event::SoftBreak | Event::HardBreak => out.push(' '),
            Event::Html(html) | Event::InlineHtml(html) => out.push_str(&html),
            _ => {}
        }
    }
}

/// 单行块级公式：`$$x$$` 返回中间的内容。
fn single_display_math(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.len() <= 4 {
        return None;
    }
    trimmed
        .strip_prefix("$$")?
        .strip_suffix("$$")
        .map(str::trim)
}

/// 块级公式的一行：四空格缩进 + 暗淡斜体。
fn render_math_line(text: &str, prefix: &str, base: &str, newline: bool, out: &mut String) {
    out.push_str(prefix);
    out.push_str(CODE_INDENT);
    out.push_str(ITALIC_ON);
    out.push_str(DIM);
    out.push_str(text);
    close_style(out, RESET, base);
    if newline {
        out.push('\n');
    }
}

/// 表格行：以 `|` 开头。
fn is_table_row(line: &str) -> bool {
    line.trim_start().starts_with('|')
}

/// 分隔行：`|---|:--:|` 这类，每个单元格是可选冒号包裹的短横线。
fn is_table_delimiter(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') {
        return false;
    }
    let cells: Vec<&str> = trimmed
        .split('|')
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .collect();
    !cells.is_empty() && cells.iter().all(|cell| is_delim_cell(cell))
}

fn is_delim_cell(cell: &str) -> bool {
    let inner = cell.strip_prefix(':').unwrap_or(cell);
    let inner = inner.strip_suffix(':').unwrap_or(inner);
    !inner.is_empty() && inner.chars().all(|c| c == '-')
}

/// 按 `|` 切单元格，去掉首尾的边框竖线。
fn split_cells(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let body = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let body = body.strip_suffix('|').unwrap_or(body);
    body.split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

/// 单元格的显示宽度：按行内渲染后的纯文本算，忽略标记与 ANSI。
fn plain_width(text: &str) -> usize {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_MATH);
    let mut width = 0;
    for event in Parser::new_ext(text, options) {
        match event {
            Event::Text(text) | Event::Code(text) => width += UnicodeWidthStr::width(text.as_ref()),
            _ => {}
        }
    }
    width
}

/// 分隔行的冒号位置决定每列对齐。
fn parse_aligns(cells: &[String]) -> Vec<Align> {
    cells
        .iter()
        .map(|cell| match (cell.starts_with(':'), cell.ends_with(':')) {
            (true, true) => Align::Center,
            (false, true) => Align::Right,
            _ => Align::Left,
        })
        .collect()
}

/// 整块渲染表格：列宽按单元格纯文本的最大显示宽度，CJK 按双宽计算。
fn render_table(rows: &[String], base: &str, out: &mut String) {
    let parsed: Vec<Vec<String>> = rows.iter().map(|row| split_cells(row)).collect();
    let header = parsed.first().cloned().unwrap_or_default();
    let aligns = parsed
        .get(1)
        .map(|row| parse_aligns(row))
        .unwrap_or_default();
    let body = parsed.get(2..).unwrap_or_default();
    let cols = parsed.iter().map(Vec::len).max().unwrap_or(0);
    if cols == 0 {
        return;
    }

    let mut widths = vec![0usize; cols];
    for (index, row) in parsed.iter().enumerate() {
        // 分隔行只是排版语法，不参与列宽。
        if index == 1 {
            continue;
        }
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(plain_width(cell));
        }
    }

    write_row(out, &header, &widths, &aligns, true, base);

    out.push_str(DIM);
    out.push('├');
    for (i, width) in widths.iter().enumerate() {
        if i > 0 {
            out.push('┼');
        }
        out.push_str(&"─".repeat(width + 2));
    }
    out.push('┤');
    close_style(out, RESET, base);
    out.push('\n');

    for row in body {
        write_row(out, row, &widths, &aligns, false, base);
    }
}

fn write_row(
    out: &mut String,
    row: &[String],
    widths: &[usize],
    aligns: &[Align],
    header: bool,
    base: &str,
) {
    out.push_str(DIM);
    out.push('│');
    close_style(out, RESET, base);
    for (i, &width) in widths.iter().enumerate() {
        let cell = row.get(i).map(String::as_str).unwrap_or("");
        let pad = width.saturating_sub(plain_width(cell));
        let (left, right) = match aligns.get(i).copied().unwrap_or(Align::Left) {
            Align::Left => (0, pad),
            Align::Center => (pad / 2, pad - pad / 2),
            Align::Right => (pad, 0),
        };
        out.push(' ');
        out.push_str(&" ".repeat(left));
        if header {
            out.push_str(HEADING_ON);
        }
        render_inline(cell, base, out);
        if header {
            close_style(out, RESET, base);
        }
        out.push_str(&" ".repeat(right));
        out.push(' ');
        out.push_str(DIM);
        out.push('│');
        close_style(out, RESET, base);
    }
    out.push('\n');
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
            b'$' => {
                // 行内与块级公式分别按定界符长度配对。
                let n = run_len(bytes, i, b'$').min(2);
                match open
                    .iter()
                    .rposition(|&(_, ch, len)| ch == b'$' && len == n)
                {
                    Some(pos) => {
                        open.remove(pos);
                    }
                    None => open.push((i, b'$', n)),
                }
                i += n;
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

/// 行首是否已经确定不是块标记（标题、列表、引用、围栏、分隔线、表格、公式）。
/// 只对确定的分支返回 false：`**加粗**`、`#tag`、`--flag` 都按段落处理。
fn head_settled(line: &str) -> bool {
    let trimmed = line.trim_start();
    let Some(first) = trimmed.chars().next() else {
        return true;
    };

    if trimmed.starts_with('>') || trimmed.starts_with('|') {
        return false;
    }
    if trimmed.starts_with("```") || trimmed.starts_with("~~~") || trimmed.starts_with("$$") {
        return false;
    }

    // 标题：`#`×n 后必须是空格。
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
        return false;
    }

    // 无序列表：标记后必须是空格。
    if trimmed.starts_with("- ") || trimmed.starts_with("+ ") || trimmed.starts_with("* ") {
        return false;
    }

    // 分隔线：三个及以上同一种 `-` / `*` / `_`，且后面没有别的内容。
    if matches!(first, '-' | '*' | '_') {
        let run = trimmed.chars().take_while(|&c| c == first).count();
        if run >= 3 && trimmed[run..].trim().is_empty() {
            return false;
        }
    }

    // 有序列表：数字后必须是 `. ` 或 `) `。
    let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        let after = &trimmed[digits..];
        if after.starts_with(". ") || after.starts_with(") ") {
            return false;
        }
    }

    true
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
    fn heading_keeps_hashes_and_colors_the_text() {
        assert_eq!(render("## 标题\n"), "## \x1b[1;36m标题\x1b[0m\n");
        assert_eq!(render("# 一级\n"), "# \x1b[1;36m一级\x1b[0m\n");
        // 没有空格的 `#tag` 不是标题。
        assert_eq!(render("#tag\n"), "#tag\n");
    }

    #[test]
    fn inline_math_drops_delimiters_and_is_dim_italic() {
        assert_eq!(
            render("$E = mc^2$\n"),
            format!("{ITALIC_ON}{DIM}E = mc^2{RESET}\n")
        );
    }

    #[test]
    fn single_line_display_math_is_indented() {
        assert_eq!(
            render("$$x + y$$\n"),
            format!("{CODE_INDENT}{ITALIC_ON}{DIM}x + y{RESET}\n")
        );
    }

    #[test]
    fn multi_line_display_math_is_indented_per_line() {
        let out = render("$$\na = b\nc = d\n$$\n");
        let line = format!("{CODE_INDENT}{ITALIC_ON}{DIM}");
        assert_eq!(out.matches(&line).count(), 2, "out was {out:?}");
        assert!(!out.contains("$$"), "out was {out:?}");
    }

    #[test]
    fn display_math_does_not_parse_inline_markup() {
        let out = render("$$\n**raw**\n$$\n");
        assert!(out.contains("**raw**"), "out was {out:?}");
        assert!(!out.contains("\x1b[1m"), "out was {out:?}");
    }

    #[test]
    fn unclosed_dollar_buffers_until_the_line_ends() {
        let mut md = Markdown::new();
        // `$` 未闭合期间只输出它之前的前缀。
        assert_eq!(md.push("花费 $5"), "花费 ");
        assert_eq!(md.push(" 再算\n"), "$5 再算\n");
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
    fn leading_space_of_a_chunk_is_preserved() {
        let mut md = Markdown::new();
        let mut out = String::new();
        out.push_str(&md.push("Hello"));
        out.push_str(&md.push(" world"));
        out.push_str(&md.push(", again"));
        out.push_str(&md.flush());
        assert_eq!(out, "Hello world, again");
    }

    #[test]
    fn whitespace_only_chunks_pass_through() {
        let mut md = Markdown::new();
        assert_eq!(md.push("a"), "a");
        assert_eq!(md.push(" "), " ");
        assert_eq!(md.push(" "), " ");
        assert_eq!(md.push("b\n"), "b\n");
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
        // 标记一闭合就立即输出，不必等换行。
        assert_eq!(md.push("ld**"), "\x1b[1mbold\x1b[22m");
        assert_eq!(md.push("\n"), "\n");
    }

    #[test]
    fn flush_renders_the_unfinished_line_without_a_newline() {
        let mut md = Markdown::new();
        assert_eq!(md.push("tail"), "tail");
        assert_eq!(md.flush(), "");

        let mut md = Markdown::new();
        assert_eq!(md.push("**tail"), "");
        assert_eq!(md.flush(), "**tail");
    }

    #[test]
    fn table_rows_without_a_delimiter_stay_text() {
        // 没有分隔行确认：按普通段落输出。
        assert_eq!(render("| a | b |\n"), "| a | b |");
        // 不以 `|` 开头的行永远不是表格。
        assert_eq!(render("a | b\n"), "a | b\n");
    }

    #[test]
    fn table_aligns_columns_and_keeps_cjk_width() {
        let out = render("| 名称 | 大小 |\n|------|-----:|\n| rho | 1.9M |\n");
        let expected = format!(
            "{DIM}│{RESET} {HEADING_ON}名称{RESET} {DIM}│{RESET} {HEADING_ON}大小{RESET} {DIM}│{RESET}\n\
             {DIM}├──────┼──────┤{RESET}\n\
             {DIM}│{RESET} rho  {DIM}│{RESET} 1.9M {DIM}│{RESET}\n"
        );
        assert_eq!(out, expected);
    }

    #[test]
    fn table_centers_with_double_colons() {
        let out = render("| a |\n|:-:|\n| x |\n");
        assert!(
            out.contains(&format!("{DIM}│{RESET} x {DIM}│{RESET}")),
            "out was {out:?}"
        );
        // 分隔行不参与列宽：单字符列宽为 1。
        assert!(out.contains("├───┤"), "out was {out:?}");
    }

    #[test]
    fn table_cells_render_inline_markdown() {
        let out = render("| a |\n|---|\n| **b** |\n");
        assert!(out.contains("\x1b[1mb\x1b[22m"), "out was {out:?}");
    }

    #[test]
    fn table_ends_at_the_first_non_table_line() {
        let out = render("| a |\n|---|\n| 1 |\n结尾\n");
        assert!(out.contains("├───┤"), "out was {out:?}");
        assert!(out.ends_with("结尾\n"), "out was {out:?}");
    }

    #[test]
    fn table_flushes_when_the_stream_ends() {
        let mut md = Markdown::new();
        assert_eq!(md.push("| a | b |\n|---|"), "");
        let out = md.flush();
        assert!(out.contains("├───┼───┤"), "out was {out:?}");
    }

    #[test]
    fn table_survives_split_deltas() {
        let whole = render("| a | b |\n|---|---|\n| 1 | 2 |\n");
        let chunked = render_chunked(&["| a |", " b |\n|-", "--|---|\n| 1 | 2 |\n"]);
        assert_eq!(whole, chunked);
    }

    #[test]
    fn base_style_is_reapplied_after_each_close() {
        // SGR 22 会同时关闭 bold 与 dim，所以基础样式要补回来。
        let mut md = Markdown::with_base("\x1b[2m");
        assert_eq!(md.push("**x**\n"), "\x1b[1mx\x1b[22m\x1b[2m\n");

        let mut md = Markdown::with_base("\x1b[2m");
        assert_eq!(md.push("- x\n"), "- x\n");
    }

    #[test]
    fn escaped_markers_stay_literal() {
        assert_eq!(render("\\*not italic\\*"), "*not italic*");
    }
}
