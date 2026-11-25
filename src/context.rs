//! 上下文压缩：切点规划、压缩请求的预算裁剪与内置摘要指令。
//!
//! 压缩由 `/compact` 手动触发：把较早的若干轮换成一条 `Msg::Summary`。
//! 发给模型的请求是真实历史前缀的副本 + 末尾一条 user 指令，其余（tools、
//! 输出上限、协议映射）与普通回合完全一致，只是收流时不渲染增量。

use crate::message::Msg;

/// 内置摘要指令：与 `src/config/default.toml` 的 `[compact].prompt` 保持一致
/// （`tests` 里的 `builtin_prompt_matches_constant` 是这条同步的护栏）。
pub const DEFAULT_COMPACT_PROMPT: &str = r#"你现在作为这个 AI 编程助手的压缩引擎。将上面的对话浓缩成一个结构化检查点，使另一个模型能够在不丢失关键上下文的情况下继续工作。

严格输出下面的 Markdown 结构：保留每个部分，并按顺序排列。使用简洁的项目符号，而不是散文段落。空部分写"(none)"——绝不要删除任何部分。

## 主要请求与意图
- [用户的原始目标和不断演变的目标；在确切措辞很重要时逐字引用]

## 关键技术概念
- [涉及的技术、框架、模式和约定]

## 文件与代码
- [确切路径：为什么重要，关键改动或代码片段]

## 错误与修复
- [错误：如何解决的，以及相关的用户反馈]

## 待办任务
- [已明确请求但尚未完成的工作]

## 当前工作
- [在此检查点时，精确说明正在进行的工作]

## 下一步
- [与最近请求直接一致的单一下一步行动，或"(none)"]

## 关键上下文
- [决策及其理由、约束、用户偏好、未决问题、继续所需的数据]

规则：
- 用简洁的中文工程性语言书写。保留确切的文件路径、命令、错误字符串、标识符、数值、函数签名和语法片段。
- 忠实记录用户反馈和明确指令，尤其是纠正意见。
- 不要提及本次摘要请求，也不要提及上下文已被压缩。
- 只输出检查点文本：不要调用任何工具或采取任何其他操作。
- 如果上面已经包含一个先前的检查点（上一轮的压缩摘要，以系统消息给出），不要逐字照搬：保留仍然成立的事实，丢弃过时信息，把较新的信息合并进同一结构下的单一汇总摘要。"#;

/// `[compact] keep_turns` 省略时保留的轮数。
pub const DEFAULT_KEEP_TURNS: usize = 2;

/// 压缩请求里单条工具结果的字符上限。
const TOOL_RESULT_CHARS: usize = 500;
/// 压缩请求里单条 assistant 正文的字符上限。
const ASSISTANT_TEXT_CHARS: usize = 2000;
/// 预算之外再留的余量（与 [`estimate_tokens`] 同一口径）。
const BUDGET_MARGIN: u64 = 1024;
/// 预算下限：窗口小于输出上限加余量时仍给一个可用值。
const MIN_BUDGET: u64 = 1024;

/// 一次压缩的切点与请求副本。
#[derive(Debug)]
pub struct Plan {
    /// 历史中被摘要替换的起始下标（system 之后）。
    pub start: usize,
    /// 历史中被摘要替换的结束下标（不含）。
    pub end: usize,
    /// 发给模型的副本：已按预算裁剪，调用方在末尾追加指令。
    pub head: Vec<Msg>,
}

impl Plan {
    /// 被替换掉的消息条数。
    pub fn dropped(&self) -> usize {
        self.end - self.start
    }
}

/// 规划一次压缩：保留最近 `keep_turns` 轮，前缀进入请求副本。
///
/// 切点只落在 `Msg::User` 之前，被替换的头部不可能拆开 `Assistant(tool_calls)`
/// 与其后的 `Tool` 消息。返回 `None` 表示没有可压缩的轮次：保留轮数覆盖了全部
/// 历史，或前缀里除了上一份摘要没有新的对话。
pub fn plan(history: &[Msg], keep_turns: usize, window: u64, max_output: u64) -> Option<Plan> {
    let keep = keep_turns.max(1);
    // system 永远不参与压缩；没有 system_prompt 时会话从 User 开始。
    let start = match history.first() {
        Some(Msg::System(_)) => 1,
        _ => 0,
    };
    let users: Vec<usize> = history
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m, Msg::User(_)))
        .map(|(i, _)| i)
        .collect();
    if users.len() <= keep {
        return None;
    }
    let end = users[users.len() - keep];
    if end <= start {
        return None;
    }

    let mut head: Vec<Msg> = history[start..end].to_vec();
    // 上一份摘要单独存在、后面没有新对话时不必再压一次。
    if !head.iter().any(|m| matches!(m, Msg::User(_))) {
        return None;
    }
    shrink(&mut head);
    fit_budget(&mut head, budget(window, max_output));
    Some(Plan { start, end, head })
}

