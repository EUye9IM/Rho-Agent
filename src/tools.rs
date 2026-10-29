//! bash 工具执行：进程组 spawn、超时升级、双 drain 管道、两段截断、
//! allowlist + 元字符判定。

use crate::config::BashToolConfig;
use crate::term::{self, Cancel, SignalState};
use crate::ui::{ConfirmOutcome, Ui};

use std::io::{ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 单次 bash 执行的结果。
#[derive(Debug)]
pub struct BashOutput {
    /// 截断后的完整文本：stdout + [stderr] 段 + 末尾退出码行
    pub content: String,
    /// 结束方式（决定占位文本的措辞）
    pub end: BashEnd,
}

#[derive(Debug)]
pub enum BashEnd {
    /// 正常退出（exit code 可能为非零，非零不算错误；负值 = 被信号终止）
    Exited(i32),
    /// 超时（已 SIGTERM→2s→SIGKILL 整个进程组）
    TimedOut,
    /// 被 Ctrl-C 取消（副作用已发生）
    Cancelled,
    /// spawn 失败（命令不存在属 bash 127，不是这里；此处是 fork/exec 层失败）
    SpawnFailed(String),
    /// waitpid 失败（回收错误，与信号退出区分：后者是 Exited 的负值）
    ReapFailed(String),
    /// quit 已请求：工具等待循环只终止进程组，把 quit 上报 agent loop，
    /// 不自行退出
    QuitRequested,
}

/// allowlist + 元字符判定：命令与规则完全相同，或前缀后紧接空格；
/// 且不含 `; & | < > $`、反引号、换行。规则前后空白在比较时忽略。
pub fn allowed(command: &str, allow: &[String]) -> bool {
    const META: &[char] = &[';', '&', '|', '<', '>', '$', '`', '\n', '\r'];
    if command.chars().any(|c| META.contains(&c)) {
        return false;
    }
    allow.iter().any(|rule| {
        let rule = rule.trim();
        !rule.is_empty()
            && (command == rule
                || command
                    .strip_prefix(rule)
                    .is_some_and(|rest| rest.starts_with(' ')))
    })
}

/// 执行 `bash -c <command>`（前台，工作目录恒为会话 cwd = 进程启动目录）。
///
/// 线程模型：
/// - setsid 建进程组；stdout/stderr 各一个 drain 线程，两段截断缓冲
/// - 主线程是唯一 reaper（waitpid 只在此处）；退出状态存共享标志
/// - drain 观察到退出 → 有界收尾读取（短超时）→ 自行退出并关自己的读端
/// - 超时/取消升级：SIGTERM 发 pgid → 2s → 无论子进程状态对 pgid 发 SIGKILL → 回收
/// - cancel 的 quit（第二次 Ctrl-C）：只终止进程组并返回给 agent loop，不自行退出
pub fn run_bash(command: &str, tool_cfg: &BashToolConfig, cancel: &Cancel) -> BashOutput {
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: pre_exec 在 fork 后、exec 前运行；setsid 是 async-signal-safe 的。
    // 这里用 setsid 而非安全的 `process_group(0)`：后者只建新进程组，子进程仍属本
    // 会话并保留控制终端，命令若打开 `/dev/tty`（ssh、sudo、分页器等）会阻塞到超时；
    // setsid 新开会话并脱离控制终端，这类命令会直接失败而不是挂住。
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
                content: format!("无法启动命令：{e}"),
                end: BashEnd::SpawnFailed(e.to_string()),
            };
        }
    };
    let pgid = child.id() as i32;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let exit_seen = Arc::new(AtomicBool::new(false));
    let limit = tool_cfg.max_output_bytes;

    let (out_tx, out_rx) = mpsc::channel::<DrainOutcome>();
    let (err_tx, err_rx) = mpsc::channel::<DrainOutcome>();
    let mut out_handle: Option<std::thread::JoinHandle<()>> = None;
    let mut err_handle: Option<std::thread::JoinHandle<()>> = None;
    if let Some(stdout) = stdout {
        let flag = exit_seen.clone();
        out_handle = Some(std::thread::spawn(move || {
            let _ = out_tx.send(drain_stream(stdout, flag, limit));
        }));
    }
    if let Some(stderr) = stderr {
        let flag = exit_seen.clone();
        err_handle = Some(std::thread::spawn(move || {
            let _ = err_tx.send(drain_stream(stderr, flag, limit));
        }));
    }

    let started = Instant::now();
    let timeout = Duration::from_secs(tool_cfg.timeout_seconds);
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
                // quit（第二次 Ctrl-C）：只终止进程组并返回给 agent loop，
                // 不自行退出
                _ => BashEnd::QuitRequested,
            };
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    // 通知 drain 线程进入有界收尾；主线程仍是唯一 reaper。
    exit_seen.store(true, Ordering::SeqCst);
    let mut notes: Vec<String> = Vec::new();
    let stdout = collect_drain(&out_rx, &mut out_handle, "stdout", &mut notes);
    let stderr = collect_drain(&err_rx, &mut err_handle, "stderr", &mut notes);

    let mut content = format_output(stdout, stderr, &end, &notes);
    if let Some(w) = kill_warning {
        content.push('\n');
        content.push_str(&format!("[警告：向进程组发信号失败：{w}]"));
    }
    BashOutput { content, end }
}

