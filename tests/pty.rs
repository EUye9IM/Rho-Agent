//! PTY 集成测试：真实终端（openpty）+ 真实 rho 二进制 + 真实 SIGINT 信号路径。
//!
//! 覆盖此前只能手工验收的链路：
//! - 信号到记录：on_sigint → self-pipe → rho-sigint 线程 → Cancel（inject()
//!   绕过这段，这里是唯一自动化覆盖）
//! - 终端行为：灰提示出现/到期擦除、Ctrl-C 行回收、rustyline 按键路径
//!
//! 数据面全部走本地 mock SSE 服务，不调用真实 API。
//! 断言基于 vt100 屏幕网格（最终状态），轮询带超时；双按窗口/到期擦除
//! 这类语义本身带时间的用例除外，不用固定 sleep 对齐时序。
//!
//! 已知竞态（macOS 实测）：行回收后的新提示符与下一个字节的输入窗口存在
//! 毫秒级竞态——提示符出现后立即写入的首个字节可能丢失。
//! 人类敲键延迟（~100ms）撞不上该窗口；用例统一在提示符后稳定 50ms 再发输入。

use portable_pty::{Child, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use vt100::Parser;

/// rho 调试二进制路径（测试工作目录 = crate 根）。
const RHO_BIN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/debug/rho");
/// 终端尺寸：固定 80×24，行回收断言的基准。
const COLS: u16 = 80;
const ROWS: u16 = 24;
/// 提示符出现后的稳定期：避开回收后输入窗口的毫秒级竞态（模拟真实敲键节奏）。
const SETTLE: Duration = Duration::from_millis(50);

static TMP_SEQ: AtomicUsize = AtomicUsize::new(0);

/// 通用屏幕断言超时：rho 启动 + 渲染余量。
const SCREEN_TIMEOUT: Duration = Duration::from_secs(8);
/// 双按窗口：两次空行 Ctrl-C 间隔须小于它；到期擦除测试须等待超过它。
const CTRL_C_WINDOW: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// mock SSE 服务（与 tests/e2e_mock.rs 同模式；独立实现避免跨测试文件共享）
// ---------------------------------------------------------------------------

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut buf).expect("read request");
        if n == 0 {
            return Vec::new();
        }
        data.extend_from_slice(&buf[..n]);
        if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&data[..header_end]).to_ascii_lowercase();
    let content_len = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while data.len() < header_end + content_len {
        let n = stream.read(&mut buf).expect("read body");
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    data[header_end..(header_end + content_len).min(data.len())].to_vec()
}

/// mock 服务句柄：Drop 时停止（accept 线程随即退出）。
struct MockServer {
    base_url: String,
    stop: Arc<AtomicBool>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// 单连接 mock：接收一个请求后按 `respond` 写响应。
fn start_mock(respond: impl FnOnce(&mut TcpStream) + Send + 'static) -> MockServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    std::thread::spawn(move || {
        loop {
            if thread_stop.load(Ordering::SeqCst) {
                return;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).ok();
                    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
                    let _ = read_request(&mut stream);
                    respond(&mut stream);
                    let _ = stream.shutdown(Shutdown::Both);
                    return;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    });
    MockServer { base_url, stop }
}

/// tool_calls 响应（触发 bash 确认）：一条 bash 调用后 finish。
fn tool_call_response(command: &str) -> impl FnOnce(&mut TcpStream) + Send {
    let args = serde_json::json!({ "command": command }).to_string();
    let chunk1 = serde_json::json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "function": { "name": "bash", "arguments": args }
                }]
            }
        }]
    });
    let chunk2 = serde_json::json!({
        "choices": [{ "delta": {}, "finish_reason": "tool_calls" }]
    });
    let body = format!("data: {chunk1}\n\ndata: {chunk2}\n\ndata: [DONE]\n\n");
    move |stream: &mut TcpStream| {
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        );
        let _ = stream.write_all(body.as_bytes());
        let _ = stream.flush();
    }
}

/// 慢流响应：发一个 Delta 后停 `pause`，再发剩余并正常收尾。
/// 给测试窗口在流式期间发 Ctrl-C（真实 SIGINT 路径）。
fn slow_stream_response(first: &str, rest: &str, pause: Duration) -> impl FnOnce(&mut TcpStream) + Send {
    let chunk1 = serde_json::json!({
        "choices": [{ "delta": { "content": first } }]
    });
    let chunk2 = serde_json::json!({
        "choices": [{ "delta": { "content": rest }, "finish_reason": "stop" }]
    });
    move |stream: &mut TcpStream| {
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        );
        let _ = stream.write_all(format!("data: {chunk1}\n\n").as_bytes());
        let _ = stream.flush();
        std::thread::sleep(pause);
        let _ = stream.write_all(format!("data: {chunk2}\n\ndata: [DONE]\n\n").as_bytes());
        let _ = stream.flush();
    }
}

