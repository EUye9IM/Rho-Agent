//! bash 工具：schema、参数校验、进程组执行、单 pipe 捕获与末尾字符截断。

use super::{truncate_chars, Tool, ToolContext, ToolResult, ToolSpec, ToolStatus};
use crate::term::{self, Cancel, SignalState};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{ErrorKind, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 300;
const MAX_OUTPUT_CHARS: usize = 50_000;
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(500);
const OUTPUT_COLLECT_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Default)]
pub struct BashTool;

impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".to_string(),
            description: "在用户机器上执行一条 bash 命令（无持久 shell，每次调用相互独立）。"
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "要执行的完整 bash 命令"
                    },
                    "timeout": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_TIMEOUT_SECS,
                        "default": DEFAULT_TIMEOUT_SECS,
                        "description": "超时时间（秒），默认 120，最大 300"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "命令的初始工作目录；相对路径按 rho 启动目录解析"
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    fn summary(&self, args: &Value) -> String {
        args.get("command")
            .and_then(Value::as_str)
            .map(|s| truncate_chars(s, 200))
            .unwrap_or_else(|| super::truncate_chars(&args.to_string(), 200))
    }

    fn invoke(&self, args: Value, ctx: &ToolContext<'_>) -> ToolResult {
        let args: BashArgs = match serde_json::from_value(args) {
            Ok(args) => args,
            Err(e) => return args_failure(format!("参数解析失败：{e}")),
        };
        if args.command.trim().is_empty() {
            return args_failure("command 不能为空".to_string());
        }
        let timeout = match args.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS as i64) {
            n if (1..=MAX_TIMEOUT_SECS as i64).contains(&n) => Duration::from_secs(n as u64),
            n => {
                return args_failure(format!(
                    "timeout 必须是 1 到 {MAX_TIMEOUT_SECS} 之间的整数秒，收到 {n}"
                ));
            }
        };
        let cwd = match args.cwd {
            None => None,
            Some(raw) if raw.is_empty() => {
                return args_failure("cwd 不能为空".to_string());
            }
            Some(raw) if raw == "~" => {
                return args_failure("cwd 不展开 ~，请传入实际路径".to_string());
            }
            Some(raw) => {
                let path = if Path::new(&raw).is_absolute() {
                    PathBuf::from(&raw)
                } else {
                    ctx.cwd.join(&raw)
                };
                if !path.is_dir() {
                    return args_failure(format!("无法进入工作目录：{}", path.display()));
                }
                Some(path)
            }
        };

        if ctx.cancel.pending().is_some() {
            let output = render_output("interrupted", "-", 0, "已取消，未启动命令".to_string());
            return ToolResult::interrupted(output);
        }

        let run = run_bash(
            &args.command,
            timeout,
            cwd.as_deref().unwrap_or(ctx.cwd),
            ctx.cancel,
        );
        let (status_word, tool_status, exit_code) = match run.end {
            BashEnd::Exited(0) => ("completed", ToolStatus::Success, "0".to_string()),
            BashEnd::Exited(code) if code > 0 => ("failed", ToolStatus::Failure, code.to_string()),
            BashEnd::Exited(_) => ("failed", ToolStatus::Failure, "-".to_string()),
            BashEnd::TimedOut => ("timed_out", ToolStatus::Failure, "-".to_string()),
            BashEnd::Cancelled | BashEnd::QuitRequested => {
                ("interrupted", ToolStatus::Interrupted, "-".to_string())
            }
            BashEnd::SpawnFailed(_) | BashEnd::ReapFailed(_) => {
                ("failed", ToolStatus::Failure, "-".to_string())
            }
        };
        let output = render_output(status_word, &exit_code, run.omitted_chars, run.body);
        ToolResult {
            display: Some(output.clone()),
            output,
            status: tool_status,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout: Option<i64>,
    #[serde(default)]
    cwd: Option<String>,
}

fn args_failure(message: String) -> ToolResult {
    let output = render_output("failed", "-", 0, format!("工具参数错误：{message}"));
    ToolResult::failure(output)
}

fn render_output(status: &str, exit_code: &str, omitted_chars: u64, body: String) -> String {
    let truncated = omitted_chars > 0;
    let mut out = format!("[status] {status}\n[exit_code] {exit_code}\n[truncated] {truncated}\n");
    if truncated {
        out.push_str(&format!("[omitted_chars] {omitted_chars}\n"));
    }
    out.push_str("[output]\n");
    out.push_str(&body);
    out
}