/// 压缩请求可用的 token 预算：扣掉输出上限与余量，留一个下限。
fn budget(window: u64, max_output: u64) -> u64 {
    window
        .saturating_sub(max_output)
        .saturating_sub(BUDGET_MARGIN)
        .max(MIN_BUDGET)
}

/// 逐条裁剪请求副本：工具结果与 assistant 正文按字符上限截断，思维链丢弃。
fn shrink(head: &mut [Msg]) {
    for msg in head.iter_mut() {
        match msg {
            Msg::Tool { content, .. } => {
                *content = crate::text::truncate_chars(content, TOOL_RESULT_CHARS);
            }
            Msg::Assistant(assistant) => {
                assistant.reasoning = None;
                if let Some(text) = &assistant.text {
                    assistant.text = Some(crate::text::truncate_chars(text, ASSISTANT_TEXT_CHARS));
                }
            }
            _ => {}
        }
    }
}

/// 逐条裁剪后仍超预算时，从最旧的一轮开始整轮丢弃；开头的上一份摘要留到最后。
/// 只剩最后一轮时停止：再丢就没有可摘要的对话了。
fn fit_budget(head: &mut Vec<Msg>, budget: u64) {
    let from = match head.first() {
        Some(Msg::Summary(_)) => 1,
        _ => 0,
    };
    while estimate_tokens(head) > budget {
        let users: Vec<usize> = head
            .iter()
            .enumerate()
            .skip(from)
            .filter(|(_, m)| matches!(m, Msg::User(_)))
            .map(|(i, _)| i)
            .collect();
        if users.len() <= 1 {
            return;
        }
        head.drain(from..users[1]);
    }
}

/// 粗估一段历史的 token 数：ASCII 4 字符 ≈ 1 token，非 ASCII 1 字符 ≈ 1 token，
/// 每条消息再计 4。中文因此偏保守（DeepSeek 实际约 0.6 token/字），
/// 压缩请求宁可少塞一点，也不要撞上窗口上限。
pub fn estimate_tokens(msgs: &[Msg]) -> u64 {
    msgs.iter().map(message_tokens).sum()
}

/// 单段文本的 token 估算（摘要正文的规模提示用）。
pub fn estimate_text(text: &str) -> u64 {
    text_tokens(text)
}

fn message_tokens(msg: &Msg) -> u64 {
    4 + match msg {
        Msg::System(text) | Msg::User(text) | Msg::Summary(text) => text_tokens(text),
        Msg::Assistant(assistant) => {
            assistant.text.as_deref().map_or(0, text_tokens)
                + assistant.reasoning.as_deref().map_or(0, text_tokens)
                + assistant
                    .tool_calls
                    .iter()
                    .map(|call| 4 + text_tokens(&call.name) + text_tokens(&call.arguments))
                    .sum::<u64>()
        }
        Msg::Tool { content, .. } => text_tokens(content),
    }
}

