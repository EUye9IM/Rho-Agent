//! 用户交互抽象：Ui trait + 生产实现 TermUi + 测试实现 ScriptedUi。
//! Ui 不承载退出语义（退出由 REPL 层判定）。见 docs/architecture.md 三节。

use crate::render::{self, RenderEvent};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

use rustyline::completion::Completer;
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{
    Cmd, ConditionalEventHandler, Context, Editor, Event, EventContext, EventHandler, Helper,
    KeyEvent, Modifiers, RepeatCount,
};

/// 提示符下 Ctrl-C 双按窗口：两次空行 Ctrl-C 间隔小于它才退出。
/// `repl` 用同一个常量判定退出，`TermUi` 用它到期撤掉灰提示。
pub const CTRL_C_WINDOW: Duration = Duration::from_secs(2);

/// 第一次空行 Ctrl-C 后在提示符行尾显示的灰色提示。
const CTRL_C_HINT: &str = "再按一次 Ctrl-C 退出";

/// 提示符。宽度按字节数算（纯 ASCII）。
const PROMPT: &str = "rho> ";

/// 一行内容（提示符 + 输入）在 `cols` 列终端上占的行数。
/// 刚好填满一列时终端把光标留在末列，收尾换行只下移一行，故不能用 `+1` 向上取整。
fn prompt_rows(width: usize, cols: usize) -> usize {
    if cols == 0 {
        return 1;
    }
    1 + width.saturating_sub(1) / cols
}

/// 终端列数（取 stdout 窗口尺寸）；拿不到时按 80 列算。
fn terminal_cols() -> usize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: ws 是合法的 winsize 可写缓冲区，fd 是常量 stdout。
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0
        && ws.ws_col > 0
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

/// Ctrl-C 灰提示的共享状态：主线程武装/撤销，期限线程到期擦掉提示。
struct HintArm {
    armed: AtomicBool,
    /// 当前提示行是否为空（`Hinter` 每次重绘时更新）。
    line_empty: AtomicBool,
    /// 是否处于 `readline` 期间：期限线程只在此时动终端。
    reading: AtomicBool,
    /// stdout 是否 TTY：非 TTY 时不写光标控制序列。
    out_tty: bool,
    deadline: Mutex<Option<Instant>>,
    ready: Condvar,
}

impl HintArm {
    fn new(out_tty: bool) -> Self {
        Self {
            armed: AtomicBool::new(false),
            line_empty: AtomicBool::new(false),
            reading: AtomicBool::new(false),
            out_tty,
            deadline: Mutex::new(None),
            ready: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Option<Instant>> {
        self.deadline.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wait<'a>(&self, guard: MutexGuard<'a, Option<Instant>>) -> MutexGuard<'a, Option<Instant>> {
        self.ready.wait(guard).unwrap_or_else(|e| e.into_inner())
    }

    fn wait_timeout<'a>(
        &self,
        guard: MutexGuard<'a, Option<Instant>>,
        dur: Duration,
    ) -> MutexGuard<'a, Option<Instant>> {
        self.ready.wait_timeout(guard, dur).unwrap_or_else(|e| e.into_inner()).0
    }

    fn arm(&self, window: Duration) {
        let mut deadline = self.lock();
        *deadline = Some(Instant::now() + window);
        self.armed.store(true, Ordering::SeqCst);
        drop(deadline);
        self.ready.notify_all();
    }

    fn disarm(&self) {
        let mut deadline = self.lock();
        *deadline = None;
        self.armed.store(false, Ordering::SeqCst);
        drop(deadline);
        self.ready.notify_all();
    }

    fn armed(&self) -> bool {
        self.armed.load(Ordering::SeqCst)
    }

    /// 到期是否该擦提示：只在提示确实画在一个空输入行上、且终端在本次 `readline` 内时动手。
    fn can_erase(&self) -> bool {
        self.out_tty && self.reading.load(Ordering::SeqCst) && self.line_empty.load(Ordering::SeqCst)
    }

    /// 擦掉光标右侧的灰提示。不移动光标，因此 rustyline 的位置模型（提示符末尾、
    /// 输入行为空）保持不变，下一次重绘自然接上。
    fn erase_hint(&self) {
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[K").and_then(|()| out.flush());
    }
}

/// rustyline helper：只在武装窗口内的空行上给出灰色提示。
struct HintHelper {
    hint: Arc<HintArm>,
}

impl Completer for HintHelper {
    type Candidate = String;
}

impl Hinter for HintHelper {
    type Hint = String;

    fn hint(&self, line: &str, _pos: usize, _ctx: &Context<'_>) -> Option<String> {
        let empty = line.is_empty();
        self.hint.line_empty.store(empty, Ordering::SeqCst);
        (empty && self.hint.armed()).then(|| CTRL_C_HINT.to_string())
    }
}

impl Highlighter for HintHelper {
    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(render::dim(hint))
    }
}

impl Validator for HintHelper {}
impl Helper for HintHelper {}

