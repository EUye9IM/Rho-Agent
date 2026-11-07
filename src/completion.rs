//! 补全协议：与终端无关的候选类型和提供者 trait。
//! `ui` 只负责把候选适配到 rustyline；命令语义与候选来源留在 `commands.rs`。
//! 提供者由 `repl` 在每次 `read_line` 前按当前会话生成，UI 不持久保存补全状态。

/// 一个补全候选。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// 候选列表中展示的文本（可带当前值标记或说明）。
    pub display: String,
    /// 选中后写回输入行的文本。
    pub replacement: String,
}

/// 一次补全请求的结果：从 `start` 开始替换当前输入。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completion {
    pub start: usize,
    pub candidates: Vec<Candidate>,
}

impl Completion {
    pub fn none(start: usize) -> Self {
        Self {
            start,
            candidates: Vec::new(),
        }
    }
}

/// 按当前输入行提供补全候选。
pub trait CompletionProvider {
    fn complete(&self, line: &str, pos: usize) -> Completion;
}

/// 没有候选的默认实现：`TermUi` 初始化 helper 时使用。
pub struct NoCompletion;

impl CompletionProvider for NoCompletion {
    fn complete(&self, _line: &str, pos: usize) -> Completion {
        Completion::none(pos)
    }
}
