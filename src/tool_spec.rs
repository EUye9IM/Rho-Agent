//! 与协议无关的工具定义；适配层负责映射成各自的 wire format。

use serde_json::{json, Value};

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

/// bash 工具的模型侧定义（OpenAI Chat / Responses 共用）。
pub fn bash() -> ToolSpec {
    ToolSpec {
        name: "bash",
        description: "在用户机器上执行一条 bash 命令（无持久 shell，每次调用相互独立）。",
        parameters: json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "要执行的完整 bash 命令"
                }
            },
            "required": ["command"]
        }),
    }
}
