//! 用户交互抽象：Ui trait + 生产实现 TermUi + 测试实现 ScriptedUi。
//! Ui 不承载退出语义（退出由 REPL 层判定）；灰提示子系统在 `hint` 子模块。
//! 见 docs/architecture.md 三节。

mod hint;
mod scripted;

pub use hint::CTRL_C_WINDOW;
pub use scripted::ScriptedUi;

use crate::render::{self, RenderEvent};
use std::fmt;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::{
    Cmd, ConditionalEventHandler, Event, EventContext, EventHandler, KeyEvent, Modifiers,
    RepeatCount,
};

use hint::{HintArm, HintHelper};

/// 提示符。宽度按字节数算（纯 ASCII）。
const PROMPT: &str = "rho> ";

/// 一行内容（提示符 + 输入）占的屏幕行数。
/// 宽度正好是列数整数倍时，rustyline 的 `calculate_position` 会把位置归一成下一行
/// （`tty/unix.rs` 里 `col == cols` 分支），`refresh` 再补一个 `\n`（“generate our own
/// newline on line wrap”），实际比内容高度多一行 —— 所以这里用 `1 + width / cols`，
/// 不能用向上取整。
/// 宽字符或制表符正好落在换列边界时，实际需要上移的行数会比这里多一：`unicode-width`
/// 不算宽字符不拆列的额外行，也不算 tab 展开到制表位的宽度。这里宁可少算（留残影，
/// 下次按键重绘即消失），也不多算（多上移会擦掉历史输出）。
fn prompt_rows(width: usize, cols: usize) -> usize {
    if cols == 0 {
        return 1;
    }
    1 + width / cols
}

/// 终端列数（取 stdout 窗口尺寸）；拿不到时按 80 列算。
fn terminal_cols() -> usize {
    let mut ws = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: ws 是合法的 winsize 可写缓冲区，fd 是常量 stdout。
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0
    {
        usize::from(ws.ws_col)
    } else {
        80
    }
}

/// 读取一行输入的结果。
#[derive(Debug)]
pub enum ReadOutcome {
    /// 普通输入或斜杠命令（Ui 不识别命令）
    Line(String),
    /// Ctrl-C：编辑中（false）清空输入；空行（true）进入双按窗口
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

    /// Ctrl-C 双按提示开关：`true` 在提示符行尾显示灰色提示，`false` 立刻撤掉。
    /// 窗口到期由 Ui 自行撤掉；退出判定归 REPL 层。
    /// 默认实现无动作（测试 Ui 不必关心）。
    fn set_ctrl_c_hint(&mut self, _armed: bool) {}

    /// 渲染/读取 I/O 失败：REPL 据此映射 exit 1。
    /// 默认实现无错误（测试 Ui 不必关心）。
    fn take_fatal(&mut self) -> Option<String> {
        None
    }
}

/// TermUi 初始化错误：非 TTY 属用法错误（exit 2），其余属内部错误（exit 1）。
#[derive(Debug)]
pub enum TermUiInitError {
    NotTty(String),
    Init(String),
}

impl fmt::Display for TermUiInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotTty(msg) | Self::Init(msg) => f.write_str(msg),
        }
    }
}

/// Ctrl-C 按键处理器：记录被中断时输入行是否为空与其显示宽度（用于回收屏幕行），
/// 再执行默认 Interrupt。
struct CtrlCHandler {
    line_empty: Arc<AtomicBool>,
    line_width: Arc<AtomicUsize>,
}

impl ConditionalEventHandler for CtrlCHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext,
    ) -> Option<Cmd> {
        let line = ctx.line();
        self.line_empty.store(line.is_empty(), Ordering::SeqCst);
        self.line_width.store(line.width(), Ordering::SeqCst);
        Some(Cmd::Interrupt)
    }
}

/// 生产实现：rustyline 后端。
///
/// - `stdin 非 TTY 拒绝启动`的检查在 `TermUi::new` 里做
/// - `confirm` 保持 ISIG 的可中断读取：Ctrl-C 产生 SIGINT 唤醒阻塞的
///   `libc::read` 返回 `EINTR`，转为 `Cancelled`（记录由专用信号线程追加，
///   此处不自行追加，无双重计数）
/// - REPL 输入态 Ctrl-C 由 rustyline 返回 Interrupted（其原始模式清 ISIG，
///   与 confirm 的信号路径互斥，不会双重计数）
/// - 空行 Ctrl-C 的灰色双按提示由 helper 提供，到期由 `rho-hint` 线程擦掉
/// - 每次 `readline` 收尾都会被 rustyline 的换行钉死一行（输入长时多行）：Ctrl-C 后
///   下一次读取前把这些行收回重用，输入过的内容也一并清掉，不另起新行
pub struct TermUi {
    editor: Editor,
    line_empty: Arc<AtomicBool>,
    line_width: Arc<AtomicUsize>,
    hint: Arc<HintArm>,
    fatal: Option<String>,
    /// 上一次 `readline` 被 Ctrl-C 中断时那一块内容的显示宽度：下一次读取前收回。
    reclaim_width: Option<usize>,
}

