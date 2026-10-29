//! Ctrl-C 双按灰提示子系统：武装窗口常量、Hinter/高亮、期限线程与到期擦除。
//! 由 `TermUi`（ui 模块）组装；只负责提示的显示与到期擦除，行回收由 TermUi 处理。

use crate::render;
use std::borrow::Cow;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rustyline::completion::Completer;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper};

/// 提示符下 Ctrl-C 双按窗口：两次空行 Ctrl-C 间隔小于它才退出。
/// `repl` 用同一个常量判定退出，`TermUi` 用它到期撤掉灰提示。
pub const CTRL_C_WINDOW: Duration = Duration::from_secs(2);

/// 第一次空行 Ctrl-C 后在提示符行尾显示的灰色提示。
pub(super) const CTRL_C_HINT: &str = "再按一次 Ctrl-C 退出";

/// 到期后是否该擦掉灰提示：只有提示确实画在一个空输入行上、且终端在本次
/// `readline` 内时才动手（否则可能擦掉别人的行）。
fn should_erase_hint(out_tty: bool, reading: bool, line_empty: bool) -> bool {
    out_tty && reading && line_empty
}

/// Ctrl-C 灰提示的共享状态：主线程武装/撤销，期限线程到期擦掉提示。
pub(super) struct HintArm {
    armed: AtomicBool,
    /// 当前提示行是否为空（`Hinter` 每次重绘时更新）。
    line_empty: AtomicBool,
    /// 灰提示是否包含在最后一次 `refresh` 的布局里（`Hinter` 每次重绘时更新）。
    /// 语义是“布局”而非“像素”：到期擦除只动屏幕、不动 rustyline 缓存的 layout，
    /// 下次 `Cmd::Interrupt` 的 `move_cursor_to_end` 仍会把光标移到含提示的 `layout.end`，
    /// 所以不能因到期擦除而清零，否则回收会少上移一行。
    layout_hint: AtomicBool,
    /// 是否处于 `readline` 期间：期限线程只在此时动终端。
    reading: AtomicBool,
    /// stdout 是否 TTY：非 TTY 时不写光标控制序列。
    out_tty: bool,
    /// 终止标志：`Drop` 时置位，让期限线程退出。
    stop: AtomicBool,
    deadline: Mutex<Option<Instant>>,
    ready: Condvar,
}

impl HintArm {
    pub(super) fn new(out_tty: bool) -> Self {
        Self {
            armed: AtomicBool::new(false),
            line_empty: AtomicBool::new(false),
            layout_hint: AtomicBool::new(false),
            reading: AtomicBool::new(false),
            out_tty,
            stop: AtomicBool::new(false),
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
        self.ready
            .wait_timeout(guard, dur)
            .unwrap_or_else(|e| e.into_inner())
            .0
    }

    pub(super) fn arm(&self, window: Duration) {
        let mut deadline = self.lock();
        *deadline = Some(Instant::now() + window);
        self.armed.store(true, Ordering::SeqCst);
        drop(deadline);
        self.ready.notify_all();
    }

    pub(super) fn disarm(&self) {
        let mut deadline = self.lock();
        *deadline = None;
        self.armed.store(false, Ordering::SeqCst);
        drop(deadline);
        self.ready.notify_all();
    }

    fn armed(&self) -> bool {
        self.armed.load(Ordering::SeqCst)
    }

    /// `TermUi::read_line` 前后切换：期限线程只在 `readline` 期间动终端。
    pub(super) fn set_reading(&self, reading: bool) {
        self.reading.store(reading, Ordering::SeqCst);
    }

    /// 通知期限线程退出（在锁内置位并唤醒，避免丢失通知）。
    pub(super) fn stop(&self) {
        let _guard = self.lock();
        self.stop.store(true, Ordering::SeqCst);
        self.ready.notify_all();
    }

    /// 到期是否该擦提示（判定见 `should_erase_hint`）。
    fn can_erase(&self) -> bool {
        should_erase_hint(
            self.out_tty,
            self.reading.load(Ordering::SeqCst),
            self.line_empty.load(Ordering::SeqCst),
        )
    }

    /// 最后一次 `refresh` 的布局里是否包含灰提示（由 `Hinter` 每次重绘时更新）。
    /// `refresh_line_with_msg`（空行回车 / Ctrl-D）不调 `hint()`，标志可能短时残留；
    /// 但 `reclaim_width` 只在 `Interrupted` 里快照，那时本次 `readline` 的首个
    /// `refresh_line` 已经重算过一遍，所以不会用到残留值。
    fn mark_layout_hint(&self, shown: bool) {
        self.layout_hint.store(shown, Ordering::SeqCst);
    }

    pub(super) fn take_layout_hint(&self) -> bool {
        self.layout_hint.swap(false, Ordering::SeqCst)
    }

    /// 擦掉光标右侧的灰提示。不移动光标，因此 rustyline 的位置模型（提示符末尾、
    /// 输入行为空）保持不变，下一次重绘自然接上。
    /// 用“擦到屏幕末尾”而不是 `\x1b[K`：窄终端下提示会换到下一行，光标却在提示符行，
    /// 只擦当前行会留下下一行的尾巴。
    fn erase_hint(&self) {
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[J").and_then(|()| out.flush());
    }
}

/// rustyline helper：只在武装窗口内的空行上给出灰色提示。
pub(super) struct HintHelper {
    hint: Arc<HintArm>,
}

impl HintHelper {
    pub(super) fn new(hint: Arc<HintArm>) -> Self {
        Self { hint }
    }
}

impl Completer for HintHelper {
    type Candidate = String;
}

impl Hinter for HintHelper {
    type Hint = String;

    fn hint(&self, line: &str, _pos: usize, _ctx: &Context<'_>) -> Option<String> {
        let empty = line.is_empty();
        let show = empty && self.hint.armed();
        self.hint.line_empty.store(empty, Ordering::SeqCst);
        self.hint.mark_layout_hint(show);
        show.then(|| CTRL_C_HINT.to_string())
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
/// 所以这里只从光标位置擦到屏幕末尾：不移动光标，与 rustyline 的位置模型不冲突
/// （到期时光标在提示符行末尾，提示符块是最后一段输出）。
/// 这是尽力而为的擦除：`can_erase` 判定与写入之间主线程可能已经开始重绘，两个方向
/// （没擦干净 / 擦掉了刚画出的新提示）都可能发生，下一次按键重绘会自愈。
pub(super) fn hint_timer(arm: Arc<HintArm>) {
    let mut guard = arm.lock();
    loop {
        if arm.stop.load(Ordering::SeqCst) {
            return;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_erase_needs_tty_readline_and_empty_line() {
        assert!(should_erase_hint(true, true, true));
        assert!(!should_erase_hint(false, true, true));
        assert!(!should_erase_hint(true, false, true));
        assert!(!should_erase_hint(true, true, false));
    }
}