/// 接收单个 drain 线程的结果（有界 1s）。
/// 成功收到才 join：此时线程只差收尾，join 很快；
/// 超时则放弃 join（detach）——线程可能仍阻塞在读取里，join 会让本函数返回无界，
/// detach 后它会在管道关闭时自行退出。捕获失败（非阻塞设置失败）以注记行呈现。
fn collect_drain(
    rx: &mpsc::Receiver<DrainOutcome>,
    handle: &mut Option<std::thread::JoinHandle<()>>,
    label: &str,
    notes: &mut Vec<String>,
) -> StreamCapture {
    match rx.recv_timeout(Duration::from_secs(1)) {
        Ok(DrainOutcome::Capture(c)) => {
            if let Some(h) = handle.take() {
                let _ = h.join();
            }
            c
        }
        Ok(DrainOutcome::SetupFailed(e)) => {
            if let Some(h) = handle.take() {
                let _ = h.join();
            }
            notes.push(format!("[{label} 捕获失败：{e}]"));
            StreamCapture::default()
        }
        // 超时/断开：放弃该流剩余输出，detach 线程，返回已有内容为空。
        Err(_) => StreamCapture::default(),
    }
}

/// 主线程是唯一 reaper：SIGTERM → 最多 2s → 无论直接子进程状态对 pgid 发 SIGKILL → 回收。
/// 若期间观察到 quit，提前升级到 SIGKILL。返回发信号失败的警告（无则 None）。
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
        // 直接子进程可以先退出；进程组清理仍等满 2s（后台子进程可能还在）。
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

/// 单条流的两段捕获结果（已按 UTF-8 边界裁剪）。
#[derive(Debug, Default)]
struct StreamCapture {
    head: String,
    tail: String,
    omitted: u64,
}

impl StreamCapture {
    fn render(&self) -> String {
        if self.omitted == 0 {
            format!("{}{}", self.head, self.tail)
        } else {
            format!("{}\n…省略 {} 字节…\n{}", self.head, self.omitted, self.tail)
        }
    }
}

/// 两段截断缓冲：头部前缀 + 滑动尾部窗口，中间字节计数丢弃。
struct TruncBuf {
    head_cap: usize,
    tail_cap: usize,
    head: Vec<u8>,
    tail: Vec<u8>,
    total: u64,
}

impl TruncBuf {
    fn new(limit: usize) -> Self {
        let head_cap = limit / 2;
        Self {
            head_cap,
            tail_cap: limit - head_cap,
            head: Vec::new(),
            tail: Vec::new(),
            total: 0,
        }
    }

    fn push(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        if self.head.len() < self.head_cap {
            let take = (self.head_cap - self.head.len()).min(data.len());
            self.head.extend_from_slice(&data[..take]);
            data = &data[take..];
        }
        if !data.is_empty() && self.tail_cap > 0 {
            self.tail.extend_from_slice(data);
            if self.tail.len() > self.tail_cap {
                let excess = self.tail.len() - self.tail_cap;
                self.tail.drain(..excess);
            }
        }
    }

    fn finish(self) -> StreamCapture {
        let limit = self.head_cap + self.tail_cap;
        if self.total <= limit as u64 {
            let mut bytes = self.head;
            bytes.extend_from_slice(&self.tail);
            return StreamCapture {
                head: String::from_utf8_lossy(&bytes).into_owned(),
                tail: String::new(),
                omitted: 0,
            };
        }

        let head_bytes = utf8_head(&self.head);
        let tail_bytes = utf8_tail(&self.tail);
        let kept = (head_bytes.len() + tail_bytes.len()) as u64;
        StreamCapture {
            head: String::from_utf8_lossy(head_bytes).into_owned(),
            tail: String::from_utf8_lossy(tail_bytes).into_owned(),
            omitted: self.total.saturating_sub(kept),
        }
    }
}