type Editor = rustyline::Editor<HintHelper, DefaultHistory>;

impl TermUi {
    pub fn new() -> Result<Self, TermUiInitError> {
        if !std::io::stdin().is_terminal() {
            return Err(TermUiInitError::NotTty(
                "stdin 不是 TTY，rho 只支持交互式 REPL".into(),
            ));
        }
        let mut editor: Editor =
            rustyline::Editor::new().map_err(|e| TermUiInitError::Init(format!("初始化终端失败：{e}")))?;
        let line_empty = Arc::new(AtomicBool::new(false));
        let line_width = Arc::new(AtomicUsize::new(0));
        let hint = Arc::new(HintArm::new(std::io::stdout().is_terminal()));
        editor.set_helper(Some(HintHelper::new(hint.clone())));
        editor.bind_sequence(
            KeyEvent::new('\x03', Modifiers::NONE),
            EventHandler::Conditional(Box::new(CtrlCHandler {
                line_empty: line_empty.clone(),
                line_width: line_width.clone(),
            })),
        );
        let thread_hint = hint.clone();
        std::thread::Builder::new()
            .name("rho-hint".into())
            .spawn(move || hint::hint_timer(thread_hint))
            .map_err(|e| TermUiInitError::Init(format!("启动提示期限线程失败：{e}")))?;
        Ok(Self {
            editor,
            line_empty,
            line_width,
            hint,
            fatal: None,
            reclaim_width: None,
        })
    }

    /// rustyline 每次 `readline` 结束都会补一个换行（`Editor::readline_with` 里的
    /// `term.writeln()`）。被 Ctrl-C 中断的输入不提交，那几行屏幕空间要还给下一次
    /// 提示符：上移回块首并用 `\x1b[J` 擦到屏幕末尾，下一次 `readline` 就在原地重画。
    /// 块宽度包含输入内容与（若最后一次布局里有）行尾灰提示的宽度。
    ///
    /// 不变式：提示符块永远是最后一段终端输出，所以 `\x1b[J` 不会误删其他输出；
    /// 块高于屏幕时视口里全是块自己的行，此时 `\x1b[{rows}A` 被终端钳制到顶行，
    /// 擦掉的也仍然只是块内容。
    /// stdout 不是 TTY 时不动光标，以免污染重定向输出。
    /// 前提：中断到真回收之间不能有其他输出（`repl` 在 `Interrupted` 分支不打印）。
    fn reclaim_prompt_rows(&mut self) {
        let Some(width) = self.reclaim_width.take() else {
            return;
        };
        if !std::io::stdout().is_terminal() {
            self.hint.take_layout_hint();
            return;
        }
        let hint_width = if self.hint.take_layout_hint() {
            hint::CTRL_C_HINT.width()
        } else {
            0
        };
        let rows = prompt_rows(width + hint_width, terminal_cols());
        let mut out = std::io::stdout();
        let seq = format!("\x1b[{rows}A\r\x1b[J");
        if let Err(e) = out.write_all(seq.as_bytes()).and_then(|()| out.flush()) {
            self.set_fatal(format!("渲染输出失败：{e}"));
        }
    }

    fn set_fatal(&mut self, msg: impl Into<String>) {
        if self.fatal.is_none() {
            self.fatal = Some(msg.into());
        }
    }
}

impl Ui for TermUi {
    fn read_line(&mut self) -> ReadOutcome {
        self.reclaim_prompt_rows();
        self.hint.set_reading(true);
        let outcome = match self.editor.readline(PROMPT) {
            Ok(line) => ReadOutcome::Line(line),
            Err(ReadlineError::Interrupted) => {
                let line_empty = self.line_empty.load(Ordering::SeqCst);
                let width = self.line_width.load(Ordering::SeqCst);
                // 被中断的内容不提交：整块屏幕行留给下一次提示符。
                self.reclaim_width = Some(PROMPT.len() + width);
                ReadOutcome::Interrupted { line_empty }
            }
            Err(ReadlineError::Eof) => ReadOutcome::Eof,
            Err(e) => {
                self.set_fatal(format!("读取输入失败：{e}"));
                ReadOutcome::Eof
            }
        };
        self.hint.set_reading(false);
        outcome
    }

