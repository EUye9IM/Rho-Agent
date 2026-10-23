//! bash 工具执行：进程组 spawn、超时升级、双 drain 管道、两段截断、
//! allowlist + 元字符判定。见 design.md 七节、architecture.md 五节。

use crate::config::BashToolConfig;
use crate::term::Cancel;
use crate::ui::{ConfirmOutcome, Ui};

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
    /// 正常退出（exit code 可能为非零，非零不算错误）
    Exited(i32),
    /// 超时（已 SIGTERM→2s→SIGKILL 整个进程组）
    TimedOut,
    /// 被 Ctrl-C 取消（副作用已发生）
    Cancelled,
    /// spawn 失败（命令不存在属 bash 127，不是这里；此处是 fork/exec 层失败）
    SpawnFailed(String),
    /// quit 已请求：工具等待循环只终止进程组，把 quit 上报 agent loop，
    /// 不自行退出（architecture.md 五节）
    QuitRequested,
}

/// allowlist + 元字符判定：命令与规则完全相同，或前缀后紧接空格；
/// 且不含 `; & | < > $`、反引号、换行。
pub fn allowed(command: &str, allow: &[String]) -> bool {
    // TODO: 元字符黑名单检查；任一命中即 false
    // TODO: allowlist 前缀匹配（完全相同或前缀+空格）
    let _ = (command, allow);
    todo!()
}

/// 执行 `bash -c <command>`（前台，工作目录恒为会话 cwd = 进程启动目录）。
///
/// 线程模型（architecture.md 五节）：
/// - setsid 建进程组；stdout/stderr 各一个 drain 线程，两段截断缓冲
/// - 主线程是唯一 reaper（waitpid 只在此处）；退出状态存共享标志
/// - drain 观察到退出 → 有界收尾读取（短超时）→ 自行退出并关自己的读端
/// - 超时/取消升级：SIGTERM 发 pgid → 2s → 无论子进程状态对 pgid 发 SIGKILL → 回收
/// - cancel 的 quit（第二次 Ctrl-C）：只终止进程组并返回给 agent loop，不自行退出
pub fn run_bash(command: &str, tool_cfg: &BashToolConfig, cancel: &Cancel) -> BashOutput {
    // TODO: spawn（stdin=/dev/null，env 完全继承）
    // TODO: 起 drain 线程（两段截断：头部前缀 + 滑动尾部窗口，中间计数丢弃）
    // TODO: 主线程 waitpid 轮询：超时检查 + cancel 检查
    // TODO: 组装 BashOutput（含截断标注与退出码行；信号终止时无整数退出码）
    let _ = (command, tool_cfg, cancel);
    todo!()
}

/// 确认流（agent loop 的 dispatch_tool_call 调用）：
/// allowlist 自动放行；否则向用户确认（--yes 跳过）。
pub fn gate(
    ui: &mut dyn Ui,
    yes: bool,
    cwd: &str,
    command: &str,
    allow: &[String],
) -> ConfirmOutcome {
    // TODO: allowed() → Yes；yes → Yes；否则 ui.confirm(cwd, command)
    let _ = (ui, yes, cwd, command, allow);
    todo!()
}

/// 两段截断：头 limit/2 + 尾 limit/2，中间标注省略字节数。
pub fn truncate_output(bytes: &[u8], limit: usize) -> String {
    // TODO: len <= limit 原样（UTF-8 边界安全截断）；
    //       否则头前缀 + "…省略 N 字节…" + 尾窗口
    let _ = (bytes, limit);
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn metachar_rejected() {
        assert!(!allowed("ls; rm -rf /", &["ls".into()]));
        assert!(!allowed("ls | cat", &["ls".into()]));
        assert!(!allowed("ls\nrm", &["ls".into()]));
    }

    #[test]
    fn truncate_keeps_head_and_tail() {
        let data = vec![b'a'; 100];
        let out = truncate_output(&data, 20);
        assert!(out.contains("省略"));
    }
}