// ---------------------------------------------------------------------------
// 执行层
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct BashOutput {
    /// 只保留末尾的输出正文（不含固定文本头）。
    body: String,
    omitted_chars: u64,
    end: BashEnd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BashEnd {
    /// 正常退出（exit code 可能为非零，非零不算错误；负值 = 被信号终止）
    Exited(i32),
    TimedOut,
    Cancelled,
    SpawnFailed(String),
    ReapFailed(String),
    QuitRequested,
}

/// 执行 `bash -c <command>`。标准输出与错误输出指向同一个 pipe；
/// `body` 是末尾 `MAX_OUTPUT_CHARS` 个字符。
fn run_bash(command: &str, timeout: Duration, cwd: &Path, cancel: &Cancel) -> BashOutput {
    let (reader, writer) = match make_pipe() {
        Ok(pair) => pair,
        Err(e) => {
            return BashOutput {
                body: format!("无法创建输出管道：{e}"),
                omitted_chars: 0,
                end: BashEnd::SpawnFailed(e.to_string()),
            };
        }
    };
    let writer2 = match writer.try_clone() {
        Ok(w) => w,
        Err(e) => {
            return BashOutput {
                body: format!("无法复制输出管道：{e}"),
                omitted_chars: 0,
                end: BashEnd::SpawnFailed(e.to_string()),
            };
        }
    };

    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(writer))
        .stderr(Stdio::from(writer2));
    // SAFETY: pre_exec 在 fork 后、exec 前运行；setsid 是 async-signal-safe 的。
    // 新会话脱离控制终端，避免命令打开 /dev/tty 后挂到超时。
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return BashOutput {
                body: format!("无法启动命令：{e}"),
                omitted_chars: 0,
                end: BashEnd::SpawnFailed(e.to_string()),
            };
        }
    };
    let pgid = child.id() as i32;

    let exit_seen = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<DrainOutcome>();
    let flag = exit_seen.clone();
    let drain_handle = std::thread::spawn(move || {
        let _ = tx.send(drain_stream(reader, flag, MAX_OUTPUT_CHARS));
    });

    let started = Instant::now();
    let mut kill_warning: Option<String> = None;
    let end = loop {
        match child.try_wait() {
            Ok(Some(status)) => break BashEnd::Exited(status.code().unwrap_or(-1)),
            Ok(None) => {}
            Err(e) => break BashEnd::ReapFailed(e.to_string()),
        }

        if started.elapsed() >= timeout {
            kill_warning = terminate_group(&mut child, pgid, cancel);
            break BashEnd::TimedOut;
        }

        if let Some(sig) = cancel.pending() {
            kill_warning = terminate_group(&mut child, pgid, cancel);
            break match sig {
                SignalState::Cancel => BashEnd::Cancelled,
                _ => BashEnd::QuitRequested,
            };
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    // 通知 drain 进入有界收尾；主线程仍是唯一 reaper。
    exit_seen.store(true, Ordering::SeqCst);
    let (capture, mut warning) = collect_drain(&rx, drain_handle);
    if let Some(w) = kill_warning {
        warning = Some(match warning {
            Some(old) => format!("{old}；{w}"),
            None => w,
        });
    }

    let omitted_chars = capture
        .total_chars
        .saturating_sub(capture.text.chars().count() as u64);
    let mut body = capture.text;
    if let Some(w) = warning {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&format!("[警告：{w}]"));
    }
    BashOutput {
        body,
        omitted_chars,
        end,
    }
}

fn make_pipe() -> std::io::Result<(std::fs::File, std::fs::File)> {
    let mut fds = [0i32; 2];
    // SAFETY: fds 是合法的两元素数组，pipe2 成功后返回两个新 fd。
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fd 由 pipe2 创建，交给 File 管理。
    unsafe {
        Ok((
            std::fs::File::from_raw_fd(fds[0]),
            std::fs::File::from_raw_fd(fds[1]),
        ))
    }
}

