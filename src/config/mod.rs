//! 配置模型：加载、严格校验、`${VAR}` 展开、供应商启用。见 design.md 五节。

mod expand;
mod validate;

use std::collections::BTreeMap;

pub use expand::expand_headers;
pub use validate::{validate, ValidationError};

/// 顶层配置（preserve_order 语义靠 Vec 承载声明顺序，HashMap 会退化成字母序）。
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub system_prompt: Option<String>,
    pub max_tool_steps: Option<u32>,
    /// (id, provider) — 声明顺序保留
    pub providers: Vec<(String, Provider)>,
    /// (id, model) — 声明顺序保留；id 全局唯一且带 provider 前缀
    pub models: Vec<(String, Model)>,
    pub tools: ToolsConfig,
}

#[derive(Debug, Clone)]
pub struct Provider {
    pub protocol: String, // v1 只实现 "openai_chat"
    pub base_url: String,
    pub auth: AuthConfig,
    pub compat: CompatConfig,
}

#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    /// header 表（值含 ${VAR}，在选择供应商时展开）
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct CompatConfig {
    pub echo_reasoning: bool,
}

impl Default for CompatConfig {
    fn default() -> Self {
        Self { echo_reasoning: false }
    }
}

#[derive(Debug, Clone)]
pub struct Model {
    /// 上游 model 字段：默认取 id 第一个 '/' 之后的部分，api_model 可覆盖
    pub api_model: Option<String>,
    pub context_window: u64,
    pub max_output: u64,
    pub thinking_default: Option<String>,
    /// (档位名, JSON 片段) — 顶层合并进请求 body
    pub thinking: Vec<(String, serde_json::Value)>,
}

#[derive(Debug, Clone)]
pub struct ToolsConfig {
    pub bash: BashToolConfig,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self { bash: BashToolConfig::default() }
    }
}

#[derive(Debug, Clone)]
pub struct BashToolConfig {
    pub allow: Vec<String>,
    pub timeout_seconds: u64,   // 默认 120
    pub max_output_bytes: usize, // 默认 16384
}

impl Default for BashToolConfig {
    fn default() -> Self {
        Self { allow: Vec::new(), timeout_seconds: 120, max_output_bytes: 16384 }
    }
}

/// 启动后的会话选择（供应商启用 + 默认值冲突消解的结果）。
#[derive(Debug, Clone)]
pub struct Selection {
    pub provider_id: String,
    pub provider: Provider,
    pub model_id: String,
    pub model: Model,
    pub thinking: Option<String>, // 档位名；无 thinking 表则 None
}

/// 加载配置：`--config` 提供则完全替换内置默认（不合并）；未提供用内置默认。
pub fn load(path: &Option<String>) -> Result<Config, String> {
    match path {
        Some(p) => load_file(p),
        None => load_builtin(),
    }
}

fn load_file(_path: &str) -> Result<Config, String> {
    // TODO: 读文件 → toml 解析 → 严格反序列化（未知字段硬报错，带 TOML 位置）
    //       → validate() 收集全部错误一次报出
    todo!()
}

fn load_builtin() -> Result<Config, String> {
    // TODO: include_str! 内置默认配置（仅 DeepSeek 两模型）→ 同 load_file 的解析路径
    todo!()
}

/// 供应商启用与默认值冲突消解（design.md 五节规则）：
/// - default_model 效力高于 default_provider 与隐式选择（前缀冲突以模型为准）
/// - 显式指定的 provider key 不齐全 → 报错列缺失变量，不回退
/// - 隐式选择：声明顺序第一个 key 齐全者；default_provider 优先；--provider 覆盖
/// - 只传 --model：直接采用（须带前缀且属已声明 provider），覆盖 default_provider
/// - 只传 --provider：模型取该 provider 声明顺序第一个
pub fn resolve_selection(
    cfg: &Config,
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
) -> Result<Selection, String> {
    // TODO: 展开各 provider 的 auth header（缺变量者标记不可用，非致命）
    // TODO: 按上述规则组合出 Selection；thinking 取所选模型的 thinking_default
    // TODO: --provider 与 --model 同给且不属该 provider → 用法错误
    let _ = (cfg, cli_provider, cli_model);
    todo!()
}

// 用于查找的辅助（供 resolve_selection / REPL 切换复用）
pub fn find_provider<'a>(cfg: &'a Config, id: &str) -> Option<&'a Provider> {
    cfg.providers.iter().find(|(pid, _)| pid == id).map(|(_, p)| p)
}

pub fn find_model<'a>(cfg: &'a Config, id: &str) -> Option<&'a Model> {
    cfg.models.iter().find(|(mid, _)| mid == id).map(|(_, m)| m)
}

#[allow(dead_code)]
fn _unused(_: &BTreeMap<String, String>) {}