/// 提示期限线程：到点撤销武装并擦掉灰提示。
/// rustyline 只在按键时重绘，从别的线程推动整行重绘会把提示符顶到下一行（留一行空白），
/// 所以这里只做“擦光标到行尾”：不移动光标，与 rustyline 的位置模型不冲突。
fn hint_timer(arm: Arc<HintArm>) {
    let mut guard = arm.lock();
    loop {
        match *guard {
            None => guard = arm.wait(guard),
            Some(deadline) => {
                let now = Instant::now();
                if now < deadline {
                    guard = arm.wait_timeout(guard, deadline - now);
                } else {
                    *guard = None;
                    arm.armed.store(false, Ordering::SeqCst);
                    let erase = arm.can_erase();
                    // 不持锁做 I/O：主线程的武装/撤销要能立刻拿到锁。
                    drop(guard);
                    if erase {
                        arm.erase_hint();
                    }
                    guard = arm.lock();
                }
            }
        }
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
    editor: Editor<HintHelper, DefaultHistory>,
    line_empty: Arc<AtomicBool>,
    line_width: Arc<AtomicUsize>,
    hint: Arc<HintArm>,
    fatal: Option<String>,
    /// 上一次 `readline` 被 Ctrl-C 中断时占用的屏幕行数：下一次读取前收回。
    reclaim_rows: usize,
}

impl TermUi {
    pub fn new() -> Result<Self, TermUiInitError> {
        if !std::io::stdin().is_terminal() {
            return Err(TermUiInitError::NotTty(
                "stdin 不是 TTY，rho 只支持交互式 REPL".into(),
            ));
        }
        let mut editor: Editor<HintHelper, DefaultHistory> = Editor::new()
            .map_err(|e| TermUiInitError::Init(format!("初始化终端失败：{e}")))?;
        let line_empty = Arc::new(AtomicBool::new(false));
        let line_width = Arc::new(AtomicUsize::new(0));
        let hint = Arc::new(HintArm::new(std::io::stdout().is_terminal()));
        editor.set_helper(Some(HintHelper { hint: hint.clone() }));
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
            .spawn(move || hint_timer(thread_hint))
            .map_err(|e| TermUiInitError::Init(format!("启动提示期限线程失败：{e}")))?;
        Ok(Self { editor, line_empty, line_width, hint, fatal: None, reclaim_rows: 0 })
    }

    /// rustyline 每次 `readline` 结束都会补一个换行（`Editor::readline_with` 里的
    /// `term.writeln()`）。被 Ctrl-C 中断的输入不提交，那几行屏幕空间要还给下一次
    /// 提示符：上移回块首并用 `\x1b[J` 擦到屏幕末尾（提示符永远是最后一段输出），
    /// 下一次 `readline` 就在原地重画。stdout 不是 TTY 时不动光标，以免污染重定向输出。
    fn reclaim_prompt_rows(&mut self) {
        let rows = std::mem::take(&mut self.reclaim_rows);
        if rows == 0 || !std::io::stdout().is_terminal() {
            return;
        }
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
        self.hint.reading.store(true, Ordering::SeqCst);
        let outcome = match self.editor.readline(PROMPT) {
            Ok(line) => ReadOutcome::Line(line),
            Err(ReadlineError::Interrupted) => {
                let line_empty = self.line_empty.load(Ordering::SeqCst);
                let width = self.line_width.load(Ordering::SeqCst);
                // 被中断的内容不提交：整块屏幕行留给下一次提示符。
                self.reclaim_rows = prompt_rows(PROMPT.len() + width, terminal_cols());
                ReadOutcome::Interrupted { line_empty }
            }
            Err(ReadlineError::Eof) => ReadOutcome::Eof,
            Err(e) => {
                self.set_fatal(format!("读取输入失败：{e}"));
                ReadOutcome::Eof
            }
        };
        self.hint.reading.store(false, Ordering::SeqCst);
        outcome
    }

    fn confirm(&mut self, cwd: &str, cmd: &str) -> ConfirmOutcome {
        print!(
            "\n当前目录：{cwd}\n将执行 bash：\n  {cmd}\n执行？[y/N] "
        );
        let mut out = std::io::stdout();
        if let Err(e) = out.flush() {
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
                    eprintln!("错误：{msg}");
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
        self.reclaim_prompt_rows();
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
            RenderEvent::LineBreak => RenderEvent::LineBreak,
            RenderEvent::ToolCall(s) => RenderEvent::ToolCall(s.clone()),
            RenderEvent::ToolResult(s) => RenderEvent::ToolResult(s.clone()),
            RenderEvent::Error(s) => RenderEvent::Error(s.clone()),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_rows_counts_wrapped_screen_rows() {
        assert_eq!(prompt_rows(0, 80), 1);
        assert_eq!(prompt_rows(5, 80), 1);
        // 刚好填满一行：光标留在末列，收尾换行只下移一行。
        assert_eq!(prompt_rows(80, 80), 1);
        assert_eq!(prompt_rows(81, 80), 2);
        assert_eq!(prompt_rows(205, 80), 3);
        // 拿不到列数时退化为不换行，且不会除零。
        assert_eq!(prompt_rows(200, 0), 1);
    }
}