/// 主线程是唯一 reaper：SIGTERM → 最多 2s → 无论直接子进程状态对 pgid 发 SIGKILL → 回收。
fn terminate_group(child: &mut Child, pgid: i32, cancel: &Cancel) -> Option<String> {
    let mut warning = match term::kill_pg(pgid, libc::SIGTERM) {
        Ok(()) => None,
        Err(e) => Some(format!("SIGTERM：{e}")),
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if cancel.state() == SignalState::Quit {
            break;
        }
        let _ = child.try_wait();
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Err(e) = term::kill_pg(pgid, libc::SIGKILL) {
        let msg = format!("SIGKILL：{e}");
        warning = Some(match warning {
            Some(w) => format!("{w}；{msg}"),
            None => msg,
        });
    }
    let _ = child.wait();
    warning
}

fn collect_drain(
    rx: &mpsc::Receiver<DrainOutcome>,
    handle: std::thread::JoinHandle<()>,
) -> (TailCapture, Option<String>) {
    match rx.recv_timeout(OUTPUT_COLLECT_TIMEOUT) {
        Ok(DrainOutcome::Capture(c)) => {
            let _ = handle.join();
            (c, None)
        }
        Ok(DrainOutcome::SetupFailed(e)) => {
            let _ = handle.join();
            (TailCapture::default(), Some(format!("输出捕获失败：{e}")))
        }
        // 超时/断开：放弃该流剩余输出，detach 线程。
        Err(_) => (TailCapture::default(), None),
    }
}

#[derive(Debug)]
enum DrainOutcome {
    Capture(TailCapture),
    SetupFailed(String),
}

/// 单 pipe 读取线程；主线程退出后进入有界收尾读取。
fn drain_stream<R: Read + AsRawFd>(
    mut reader: R,
    exit_seen: Arc<AtomicBool>,
    limit: usize,
) -> DrainOutcome {
    let fd = reader.as_raw_fd();
    // SAFETY: fd 由 reader 持有，在函数期间有效。
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags == -1 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
            let e = std::io::Error::last_os_error();
            return DrainOutcome::SetupFailed(e.to_string());
        }
    }

    let mut decoder = Utf8Tail::new(limit);
    let mut chunk = [0u8; 8192];
    let mut final_deadline: Option<Instant> = None;
    loop {
        if final_deadline.is_none() && exit_seen.load(Ordering::SeqCst) {
            final_deadline = Some(Instant::now() + OUTPUT_DRAIN_GRACE);
        }
        if let Some(deadline) = final_deadline {
            if Instant::now() >= deadline {
                break;
            }
        }
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => decoder.push_bytes(&chunk[..n]),
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => match final_deadline {
                Some(_) => poll_readable(fd, 50),
                None => poll_readable(fd, 100),
            },
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    decoder.finish();
    DrainOutcome::Capture(decoder.buffer.finish())
}

fn poll_readable(fd: i32, timeout_ms: i32) {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: pfd 是合法的 pollfd。
    unsafe {
        let _ = libc::poll(&mut pfd, 1, timeout_ms);
    }
}

#[derive(Debug, Default)]
struct TailCapture {
    text: String,
    total_chars: u64,
}

struct TailBuffer {
    chars: VecDeque<char>,
    limit: usize,
    total_chars: u64,
}

impl TailBuffer {
    fn new(limit: usize) -> Self {
        Self {
            chars: VecDeque::with_capacity(limit.min(8192)),
            limit,
            total_chars: 0,
        }
    }

    fn push_str(&mut self, text: &str) {
        for ch in text.chars() {
            self.total_chars += 1;
            self.chars.push_back(ch);
            if self.chars.len() > self.limit {
                self.chars.pop_front();
            }
        }
    }

    fn finish(self) -> TailCapture {
        TailCapture {
            text: self.chars.into_iter().collect(),
            total_chars: self.total_chars,
        }
    }
}

/// UTF-8 流式解码：跨 read 边界的半个字符先留在 pending，非法字节按替换字符计数。
struct Utf8Tail {
    pending: Vec<u8>,
    buffer: TailBuffer,
}

impl Utf8Tail {
    fn new(limit: usize) -> Self {
        Self {
            pending: Vec::new(),
            buffer: TailBuffer::new(limit),
        }
    }

