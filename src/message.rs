//! 与协议解耦的内部消息模型：chat 历史与 provider 适配层共用。
//! 会话文件按本模块的 serde 表示原样落盘（`store`），字段名即文件格式。

use serde::{Deserialize, Serialize};

/// 工具执行状态：既决定 UI 样式，也决定 agent 控制流。
/// 定义在这里而不用 `tools` 的类型，是因为它随 `Msg::Tool` 一起落盘并被重放使用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolStatus {
    /// 工具完成，结果有效。
    Success,
    /// 参数错误、执行失败、超时、非零退出等；继续让模型自行纠正。
    Failure,
    /// 用户 Ctrl-C 打断；记录当前结果，剩余调用补占位并停止本轮。
    Interrupted,
}

/// 与协议解耦的消息模型（适配层负责映射）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Msg {
    System(String),
    /// 压缩产生的检查点：位于 system 之后、保留轮之前（适配层映射为 system 消息）。
    Summary(String),
    User(String),
    /// Assistant 载荷：流终态与历史共用同一类型（见 [`AssistantMsg`]）。
    Assistant(AssistantMsg),
    /// 工具结果。`name` / `display` / `status` 只用于重放渲染；适配层只读 id 与 content。
    Tool {
        tool_call_id: String,
        name: String,
        content: String,
        /// 当时给用户展示的文本；`None` 表示不展示结果体（如成功读取文件）。
        display: Option<String>,
        status: ToolStatus,
    },
}

/// Assistant 消息的载荷。`Final` 事件与历史中的 Assistant 消息共用此类型，
/// 避免事件携带 System/User/Tool 等无效状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMsg {
    pub text: Option<String>,
    pub reasoning: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

/// arguments 保持原始 JSON 字符串。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// 上游返回的单次请求 token 用量（已从协议字段归一化）。
/// `cached_input` 只在供应商提供缓存命中统计时存在。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub total: u64,
    pub cached_input: Option<u64>,
}
