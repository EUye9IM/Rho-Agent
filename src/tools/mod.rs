//! 工具中间层：统一 spec / 参数解析 / 执行结果 / 注册表。
//! 具体工具在子模块中自行持有 schema、参数结构、语义校验和执行逻辑。

mod bash;
mod edit;
mod read;
mod write;

pub use bash::BashTool;
pub use edit::EditTool;
pub use read::ReadTool;
pub use write::WriteTool;

use crate::term::Cancel;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// 与协议无关的工具定义；适配层负责映射成各自的 wire format。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// 工具执行状态：既决定 UI 样式，也决定 agent 控制流。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    /// 工具完成，结果有效。
    Success,
    /// 参数错误、执行失败、超时、非零退出等；继续让模型自行纠正。
    Failure,
    /// 用户 Ctrl-C 打断；记录当前结果，剩余调用补占位并停止本轮。
    Interrupted,
}

/// 工具执行的统一结果。
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// 给模型的结果。
    pub output: String,
    /// 给用户看的结果；None 表示不展示结果体。
    pub display: Option<String>,
    /// UI 样式 + agent 控制流。
    pub status: ToolStatus,
}

impl ToolResult {
    pub fn success(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            display: None,
            status: ToolStatus::Success,
        }
    }

    pub fn success_with_display(output: impl Into<String>, display: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            display: Some(display.into()),
            status: ToolStatus::Success,
        }
    }

    pub fn failure(output: impl Into<String>) -> Self {
        let output = output.into();
        Self {
            display: Some(output.clone()),
            output,
            status: ToolStatus::Failure,
        }
    }

    pub fn failure_with_display(output: impl Into<String>, display: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            display: Some(display.into()),
            status: ToolStatus::Failure,
        }
    }

    pub fn interrupted(output: impl Into<String>) -> Self {
        let output = output.into();
        Self {
            display: Some(output.clone()),
            output,
            status: ToolStatus::Interrupted,
        }
    }
}

/// 工具执行上下文。cwd 是 rho 进程启动目录，用于解析相对路径。
pub struct ToolContext<'a> {
    pub cwd: &'a Path,
    pub cancel: &'a Cancel,
}

/// 每个工具自己负责 schema、参数校验、执行和结果格式化。
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    fn invoke(&self, args: Value, ctx: &ToolContext<'_>) -> ToolResult;

    /// 调用行摘要；默认展示紧凑 JSON，具体工具可覆盖成更易读的字段。
    fn summary(&self, args: &Value) -> String {
        let text = serde_json::to_string(args).unwrap_or_else(|_| "…".to_string());
        truncate_chars(&text, 200)
    }
}

/// 工具注册表：按名字查找，按声明顺序生成 tools 数组。
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<(String, Box<dyn Tool>)>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 内置工具集合。配置无法关闭或替换这些工具。
    pub fn builtin() -> Self {
        let mut registry = Self::new();
        registry.register(BashTool);
        registry.register(ReadTool);
        registry.register(WriteTool);
        registry.register(EditTool);
        registry
    }

    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        let name = tool.spec().name;
        if let Some(slot) = self.tools.iter_mut().find(|(n, _)| *n == name) {
            slot.1 = Box::new(tool);
        } else {
            self.tools.push((name, Box::new(tool)));
        }
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, tool)| tool.as_ref())
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|(_, tool)| tool.spec()).collect()
    }
}

/// 相对路径按 rho 启动目录解析；不做 canonicalize，也不展开 `~`。
pub(crate) fn resolve_path(cwd: &Path, raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() {
        return Err("path 不能为空".to_string());
    }
    let path = Path::new(raw);
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    })
}

pub(crate) fn display_path(path: &Path) -> String {
    path.display().to_string()
}

/// 按 Unicode 字符截断，超出时补省略号。
pub(crate) fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{out}…")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_has_expected_tools_in_order() {
        let registry = ToolRegistry::builtin();
        let names: Vec<String> = registry.specs().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["bash", "read", "write", "edit"]);
    }

    #[test]
    fn every_builtin_schema_is_a_closed_object() {
        for spec in ToolRegistry::builtin().specs() {
            assert_eq!(spec.parameters["type"], "object", "{}", spec.name);
            assert_eq!(
                spec.parameters["additionalProperties"], false,
                "{}",
                spec.name
            );
        }
    }

    #[test]
    fn register_replaces_same_name_in_place() {
        struct Stub(&'static str);
        impl Tool for Stub {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "bash".into(),
                    description: self.0.into(),
                    parameters: serde_json::json!({"type": "object"}),
                }
            }
            fn invoke(&self, _args: Value, _ctx: &ToolContext<'_>) -> ToolResult {
                ToolResult::success("ok")
            }
        }

        let mut registry = ToolRegistry::new();
        registry.register(Stub("first"));
        registry.register(Stub("second"));
        let specs = registry.specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].description, "second");
    }

    #[test]
    fn relative_path_uses_context_cwd() {
        let path = resolve_path(Path::new("/tmp/work"), "a/b").unwrap();
        assert_eq!(path, Path::new("/tmp/work/a/b"));
        assert_eq!(
            resolve_path(Path::new("/tmp/work"), "/abs").unwrap(),
            Path::new("/abs")
        );
        assert!(resolve_path(Path::new("/tmp/work"), "").is_err());
    }
}