    fn confirm(&mut self, cwd: &str, cmd: &str) -> ConfirmOutcome {
        let result = (|| -> std::io::Result<()> {
            let mut out = std::io::stdout().lock();
            write!(
                out,
                "\n当前目录：{cwd}\n将执行 bash：\n  {cmd}\n执行？[y/N] "
            )?;
            out.flush()
        })();
        if let Err(e) = result {
            self.set_fatal(format!("渲染输出失败：{e}"));
            return ConfirmOutcome::No;
        }
        match read_confirm_line() {
            Ok(Some(line)) => match line.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" => ConfirmOutcome::Yes,
                _ => ConfirmOutcome::No,
            },
            Ok(None) => ConfirmOutcome::No,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => ConfirmOutcome::Cancelled,
            Err(e) => {
                self.set_fatal(format!("读取确认输入失败：{e}"));
                ConfirmOutcome::No
            }
        }
    }

    fn print(&mut self, ev: &RenderEvent) {
        let result = (|| -> std::io::Result<()> {
            let mut out = std::io::stdout().lock();
            match ev {
                RenderEvent::ReasoningDelta(s) => write!(out, "{}", render::dim(s))?,
                RenderEvent::TextDelta(s) => write!(out, "{s}")?,
                RenderEvent::LineBreak => writeln!(out)?,
                RenderEvent::ToolCall(cmd) => {
                    write!(out, "\n{}\n", render::tool_call_line(cmd))?
                }
                RenderEvent::ToolResult(content) => {
                    writeln!(out, "{}", render::tool_result_block(content))?
                }
                RenderEvent::Error(msg) => {
                    out.flush()?;
                    // 错误走 stderr（与渲染流分离）；写失败与渲染同路径进 fatal，
                    // 不用可能 panic 的 eprintln!。
                    let mut err = std::io::stderr().lock();
                    writeln!(err, "错误：{msg}")?;
                    err.flush()?;
                }
            }
            out.flush()
        })();
        if let Err(e) = result {
            self.set_fatal(format!("渲染输出失败：{e}"));
        }
    }

    fn set_ctrl_c_hint(&mut self, armed: bool) {
        if armed {
            self.hint.arm(CTRL_C_WINDOW);
        } else {
            self.hint.disarm();
        }
    }

    fn take_fatal(&mut self) -> Option<String> {
        self.fatal.take()
    }
}

impl Drop for TermUi {
    fn drop(&mut self) {
        // 双按退出时最后一段是被放弃的提示符块，退出前收回。
        // 前提：中断到 drop 之间没有其他输出，否则上移落点会比块首低一行，
        // 既留下块首行、又擦掉中间那段输出。
        self.reclaim_prompt_rows();
        self.hint.stop();
    }
}

/// 直接读 stdin fd（不经 std 的 BufReader，避免吞掉后续输入）。
/// 返回 Ok(None) 表示 EOF；Err(Interrupted) 表示被 SIGINT 打断。
fn read_confirm_line() -> std::io::Result<Option<String>> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        // SAFETY: fd 0 是 stdin；byte 是合法的 1 字节可写缓冲区。
        let n = unsafe { libc::read(0, byte.as_mut_ptr().cast(), 1) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if n == 0 {
            if bytes.is_empty() {
                return Ok(None);
            }
            break;
        }
        match byte[0] {
            b'\n' => break,
            b'\r' => {}
            b => bytes.push(b),
        }
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_rows_counts_wrapped_screen_rows() {
        // 这里是“回收时要上移几行”，等价于 rustyline 的 `layout.end.row + 1`，
        // 不是内容本身的高度（整倍数时多一行）。
        assert_eq!(prompt_rows(0, 80), 1);
        assert_eq!(prompt_rows(5, 80), 1);
        assert_eq!(prompt_rows(79, 80), 1);
        // 整倍数：rustyline 自己补了一个换行，比内容高度多一行。
        assert_eq!(prompt_rows(80, 80), 2);
        assert_eq!(prompt_rows(81, 80), 2);
        assert_eq!(prompt_rows(205, 80), 3);
        // 拿不到列数时退化为不换行，且不会除零。
        assert_eq!(prompt_rows(200, 0), 1);
    }

    #[test]
    fn prompt_rows_counts_hint_on_narrow_terminals() {
        let with_hint = PROMPT.len() + hint::CTRL_C_HINT.width();
        // 80 列下多出来的灰提示仍在一行内。
        assert_eq!(prompt_rows(with_hint, 80), 1);
        // 窄于提示 + 提示符总宽时占两行：回收必须上移两行，否则会留残影。
        assert_eq!(prompt_rows(with_hint, with_hint - 1), 2);
    }
}
