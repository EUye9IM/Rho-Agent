//! 配置模型：加载、严格校验、`${VAR}` 展开、供应商启用。见 design.md 五节。

mod expand;
mod validate;

use serde::Deserialize;
use std::collections::BTreeMap;

pub use expand::expand_headers;
pub use expand::warn_plaintext_secrets;
pub use validate::{validate, ValidationError};

/// 有序 map：IndexMap 保插入序（与 toml preserve_order 同底座），
/// 且 serde 原生支持反序列化任意值类型（toml::map::Map 只对 Value 实现 trait）。
pub type OrderedMap<V> = indexmap::IndexMap<String, V>;

/// 顶层配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub system_prompt: Option<String>,
    pub max_tool_steps: Option<u32>,
    pub providers: OrderedMap<Provider>,
    pub models: OrderedMap<Model>,
    #[serde(default)]
    pub tools: ToolsConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub protocol: String, // v1 只实现 "openai_chat"
    pub base_url: String,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub compat: CompatConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// header 表（值含 ${VAR}，在选择供应商时展开）
    #[serde(default)]
    pub headers: OrderedMap<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompatConfig {
    #[serde(default)]
    pub echo_reasoning: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    /// 上游 model 字段：默认取 id 第一个 '/' 之后的部分，api_model 可覆盖
    pub api_model: Option<String>,
    pub context_window: u64,
    pub max_output: u64,
    #[serde(default)]
    pub thinking_default: Option<String>,
    /// (档位名, JSON 片段) — 顶层合并进请求 body
    #[serde(default)]
    pub thinking: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolsConfig {
    #[serde(default)]
    pub bash: BashToolConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BashToolConfig {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_max_output")]
    pub max_output_bytes: usize,
}

fn default_timeout() -> u64 {
    120
}

fn default_max_output() -> usize {
    16384
}

impl Default for BashToolConfig {
    fn default() -> Self {
        Self { allow: Vec::new(), timeout_seconds: default_timeout(), max_output_bytes: default_max_output() }
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

fn load_file(path: &str) -> Result<Config, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| format!("无法读取配置文件 {path}：{e}"))?;
    load_source(&source, path)
}

fn load_builtin() -> Result<Config, String> {
    load_source(include_str!("default.toml"), "<内置默认配置>")
}

fn load_source(source: &str, label: &str) -> Result<Config, String> {
    let cfg: Config = toml::from_str(source).map_err(|e| {
        let line = e
            .span()
            .and_then(|s| source.get(..s.start))
            .map(|prefix| prefix.matches('\n').count() + 1);
        match line {
            Some(n) => format!("配置解析失败 {label}（第 {n} 行）：{e}"),
            None => format!("配置解析失败 {label}：{e}"),
        }
    })?;
    // 明文密钥告警在加载时统一扫描一次，选择/切换阶段不再重复打印。
    for (_, provider) in providers_in_order(&cfg) {
        let pairs: Vec<(String, String)> = provider
            .auth
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        warn_plaintext_secrets(&pairs);
    }
    match validate(&cfg) {
        Ok(()) => Ok(cfg),
        Err(errs) => {
            let mut msg = format!("配置校验失败 {label}：");
            for e in errs {
                msg.push('\n');
                if let Some(line) = e.line {
                    msg.push_str(&format!("  - {}（第 {} 行）：{}", e.path, line, e.message));
                } else {
                    msg.push_str(&format!("  - {}：{}", e.path, e.message));
                }
            }
            Err(msg)
        }
    }
}

/// 模型 id 的供应商前缀（`deepseek/model` → `deepseek`）。
pub fn model_provider_prefix(model_id: &str) -> Option<&str> {
    model_id
        .split_once('/')
        .and_then(|(p, m)| (!p.is_empty() && !m.is_empty()).then_some(p))
}

/// 某供应商声明顺序中的第一个模型。
pub fn first_model_for<'a>(cfg: &'a Config, provider_id: &str) -> Option<(&'a String, &'a Model)> {
    cfg.models
        .iter()
        .find(|(id, _)| model_provider_prefix(id) == Some(provider_id))
}

fn expand_provider(provider: &Provider) -> Result<Provider, String> {
    let pairs: Vec<(String, String)> = provider
        .auth
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let expanded = expand_headers(&pairs)?;
    let mut out = provider.clone();
    out.auth.headers = expanded.into_iter().collect();
    Ok(out)
}

fn provider_unavailable(id: &str, err: String) -> String {
    format!("供应商 {id} 不可用：认证变量缺失或无效（{err}）")
}

/// 供应商是否可用（已声明且认证变量齐全）。REPL /provider 列表标注用。
pub fn provider_availability(cfg: &Config, provider_id: &str) -> Result<(), String> {
    let provider = cfg
        .providers
        .get(provider_id)
        .ok_or_else(|| format!("未声明的供应商：{provider_id}"))?;
    expand_provider(provider)
        .map(|_| ())
        .map_err(|e| provider_unavailable(provider_id, e))
}

/// 按供应商选择：认证 header 在此展开，模型取该供应商声明顺序第一个。
pub fn selection_for_provider(cfg: &Config, provider_id: &str) -> Result<Selection, String> {
    let provider = cfg
        .providers
        .get(provider_id)
        .ok_or_else(|| format!("未声明的供应商：{provider_id}"))?;
    let provider = expand_provider(provider).map_err(|e| provider_unavailable(provider_id, e))?;
    let (model_id, model) = first_model_for(cfg, provider_id)
        .ok_or_else(|| format!("供应商 {provider_id} 未声明任何模型"))?;
    Ok(Selection {
        provider_id: provider_id.to_string(),
        provider,
        model_id: model_id.clone(),
        model: model.clone(),
        thinking: model.thinking_default.clone(),
    })
}

/// 按模型选择：供应商由 id 前缀决定。
pub fn selection_for_model(cfg: &Config, model_id: &str) -> Result<Selection, String> {
    selection_for(cfg, model_id, None)
}

fn selection_for(
    cfg: &Config,
    model_id: &str,
    required_provider: Option<&str>,
) -> Result<Selection, String> {
    let model = find_model(cfg, model_id).ok_or_else(|| format!("未声明的模型：{model_id}"))?;
    let prefix = model_provider_prefix(model_id)
        .ok_or_else(|| format!("模型 id 必须带供应商前缀：{model_id}"))?;
    if let Some(required) = required_provider {
        if prefix != required {
            return Err(format!("模型 {model_id} 不属于供应商 {required}"));
        }
    }
    let provider = cfg
        .providers
        .get(prefix)
        .ok_or_else(|| format!("模型 {model_id} 的前缀供应商未声明：{prefix}"))?;
    let provider = expand_provider(provider).map_err(|e| provider_unavailable(prefix, e))?;
    Ok(Selection {
        provider_id: prefix.to_string(),
        provider,
        model_id: model_id.to_string(),
        model: model.clone(),
        thinking: model.thinking_default.clone(),
    })
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
    match (cli_provider, cli_model) {
        (Some(p), Some(m)) => selection_for(cfg, m, Some(p)),
        (Some(p), None) => selection_for_provider(cfg, p),
        (None, Some(m)) => selection_for_model(cfg, m),
        (None, None) => {
            if let Some(model) = cfg.default_model.as_deref() {
                return selection_for_model(cfg, model);
            }
            if let Some(provider) = cfg.default_provider.as_deref() {
                return selection_for_provider(cfg, provider);
            }
            let mut unavailable = Vec::new();
            for (id, provider) in providers_in_order(cfg) {
                match expand_provider(provider) {
                    Ok(_) => {
                        if first_model_for(cfg, id).is_some() {
                            return selection_for_provider(cfg, id);
                        }
                        unavailable.push(format!("{id}：未声明任何模型"));
                    }
                    Err(e) => unavailable.push(format!("{id}：{e}")),
                }
            }
            Err(format!(
                "没有可用的供应商（未找到认证变量齐全者）：\n  {}",
                unavailable.join("\n  ")
            ))
        }
    }
}

/// 查找辅助（供 resolve_selection / REPL 切换复用）。
pub fn find_provider<'a>(cfg: &'a Config, id: &str) -> Option<&'a Provider> {
    cfg.providers.get(id)
}

pub fn find_model<'a>(cfg: &'a Config, id: &str) -> Option<&'a Model> {
    cfg.models.get(id)
}

/// 声明顺序迭代（OrderedMap 保序）。
pub fn providers_in_order(cfg: &Config) -> impl Iterator<Item = (&String, &Provider)> {
    cfg.providers.iter()
}

pub fn models_in_order(cfg: &Config) -> impl Iterator<Item = (&String, &Model)> {
    cfg.models.iter()
}