/// 头部前缀截断：末尾若是半个 UTF-8 字符则回退到合法边界。
fn utf8_head(bytes: &[u8]) -> &[u8] {
    match std::str::from_utf8(bytes) {
        Ok(_) => bytes,
        // error_len() == None 表示输入以不完整序列结尾。
        Err(e) if e.error_len().is_none() => &bytes[..e.valid_up_to()],
        // 非法字节交给 lossy 处理，不做二次破坏。
        Err(_) => bytes,
    }
}

/// 尾部窗口截断：开头若是半个 UTF-8 字符则前移到合法边界。
fn utf8_tail(mut bytes: &[u8]) -> &[u8] {
    for _ in 0..4 {
        match std::str::from_utf8(bytes) {
            Ok(_) => return bytes,
            Err(e) if e.valid_up_to() == 0 && e.error_len().is_some() => {
                // 起始处是 continuation byte / 非法字节：跳过 1 字节重试。
                bytes = &bytes[1..];
            }
            Err(e) if e.error_len().is_none() => {
                // 末尾不完整：只保留完整部分。
                return &bytes[..e.valid_up_to()];
            }
            Err(_) => return bytes,
        }
    }
    bytes
}

/// 单条流的捕获结果或捕获失败（非阻塞设置失败时无法保证有界收尾，显式报告而非静默降级）。
#[derive(Debug)]
enum DrainOutcome {
    Capture(StreamCapture),
    SetupFailed(String),
}

/// 并行读取一条管道；主线程退出后进入 500ms 有界收尾读取。
fn drain_stream<R: Read + AsRawFd>(
    mut reader: R,
    exit_seen: Arc<AtomicBool>,
    limit: usize,
) -> DrainOutcome {
    let fd = reader.as_raw_fd();
    // SAFETY: fd 由 reader 持有，在函数期间有效。
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags == -1
            || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1
        {
            // 非阻塞设置失败：后续读取无法兑现收尾期限，静默降级成阻塞模式
            // 会让线程（及 join 它的主线程）无界等待。放弃捕获剩余数据，直接返回。
            let e = std::io::Error::last_os_error();
            return DrainOutcome::SetupFailed(e.to_string());
        }
    }

    let mut buf = TruncBuf::new(limit);
    let mut chunk = [0u8; 8192];
    let mut final_deadline: Option<Instant> = None;
    loop {
        // 主线程已回收 bash：进入有界收尾，防止后台进程持续写入拖住线程。
        if final_deadline.is_none() && exit_seen.load(Ordering::SeqCst) {
            final_deadline = Some(Instant::now() + Duration::from_millis(500));
        }
        if let Some(deadline) = final_deadline {
            if Instant::now() >= deadline {
                break;
            }
        }
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.push(&chunk[..n]),
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => match final_deadline {
                Some(_) => poll_readable(fd, 50),
                None => poll_readable(fd, 100),
            },
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    DrainOutcome::Capture(buf.finish())
}

fn poll_readable(fd: i32, timeout_ms: i32) {
    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    // SAFETY: pfd 是合法的 pollfd。
    unsafe {
        let _ = libc::poll(&mut pfd, 1, timeout_ms);
    }
}

fn format_output(
    stdout: StreamCapture,
    stderr: StreamCapture,
    end: &BashEnd,
    notes: &[String],
) -> String {
    let stdout_text = stdout.render();
    let stderr_text = stderr.render();
    let mut out = String::new();
    out.push_str(&stdout_text);
    if !stderr_text.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("[stderr]\n");
        out.push_str(&stderr_text);
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    match end {
        BashEnd::Exited(code) if *code >= 0 => {
            out.push_str(&format!("[Command finished with exit code {code}]"))
        }
        BashEnd::Exited(_) => out.push_str("[Command terminated by signal]"),
        BashEnd::TimedOut => {
            out.push_str("[Command timed out: 已部分执行（超时），进程组已终止]")
        }
        BashEnd::Cancelled => {
            out.push_str("[Command cancelled: 已部分执行（被取消，副作用已发生），进程组已终止]")
        }
        BashEnd::QuitRequested => out.push_str(
            "[Command cancelled (quit requested): 已部分执行（被取消，副作用已发生），进程组已终止]",
        ),
        BashEnd::SpawnFailed(e) => out.push_str(&format!("[Command failed to start: {e}]")),
        BashEnd::ReapFailed(e) => out.push_str(&format!("[Command reap failed: {e}]")),
    }
    // 捕获/终止警告附在末尾（诊断信息，不掩盖结束标记）。
    for note in notes {
        out.push('\n');
        out.push_str(note);
    }
    out
}