    fn push_bytes(&mut self, data: &[u8]) {
        self.pending.extend_from_slice(data);
        loop {
            if self.pending.is_empty() {
                break;
            }
            let invalid_len = match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    self.buffer.push_str(text);
                    self.pending.clear();
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        let text = std::str::from_utf8(&self.pending[..valid])
                            .expect("valid_up_to 保证前缀合法");
                        self.buffer.push_str(text);
                        self.pending.drain(..valid);
                        continue;
                    }
                    e.error_len()
                }
            };
            match invalid_len {
                Some(n) => {
                    self.buffer.push_str("\u{FFFD}");
                    self.pending.drain(..n);
                }
                // 末尾是不完整字符：留到下一次 read。
                None => break,
            }
        }
    }

    fn finish(&mut self) {
        if !self.pending.is_empty() {
            let text = String::from_utf8_lossy(&self.pending).into_owned();
            self.buffer.push_str(&text);
            self.pending.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir() -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("rho-bash-test-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run(command: &str, timeout_secs: u64) -> BashOutput {
        run_bash(
            command,
            Duration::from_secs(timeout_secs),
            Path::new("."),
            &Cancel::new(),
        )
    }

    #[test]
    fn captures_stdout_and_stderr_in_one_pipe() {
        let out = run("printf hi; printf err >&2; exit 3", 5);
        assert_eq!(out.end, BashEnd::Exited(3));
        assert!(out.body.contains("hi"));
        assert!(out.body.contains("err"));
        assert!(!out.body.contains("[stderr]"));
    }

    #[test]
    fn keeps_only_tail_in_characters() {
        let text = format!("head-{}-tail", "你".repeat(100));
        let mut tail = Utf8Tail::new(20);
        tail.push_bytes(text.as_bytes());
        tail.finish();
        let capture = tail.buffer.finish();
        assert_eq!(capture.text.chars().count(), 20);
        assert!(capture.text.ends_with("tail"));
        assert_eq!(capture.total_chars, text.chars().count() as u64);
    }

    #[test]
    fn utf8_split_across_chunks_is_not_replacement() {
        let data = "a你b".as_bytes();
        let mut tail = Utf8Tail::new(100);
        for chunk in data.chunks(1) {
            tail.push_bytes(chunk);
        }
        tail.finish();
        let capture = tail.buffer.finish();
        assert_eq!(capture.text, "a你b");
    }

    #[test]
    fn invalid_utf8_counts_replacement_chars() {
        let mut tail = Utf8Tail::new(100);
        tail.push_bytes(&[0xff, b'a']);
        tail.finish();
        let capture = tail.buffer.finish();
        assert_eq!(capture.text, "\u{FFFD}a");
        assert_eq!(capture.total_chars, 2);
    }

    #[test]
    fn timeout_terminates_group() {
        let started = Instant::now();
        let out = run("sleep 30", 1);
        assert_eq!(out.end, BashEnd::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn background_child_does_not_hold_tool_open() {
        let started = Instant::now();
        let out = run("echo hi; sleep 30 &", 5);
        assert_eq!(out.end, BashEnd::Exited(0));
        assert!(out.body.contains("hi"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn timeout_kills_background_children_too() {
        let started = Instant::now();
        let out = run("sleep 30 & sleep 30", 1);
        assert_eq!(out.end, BashEnd::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn cancel_terminates_group() {
        let cancel = Cancel::new();
        let injector = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            injector.inject();
        });
        let started = Instant::now();
        let out = run_bash(
            "sleep 30 & sleep 30",
            Duration::from_secs(30),
            Path::new("."),
            &cancel,
        );
        assert_eq!(out.end, BashEnd::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn render_has_fixed_header() {
        let output = render_output("completed", "0", 0, "hi".to_string());
        assert!(output
            .starts_with("[status] completed\n[exit_code] 0\n[truncated] false\n[output]\nhi"));
    }

    #[test]
    fn rejects_unknown_fields_and_bad_timeout() {
        let cancel = Cancel::new();
        let ctx = ToolContext {
            cwd: Path::new("."),
            cancel: &cancel,
        };
        let extra = BashTool.invoke(json!({"command": "true", "extra": 1}), &ctx);
        assert_eq!(extra.status, ToolStatus::Failure);
        assert!(extra.output.contains("unknown field"));
        let timeout = BashTool.invoke(json!({"command": "true", "timeout": 301}), &ctx);
        assert_eq!(timeout.status, ToolStatus::Failure);
        assert!(timeout.output.contains("1 到 300"));
    }

    #[test]
    fn cwd_resolves_relative_to_context_and_must_exist() {
        let dir = temp_dir();
        std::fs::write(dir.join("marker"), "").unwrap();
        let cancel = Cancel::new();
        let ctx = ToolContext {
            cwd: &dir,
            cancel: &cancel,
        };
        let ok = BashTool.invoke(json!({"command": "test -f marker", "cwd": "."}), &ctx);
        assert_eq!(ok.status, ToolStatus::Success);
        assert_eq!(ok.display.as_deref(), Some(ok.output.as_str()));

        let bad = BashTool.invoke(json!({"command": "true", "cwd": "missing"}), &ctx);
        assert_eq!(bad.status, ToolStatus::Failure);
        assert!(bad.output.contains("无法进入工作目录"));
    }
}
