//! 用户交互抽象：Ui trait + 生产实现 TermUi + 测试实现 ScriptedUi。
//! Ui 不承载退出语义（退出由 REPL 层判定）。见 docs/architecture.md 三节。

use crate::render::{self, RenderEvent};
use std::collections::VecDeque;
use std::fmt;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rustyline::error::ReadlineError;
use rustyline::{
    Cmd, ConditionalEventHandler, DefaultEditor, Event, EventContext, EventHandler, KeyEvent,
    Modifiers, RepeatCount,
};

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

/// Ctrl-C 按键处理器：记录被中断时输入行是否为空，再执行默认 Interrupt。
struct CtrlCHandler {
    line_empty: Arc<AtomicBool>,
}

impl ConditionalEventHandler for CtrlCHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext,
    ) -> Option<Cmd> {
        self.line_empty.store(ctx.line().is_empty(), Ordering::SeqCst);
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
pub struct TermUi {
    editor: DefaultEditor,
    line_empty: Arc<AtomicBool>,
    fatal: Option<String>,
}

impl TermUi {
    pub fn new() -> Result<Self, TermUiInitError> {
        if !std::io::stdin().is_terminal() {
            return Err(TermUiInitError::NotTty(
                "stdin 不是 TTY，rho 只支持交互式 REPL".into(),
            ));
        }
        let mut editor = DefaultEditor::new()
            .map_err(|e| TermUiInitError::Init(format!("初始化终端失败：{e}")))?;
        let line_empty = Arc::new(AtomicBool::new(false));
        editor.bind_sequence(
            KeyEvent::new('\x03', Modifiers::NONE),
            EventHandler::Conditional(Box::new(CtrlCHandler {
                line_empty: line_empty.clone(),
            })),
        );
        Ok(Self { editor, line_empty, fatal: None })
    }

    fn set_fatal(&mut self, msg: impl Into<String>) {
        if self.fatal.is_none() {
            self.fatal = Some(msg.into());
        }
    }
}

impl Ui for TermUi {
    fn read_line(&mut self) -> ReadOutcome {
        match self.editor.readline("rho> ") {
            Ok(line) => ReadOutcome::Line(line),
            Err(ReadlineError::Interrupted) => ReadOutcome::Interrupted {
                line_empty: self.line_empty.load(Ordering::SeqCst),
            },
            Err(ReadlineError::Eof) => ReadOutcome::Eof,
            Err(e) => {
                self.set_fatal(format!("读取输入失败：{e}"));
                ReadOutcome::Eof
            }
        }
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

    fn take_fatal(&mut self) -> Option<String> {
        self.fatal.take()
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