/// 确认流（agent loop 的 dispatch_tool_call 调用）：
/// allowlist 自动放行；否则向用户确认（--yes 跳过全部确认）。
pub fn gate(
    ui: &mut dyn Ui,
    yes: bool,
    cwd: &str,
    command: &str,
    allow: &[String],
) -> ConfirmOutcome {
    if allowed(command, allow) || yes {
        ConfirmOutcome::Yes
    } else {
        ui.confirm(cwd, command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用便捷封装：一次性推入并渲染（流式路径由 `drain_stream` 分块推入）。
    fn truncate(bytes: &[u8], limit: usize) -> String {
        let mut buf = TruncBuf::new(limit);
        buf.push(bytes);
        buf.finish().render()
    }

    #[test]
    fn allowlist_exact_match() {
        assert!(allowed("ls", &["ls".into()]));
    }

    #[test]
    fn allowlist_prefix_with_space() {
        assert!(allowed("ls -la", &["ls".into()]));
        // 前缀后无空格边界：lsof 不被 ls 放行
        assert!(!allowed("lsof", &["ls".into()]));
    }

    #[test]
    fn allowlist_rule_whitespace_is_ignored() {
        assert!(allowed("ls", &["  ls  ".into()]));
        assert!(allowed("ls -la", &[" ls ".into()]));
        assert!(!allowed("lsof", &[" ls ".into()]));
        assert!(!allowed("ls", &["   ".into()]));
    }

    #[test]
    fn metachar_rejected() {
        assert!(!allowed("ls; rm -rf /", &["ls".into()]));
        assert!(!allowed("ls | cat", &["ls".into()]));
        assert!(!allowed("ls\nrm", &["ls".into()]));
    }

    #[test]
    fn truncate_keeps_head_and_tail() {
        let data = vec![b'a'; 100];
        assert!(truncate(&data, 20).contains("省略"));
    }

    #[test]
    fn truncate_short_is_lossless() {
        assert_eq!(truncate("你好".as_bytes(), 100), "你好");
    }

    #[test]
    fn truncation_preserves_utf8_boundaries() {
        let data = "你".repeat(100);
        let out = truncate(data.as_bytes(), 21);
        assert!(out.is_char_boundary(0));
        assert!(out.starts_with('你'));
        assert!(out.ends_with('你'));
    }

    #[test]
    fn trunc_buf_counts_actual_dropped_bytes() {
        let mut buf = TruncBuf::new(20);
        buf.push(&[b'a'; 100]);
        let capture = buf.finish();
        assert_eq!(capture.head.len() + capture.tail.len(), 20);
        assert_eq!(capture.omitted, 80);
        assert!(capture.render().contains("省略 80 字节"));
    }

    #[test]
    fn trunc_buf_respects_utf8_boundaries() {
        let mut buf = TruncBuf::new(7); // head=3, tail=4
        buf.push("你".repeat(10).as_bytes());
        let capture = buf.finish();
        assert!(!capture.head.contains('\u{FFFD}'));
        assert!(!capture.tail.contains('\u{FFFD}'));
        assert_eq!(capture.head, "你");
        assert_eq!(capture.tail, "你");
        assert_eq!(capture.omitted, 24);
    }

    #[test]
    fn trunc_buf_keeps_utf8_split_when_output_fits_limit() {
        let mut buf = TruncBuf::new(8); // head=4 在“你”中间，尾部保留剩余字节
        buf.push("ab你".as_bytes());
        assert_eq!(buf.finish().render(), "ab你");
        assert_eq!(truncate("ab你".as_bytes(), 8), "ab你");
    }

    #[test]
    fn trunc_buf_keeps_utf8_split_across_multiple_pushes() {
        let mut buf = TruncBuf::new(8); // head=4，尾窗 4
        for chunk in "ab你".as_bytes().chunks(1) {
            buf.push(chunk);
        }
        assert_eq!(buf.finish().render(), "ab你");
    }
}