/// 写 mock 配置并返回路径。provider 指向 mock 服务。
fn write_config(base_url: &str) -> String {
    let path = std::env::temp_dir().join(format!(
        "rho-pty-{}-{}.toml",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::write(
        &path,
        format!(
            r#"
[providers.mock]
protocol = "openai_chat"
base_url = "{base_url}"
[providers.mock.auth]
headers = {{ Authorization = "Bearer sk-pty-test" }}

[models."mock/m"]
context_window = 10000
max_output = 1000

[tools.bash]
allow = []
timeout_seconds = 2
max_output_bytes = 4096
"#
        ),
    )
    .unwrap();
    path.display().to_string()
}

// ---------------------------------------------------------------------------
// PTY harness
// ---------------------------------------------------------------------------

/// PTY 里运行的 rho：按键写入、屏幕断言、退出码。
struct PtyRho {
    child: Box<dyn Child + Send + Sync>,
    _master: Box<dyn MasterPty + Send>,
    raw: Arc<Mutex<Vec<u8>>>,
    writer: Box<dyn std::io::Write + Send>,
    /// 读线程持续推进 vt100 解析器（屏幕网格即最终状态断言依据）。
    screen: Arc<Mutex<Parser>>,
    killed: Arc<AtomicBool>,
}

impl PtyRho {
    /// 起一个挂 mock 配置的 rho（不依赖外部环境变量）。
    fn spawn(config_path: &str) -> Self {
        let pty = NativePtySystem::default()
            .openpty(PtySize { rows: ROWS, cols: COLS, ..Default::default() })
            .unwrap();
        let mut cmd = CommandBuilder::new(RHO_BIN);
        cmd.arg("--config");
        cmd.arg(config_path);
        if std::env::var("RHO_PTY_TRACE").is_ok() {
            cmd.env("RHO_PTY_TRACE", "1");
        }
        cmd.cwd(env!("CARGO_MANIFEST_DIR"));
        let child = pty.slave.spawn_command(cmd).unwrap();
        // 关键：测试进程持有的 slave 必须关掉，rho 退出后 EOF 才能传到 master。
        drop(pty.slave);

        let mut reader = pty.master.try_clone_reader().unwrap();
        let writer = pty.master.take_writer().unwrap();
        let screen = Arc::new(Mutex::new(Parser::new(
            std::num::NonZeroU16::new(ROWS).unwrap(),
            std::num::NonZeroU16::new(COLS).unwrap(),
            0,
        )));
        let raw: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let raw_thread = raw.clone();
        let screen_thread = screen.clone();
        let killed = Arc::new(AtomicBool::new(false));
        let killed_thread = killed.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // rho 退出：全部 slave fd 关闭后的 EOF
                    Ok(n) => {
                        if killed_thread.load(Ordering::SeqCst) {
                            break;
                        }
                        raw_thread.lock().unwrap().extend_from_slice(&buf[..n]);
                        screen_thread.lock().unwrap().process(&buf[..n]);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });
        PtyRho { child, _master: pty.master, writer, screen, raw, killed }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    /// 发送一行并回车。
    fn send_line(&mut self, line: &str) {
        self.send(format!("{line}\r").as_bytes());
    }

    /// 当前屏幕文本（整屏，去每行尾空白）。
    fn screen_text(&self) -> String {
        let parser = self.screen.lock().unwrap();
        parser
            .screen()
            .contents()
            .lines()
            .map(|l| l.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 诊断：最近输出的原始字节（可读转义形式）。
    fn raw_tail(&self, n: usize) -> String {
        let raw = self.raw.lock().unwrap();
        let start = raw.len().saturating_sub(n);
        String::from_utf8_lossy(&raw[start..])
            .replace('\x1b', "<ESC>")
            .replace('\r', "<CR>")
            .replace('\n', "<LF>\n")
    }

    /// 轮询等待屏幕包含 `needle`（vt100 网格，不受 ANSI 重绘影响）。
    fn expect_screen(&self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let text = self.screen_text();
            if text.contains(needle) {
                return;
            }
            if Instant::now() > deadline {
                panic!(
                    "屏幕未见 `{needle}`，超时 {timeout:?}。当前屏幕：\n{}\n原始输出尾部：\n{}",
                    text,
                    self.raw_tail(1500)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// 轮询等待屏幕不再包含 `needle`。
    fn expect_screen_absent(&self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let text = self.screen_text();
            if !text.contains(needle) {
                return;
            }
            if Instant::now() > deadline {
                panic!(
                    "屏幕仍含 `{needle}`，超时 {timeout:?}。当前屏幕：\n{}\n原始输出尾部：\n{}",
                    text,
                    self.raw_tail(1500)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// 有界等待 rho 退出，返回退出码。
    fn expect_exit(&mut self, timeout: Duration) -> u32 {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return status.exit_code();
            }
            if Instant::now() > deadline {
                panic!("rho 未在 {timeout:?} 内退出。当前屏幕：\n{}", self.screen_text());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for PtyRho {
    fn drop(&mut self) {
        // 兜底清理：测试失败/panic 也先停读线程再杀 rho；
        // master 由字段 drop 自动关闭（触发 SIGHUP）。
        self.killed.store(true, Ordering::SeqCst);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

#[test]
fn double_ctrl_c_at_empty_prompt_exits_zero() {
    let config = write_config("http://127.0.0.1:1");
    let mut rho = PtyRho::spawn(&config);

    rho.expect_screen("rho>", SCREEN_TIMEOUT);
    rho.send(b"\x03"); // 第一次：武装 + 灰提示
    rho.expect_screen("再按一次 Ctrl-C 退出", SCREEN_TIMEOUT);
    rho.send(b"\x03"); // 第二次（窗口内）：退出
    assert_eq!(rho.expect_exit(Duration::from_secs(5)), 0);
}

#[test]
fn single_ctrl_c_hint_expires_after_window() {
    let config = write_config("http://127.0.0.1:1");
    let mut rho = PtyRho::spawn(&config);

    rho.expect_screen("rho>", SCREEN_TIMEOUT);
    rho.send(b"\x03");
    rho.expect_screen("再按一次 Ctrl-C 退出", SCREEN_TIMEOUT);
    // 等窗口过期（含擦除余量）：灰提示被 rho-hint 线程擦掉。
    std::thread::sleep(CTRL_C_WINDOW + Duration::from_millis(800));
    rho.expect_screen_absent("再按一次 Ctrl-C 退出", SCREEN_TIMEOUT);
    // 过期后再按一次：重新武装，不退出。
    rho.send(b"\x03");
    rho.expect_screen("再按一次 Ctrl-C 退出", SCREEN_TIMEOUT);
    // 提交空行撤销武装；单次 Ctrl-C 不退出——用 /exit 收尾验证 rho 仍活着。
    rho.send(b"\r");
    rho.expect_screen_absent("再按一次 Ctrl-C 退出", SCREEN_TIMEOUT);
    std::thread::sleep(SETTLE);
    rho.send_line("/exit");
    assert_eq!(rho.expect_exit(Duration::from_secs(5)), 0);
}

#[test]
fn ctrl_c_while_typing_clears_line_and_disarms() {
    let config = write_config("http://127.0.0.1:1");
    let mut rho = PtyRho::spawn(&config);

    rho.expect_screen("rho>", SCREEN_TIMEOUT);
    // 空行武装 → 打字（清行+撤销）→ 再空行 Ctrl-C 只重新武装，不退出。
    rho.send(b"\x03");
    rho.expect_screen("再按一次 Ctrl-C 退出", SCREEN_TIMEOUT);
    rho.send(b"some typing");
    rho.send(b"\x03"); // 编辑中：清行、撤销武装
    rho.expect_screen_absent("再按一次 Ctrl-C 退出", SCREEN_TIMEOUT);
    std::thread::sleep(SETTLE);
    rho.send(b"\x03"); // 重新武装（非窗口内第二次）
    rho.expect_screen("再按一次 Ctrl-C 退出", SCREEN_TIMEOUT);
    std::thread::sleep(SETTLE);
    rho.send_line("/exit");
    assert_eq!(rho.expect_exit(Duration::from_secs(5)), 0);
}

#[test]
fn ctrl_c_after_typing_reclaims_prompt_rows() {
    let config = write_config("http://127.0.0.1:1");
    let mut rho = PtyRho::spawn(&config);

    rho.expect_screen("rho>", SCREEN_TIMEOUT);
    rho.send(b"long input that certainly wraps across the 80 column terminal width xxxxx");
    rho.send(b"\x03");
    rho.expect_screen_absent("certainly wraps", SCREEN_TIMEOUT);
    rho.expect_screen("rho>", SCREEN_TIMEOUT);
    // 稳定期后输入仍可用（避开回收后输入窗口的毫秒级竞态，见文件头注释）。
    std::thread::sleep(SETTLE);
    rho.send_line("/exit");
    assert_eq!(rho.expect_exit(Duration::from_secs(5)), 0);
}

/// 流式期间真实 SIGINT：on_sigint → self-pipe → 信号线程 → Cancel →
/// 取消本轮 → 回提示符。inject() 测试绕过这段，此处是唯一端到端覆盖。
#[test]
fn real_sigint_during_stream_cancels_and_returns_to_prompt() {
    let server = start_mock(slow_stream_response("partial", "late", Duration::from_secs(1)));
    let config = write_config(&server.base_url);
    let mut rho = PtyRho::spawn(&config);

    rho.expect_screen("rho>", SCREEN_TIMEOUT);
    rho.send_line("go");
    rho.expect_screen("partial", SCREEN_TIMEOUT);
    rho.send(b"\x03"); // 真实 SIGINT：信号处理器路径接管
    rho.expect_screen("已取消本轮响应", SCREEN_TIMEOUT);
    rho.expect_screen("rho>", SCREEN_TIMEOUT);
    std::thread::sleep(SETTLE);
    rho.send_line("/exit");
    assert_eq!(rho.expect_exit(Duration::from_secs(5)), 0);
}

/// 流式期间双按（第二次 Ctrl-C 为当轮第二次信号）：quit 收尾退出。
/// 时序约束严格：第二次必须落在第一次取消的收尾窗口（worker abandon 到回 REPL
/// 之间）；晚了会被 rustyline 按键路径吃掉（raw 模式下 \x03 是普通按键，不产生
/// 信号），早了两次会被合并统计。mock 慢流 + 密集两击覆盖该窗口。
#[test]
fn real_double_sigint_during_stream_quits() {
    let server = start_mock(slow_stream_response("partial", "late", Duration::from_secs(10)));
    let config = write_config(&server.base_url);
    let mut rho = PtyRho::spawn(&config);

    rho.expect_screen("rho>", SCREEN_TIMEOUT);
    rho.send_line("go");
    rho.expect_screen("partial", SCREEN_TIMEOUT);
    // 轮询节奏 100ms：第一次 SIGINT 被记录后，主线程在下个轮询周期发现并收尾；
    // 第二击须在此之前到达信号线程（锁内追加为第 2 条记录 → quit）。
    // 50ms 间隔 < 轮询周期，保证第二击落在取消处理前。
    rho.send(b"\x03");
    std::thread::sleep(Duration::from_millis(50));
    rho.send(b"\x03");
    assert_eq!(rho.expect_exit(Duration::from_secs(5)), 0);
}

/// 确认界面的 Ctrl-C：可中断读取被 SIGINT 唤醒，取消整轮回提示符。
#[test]
fn real_sigint_at_confirm_cancels_turn() {
    let server = start_mock(tool_call_response("sleep 5"));
    let config = write_config(&server.base_url);
    let mut rho = PtyRho::spawn(&config);

    rho.expect_screen("rho>", SCREEN_TIMEOUT);
    rho.send_line("go");
    rho.expect_screen("执行？[y/N]", SCREEN_TIMEOUT);
    // 确认界面在 cooked 模式：Ctrl-C 产生 SIGINT，read 返回 EINTR → Cancelled；
    // 当前及剩余调用补取消占位（占位文本与流式取消的文案不同）。
    rho.send(b"\x03");
    rho.expect_screen("用户取消执行", SCREEN_TIMEOUT);
    rho.expect_screen("rho>", SCREEN_TIMEOUT);
    std::thread::sleep(SETTLE);
    rho.send_line("/exit");
    assert_eq!(rho.expect_exit(Duration::from_secs(5)), 0);
}

/// bash 超时在真实终端的表现：超时标注 + 进程组终止后回提示符。
#[test]
fn bash_timeout_shows_annotation_and_returns_to_prompt() {
    let server = start_mock(tool_call_response("sleep 30"));
    let config = write_config(&server.base_url);
    let mut rho = PtyRho::spawn(&config);

    rho.expect_screen("rho>", SCREEN_TIMEOUT);
    rho.send_line("go");
    rho.expect_screen("执行？[y/N]", SCREEN_TIMEOUT);
    rho.send_line("y");
    // timeout_seconds = 2：SIGTERM→2s→SIGKILL，总等待 ≤ ~5s。
    rho.expect_screen("已部分执行（超时）", Duration::from_secs(8));
    // 超时结果喂回后继续请求（mock 单连接已关：第二次请求失败）——回提示符即收尾。
    rho.expect_screen("rho>", Duration::from_secs(8));
    std::thread::sleep(SETTLE);
    rho.send_line("/exit");
    assert_eq!(rho.expect_exit(Duration::from_secs(5)), 0);
}
