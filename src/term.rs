//! 终端信号处理：SIGINT self-pipe、专用信号线程、Cancel（epoch 记录）。
//! 见 docs/architecture.md 五节。

use std::io::{ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// 信号处理器写入端 fd（async-signal-safe 路径只碰这个原子量与 write(2)）。
static WRITE_FD: AtomicI32 = AtomicI32::new(-1);
/// 写端本体：进程生命周期内常驻，保证 [`WRITE_FD`] 指向的 fd 不被关闭。
static WRITE_END: OnceLock<UnixStream> = OnceLock::new();
static CANCEL: OnceLock<Arc<Cancel>> = OnceLock::new();

/// Ctrl-C 记录中心（活动操作期间的唯一记录消费入口）。
///
/// 并发模型：
/// - 真正的信号处理器只向 self-pipe 写一个字节（async-signal-safe，不取锁）
/// - 专用信号线程消费 self-pipe，在锁内做 epoch 读取 + 记录追加
/// - 主线程切轮（advance_epoch）也在锁内；旧轮未消费的 quit 结转 prev_quit
/// - REPL 输入态的 Ctrl-C 走 rustyline 按键路径，不经此记录
pub struct Cancel {
    inner: Mutex<State>,
}

struct State {
    epoch: u64,
    /// 当前轮的第几次 Ctrl-C（1 = cancel，≥2 = quit）
    records: Vec<u32>,
    /// 旧轮未消费的 quit 结转；查询时先看它
    prev_quit: bool,
}

/// 信号路径查询结果。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SignalState {
    /// 无 Ctrl-C（或旧轮非 quit 记录已作废）
    None,
    /// 当前轮第 1 次：取消当前操作
    Cancel,
    /// 当前轮第 2 次及以上（或 prev_quit）：退出程序
    Quit,
}

impl Cancel {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(State { epoch: 0, records: Vec::new(), prev_quit: false }),
        })
    }

    /// 主线程查询当前信号状态（渲染循环逐事件查、工具等待循环轮询、agent loop 收尾查）。
    pub fn state(&self) -> SignalState {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if state.prev_quit {
            return SignalState::Quit;
        }
        match state.records.len() {
            0 => SignalState::None,
            1 => SignalState::Cancel,
            _ => SignalState::Quit,
        }
    }

    /// 便捷查询：有未消费的 Ctrl-C 记录时返回 Some(Cancel/Quit)，无信号返回 None。
    /// 用于“无信号继续，否则按取消/退出收尾”的判定点（渲染循环、工具等待循环）。
    pub fn pending(&self) -> Option<SignalState> {
        match self.state() {
            SignalState::None => None,
            sig => Some(sig),
        }
    }

    /// 便捷判定：当前轮是否已请求退出（含旧轮结转的 quit）。
    pub fn is_quit(&self) -> bool {
        matches!(self.state(), SignalState::Quit)
    }

    /// 切轮（每次用户输入触发的新回合开始前调用）。
    /// 返回 prev_quit：true 表示旧轮遗留退出请求，应退出程序。
    pub fn advance_epoch(&self) -> bool {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.epoch = state.epoch.wrapping_add(1);
        let quit = state.prev_quit || state.records.len() >= 2;
        state.prev_quit = quit;
        state.records.clear();
        quit
    }

    /// 测试/集成注入：直接追加一次 Ctrl-C 记录（等效信号线程；tests/ 集成可用，
    /// 非 cfg(test)——单元测试与集成测试都需要它驱动取消/退出路径）。
    pub fn inject(&self) -> u32 {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let n = state.records.len() as u32 + 1;
        state.records.push(n);
        n
    }
}

/// 信号处理器：只写一个字节，不取锁、不分配。
extern "C" fn on_sigint(_sig: libc::c_int) {
    let fd = WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = [1u8];
        // SAFETY: fd 在进程生命周期内保持有效；write 是 async-signal-safe 的。
        unsafe {
            let _ = libc::write(fd, byte.as_ptr().cast(), 1);
        }
    }
}