fn text_tokens(text: &str) -> u64 {
    let mut ascii = 0u64;
    let mut wide = 0u64;
    for c in text.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            wide += 1;
        }
    }
    ascii.div_ceil(4) + wide
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::AssistantMsg;

    fn user(text: &str) -> Msg {
        Msg::User(text.into())
    }

    fn assistant(text: &str) -> Msg {
        Msg::Assistant(AssistantMsg {
            text: Some(text.into()),
            reasoning: None,
            tool_calls: vec![],
        })
    }

    fn tool(content: &str) -> Msg {
        Msg::Tool {
            tool_call_id: "c1".into(),
            name: "bash".into(),
            content: content.into(),
            display: Some(content.into()),
            status: crate::message::ToolStatus::Success,
        }
    }

    /// 5 轮：每轮 User + Assistant。
    fn five_turns() -> Vec<Msg> {
        let mut history = vec![Msg::System("sys".into())];
        for i in 1..=5 {
            history.push(user(&format!("u{i}")));
            history.push(assistant(&format!("a{i}")));
        }
        history
    }

    #[test]
    fn plan_keeps_recent_turns_and_drops_the_rest() {
        let history = five_turns();
        let plan = plan(&history, 2, 1_000_000, 1_000).unwrap();
        assert_eq!(plan.start, 1);
        assert_eq!(plan.end, 7); // 第 3 轮（u3）之前
        assert_eq!(plan.dropped(), 6);
        assert_eq!(plan.head.len(), 6);
        assert!(matches!(&plan.head[0], Msg::User(u) if u == "u1"));
        assert!(matches!(&plan.head[5], Msg::Assistant(a) if a.text.as_deref() == Some("a3")));
        // 尾部两轮（u4、u5）原样留在历史里。
        assert!(matches!(&history[7], Msg::User(u) if u == "u4"));
    }

    #[test]
    fn plan_needs_at_least_one_turn_beyond_keep() {
        let history = five_turns();
        assert!(plan(&history, 5, 1_000_000, 1_000).is_none());
        assert!(plan(&history, 99, 1_000_000, 1_000).is_none());
        assert!(plan(&history, 1, 1_000_000, 1_000).is_some());
    }

    #[test]
    fn plan_skips_history_without_system_message() {
        let mut history = five_turns();
        history.remove(0);
        let plan = plan(&history, 2, 1_000_000, 1_000).unwrap();
        assert_eq!(plan.start, 0);
        assert_eq!(plan.end, 6);
        assert!(matches!(&plan.head[0], Msg::User(u) if u == "u1"));
    }

    #[test]
    fn plan_refuses_when_only_a_previous_summary_can_move() {
        // 压缩后：摘要 + 保留的两轮；再压一次没有新对话可摘要。
        let history = vec![
            Msg::System("sys".into()),
            Msg::Summary("checkpoint".into()),
            user("u4"),
            assistant("a4"),
            user("u5"),
            assistant("a5"),
        ];
        assert!(plan(&history, 2, 1_000_000, 1_000).is_none());
        // 再来一轮后，摘要与新的一轮一起被替换。
        let mut history = history;
        history.push(user("u6"));
        history.push(assistant("a6"));
        let plan = plan(&history, 2, 1_000_000, 1_000).unwrap();
        assert_eq!(plan.start, 1);
        assert_eq!(plan.end, 4); // 保留 u5、u6 两轮
        assert!(matches!(&plan.head[0], Msg::Summary(_)));
    }

    #[test]
    fn shrink_truncates_tool_results_and_drops_reasoning() {
        let long_tool = "x".repeat(2_000);
        let long_text = "y".repeat(5_000);
        let history = vec![
            Msg::System("sys".into()),
            user("u1"),
            Msg::Assistant(AssistantMsg {
                text: Some(long_text.clone()),
                reasoning: Some("thinking".into()),
                tool_calls: vec![],
            }),
            tool(&long_tool),
            user("u2"),
            assistant("a2"),
            user("u3"),
            assistant("a3"),
        ];
        let plan = plan(&history, 2, 1_000_000, 1_000).unwrap();
        match &plan.head[1] {
            Msg::Assistant(a) => {
                assert!(a.reasoning.is_none());
                let text = a.text.as_deref().unwrap();
                assert_eq!(text.chars().count(), ASSISTANT_TEXT_CHARS + 1); // 含省略号
            }
            other => panic!("unexpected {other:?}"),
        }
        match &plan.head[2] {
            Msg::Tool { content, .. } => {
                assert_eq!(content.chars().count(), TOOL_RESULT_CHARS + 1);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn budget_trimming_drops_oldest_turns_first() {
        // 窗口极小 + 每轮正文接近逐条截断上限：只有丢掉早期轮次才塞得下。
        let mut history = vec![Msg::System("sys".into())];
        let long = "y".repeat(2_000);
        for i in 1..=20 {
            history.push(user(&format!("u{i}")));
            history.push(assistant(&long));
        }
        let plan = plan(&history, 2, 1_000, 100).unwrap();
        let kept_users = plan
            .head
            .iter()
            .filter(|m| matches!(m, Msg::User(_)))
            .count();
        // 至少留下最旧的一轮（再丢就没内容可摘要了），且比全部轮数少。
        assert!(kept_users >= 1);
        assert!(kept_users < 18);
        // 留下的永远是最新的那几轮。
        assert!(matches!(&plan.head[0], Msg::User(u) if u == &format!("u{}", 19 - kept_users)));
    }

    #[test]
    fn budget_keeps_the_leading_summary_as_long_as_possible() {
        let mut history = vec![Msg::System("sys".into()), Msg::Summary("old".into())];
        let long = "y".repeat(2_000);
        for i in 1..=8 {
            history.push(user(&format!("u{i}")));
            history.push(assistant(&long));
        }
        let plan = plan(&history, 2, 1_000, 100).unwrap();
        assert!(matches!(&plan.head[0], Msg::Summary(s) if s == "old"));
    }

    #[test]
    fn estimate_tokens_is_ascii_and_cjk_aware() {
        assert_eq!(estimate_text(""), 0);
        assert_eq!(estimate_text("abcd"), 1);
        assert_eq!(estimate_text("abcde"), 2); // 向上取整
        assert_eq!(estimate_text("中文"), 2);
        assert_eq!(estimate_text("a中"), 2);
        // 每条消息固定 4 的开销。
        assert_eq!(estimate_tokens(&[user("")]), 4);
        assert_eq!(estimate_tokens(&[]), 0);
    }

    #[test]
    fn builtin_prompt_matches_constant() {
        let cfg = crate::config::load_str(crate::config::BUILTIN_TOML, "<内置>").unwrap();
        assert_eq!(cfg.compact.prompt.as_deref(), Some(DEFAULT_COMPACT_PROMPT));
        assert_eq!(cfg.compact.keep_turns, Some(DEFAULT_KEEP_TURNS));
    }
}
