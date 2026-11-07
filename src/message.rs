//! 与协议解耦的内部消息模型：chat 历史与 provider 适配层共用。

/// 与协议解耦的消息模型（适配层负责映射）。
#[derive(Debug, Clone)]
pub enum Msg {
    System(String),
    User(String),
    /// Assistant 载荷：流终态与历史共用同一类型（见 [`AssistantMsg`]）。
    Assistant(AssistantMsg),
    Tool {
        tool_call_id: String,
        content: String,
    },
}

/// Assistant 消息的载荷。`Final` 事件与历史中的 Assistant 消息共用此类型，
/// 避免事件携带 System/User/Tool 等无效状态。
#[derive(Debug, Clone)]
pub struct AssistantMsg {
    pub text: Option<String>,
    pub reasoning: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

/// arguments 保持原始 JSON 字符串。
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}