fn last_os_error() -> String {
    std::io::Error::last_os_error().to_string()
}

/// 启动信号基础设施：装 SIGINT 处理器（self-pipe）+ 起专用信号线程。
/// 返回 Cancel 的 Arc。失败 = exit 1 级内部错误。
pub fn init() -> Result<Arc<Cancel>, String> {
    if let Some(cancel) = CANCEL.get() {
        return Ok(cancel.clone());
    }

    // self-pipe 用 socketpair：std 的 `UnixStream` 自带 CLOEXEC，并提供非阻塞开关与
    // 基于 `Read` 的阻塞读取，省去 pipe2/fcntl/close 的裸 fd 管理。
    let (read_end, write_end) =
        UnixStream::pair().map_err(|e| format!("创建信号管道失败：{e}"))?;
    // 写端非阻塞：管道满时处理器直接丢弃（不能阻塞在信号上下文）。
    write_end
        .set_nonblocking(true)
        .map_err(|e| format!("设置信号管道非阻塞失败：{e}"))?;
    let write_fd = write_end.as_raw_fd();
    WRITE_END
        .set(write_end)
        .map_err(|_| "信号管道初始化了两次".to_string())?;
    WRITE_FD.store(write_fd, Ordering::SeqCst);

    // SAFETY: 零初始化 sigaction 后逐字段赋值；handler 为 extern "C"。
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_sigint as *const () as libc::sighandler_t;
        // 不设 SA_RESTART：确认界面的 read 需要被 SIGINT 打断返回 EINTR。
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) == -1 {
            WRITE_FD.store(-1, Ordering::SeqCst);
            return Err(format!("安装 SIGINT 处理器失败：{}", last_os_error()));
        }
    }

    let cancel = Cancel::new();
    let thread_cancel = cancel.clone();
    let spawned = std::thread::Builder::new()
        .name("rho-sigint".into())
        .spawn(move || {
            let mut read_end = read_end;
            let mut buf = [0u8; 64];
            loop {
                match read_end.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        for _ in 0..n {
                            thread_cancel.inject();
                        }
                    }
                    // 信号可能投递到本线程：EINTR 不是管道关闭，继续读。
                    Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            // 读端由 read_end 的 Drop 关闭。
        });
    if let Err(e) = spawned {
        // 处理器已装但无人消费：撤销写端指向，避免处理器写入无读者的管道。
        WRITE_FD.store(-1, Ordering::SeqCst);
        return Err(format!("启动信号线程失败：{e}"));
    }

    match CANCEL.set(cancel.clone()) {
        Ok(()) => Ok(cancel),
        Err(_) => Ok(CANCEL.get().expect("signal cancel initialized").clone()),
    }
}

/// 对整个进程组发信号的辅助（tools.rs 复用）。
/// kill 语义：SIGTERM → 2s → 无论直接子进程状态，对 pgid 发 SIGKILL。
pub fn kill_pg(pgid: i32, sig: libc::c_int) -> Result<(), String> {
    if pgid <= 0 {
        return Err(format!("非法的进程组 id：{pgid}"));
    }
    // SAFETY: 负 pid 表示向进程组发信号。
    if unsafe { libc::kill(-pgid, sig) } == -1 {
        let err = std::io::Error::last_os_error();
        // 进程组已不存在不算错误。
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(format!("向进程组 {pgid} 发信号失败：{err}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_is_cancel_second_is_quit() {
        let c = Cancel::new();
        assert_eq!(c.state(), SignalState::None);
        c.inject();
        assert_eq!(c.state(), SignalState::Cancel);
        c.inject();
        assert_eq!(c.state(), SignalState::Quit);
    }

    #[test]
    fn advance_carries_unconsumed_quit() {
        let c = Cancel::new();
        c.inject();
        c.inject();
        assert!(c.advance_epoch());
        assert_eq!(c.state(), SignalState::Quit);
    }

    #[test]
    fn advance_discards_single_cancel() {
        let c = Cancel::new();
        c.inject();
        assert!(!c.advance_epoch());
        assert_eq!(c.state(), SignalState::None);
    }
}
