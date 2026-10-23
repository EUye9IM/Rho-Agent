//! 终端信号处理：SIGINT self-pipe、专用信号线程、Cancel（epoch 记录）。
//! 见 architecture.md 五节。

use std::sync::{Arc, Mutex};

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
#[derive(Debug, PartialEq)]
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
        // TODO: 由 term::init 创建
        todo!()
    }

    /// 主线程查询当前信号状态（渲染循环逐事件查、工具等待循环轮询、agent loop 收尾查）。
    pub fn state(&self) -> SignalState {
        // TODO: 锁内：prev_quit → Quit；records 空为 None；
        //       len==1 → Cancel；len>=2 → Quit
        todo!()
    }

    /// 切轮（每次用户输入触发的新回合开始前调用）。
    /// 返回 prev_quit：true 表示旧轮遗留退出请求，应退出程序。
    pub fn advance_epoch(&self) -> bool {
        // TODO: 锁内：epoch+=1；records 长度 >=2 → prev_quit=true；
        //       清空 records；返回（含原）prev_quit
        todo!()
    }

    /// 测试/集成注入：直接追加一次 Ctrl-C 记录（等效信号线程；tests/ 集成可用，
    /// 非 cfg(test)——单元测试与集成测试都需要它驱动取消/退出路径）。
    pub fn inject(&self) {
        // TODO: 锁内 records.push(len+1)
        todo!()
    }
}

/// 启动信号基础设施：装 SIGINT 处理器（self-pipe）+ 起专用信号线程。
/// 返回 Cancel 的 Arc。失败 = exit 1 级内部错误。
pub fn init() -> Result<Arc<Cancel>, String> {
    // TODO: pipe2(O_NONBLOCK)；sigaction SIGINT → 处理器写 1 字节（不取锁）
    // TODO: spawn 专用线程：阻塞读 self-pipe → 每字节锁内追加一条记录
    // TODO: 信号线程与 advance_epoch 在同一锁内串行化（结构上已保证）
    todo!()
}

/// 对整个进程组发信号的辅助（tools.rs 复用）。
/// kill 语义：SIGTERM → 2s → 无论直接子进程状态，对 pgid 发 SIGKILL。
pub fn kill_pg(pgid: i32, sig: libc::c_int) -> Result<(), String> {
    // TODO: libc::kill(-pgid, sig)
    let _ = (pgid, sig);
    todo!()
}
