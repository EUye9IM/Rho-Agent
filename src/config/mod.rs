//! 配置模型：加载、严格校验、`${VAR}` 展开、供应商启用。

mod dns;
mod expand;
mod validate;

use serde::Deserialize;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

pub use dns::parse_endpoint as parse_dns_endpoint;
pub use expand::expand_headers;
pub use expand::warn_plaintext_secrets;
pub use validate::{validate, ValidationError};

/// 有序 map：IndexMap 保插入序（与 toml preserve_order 同底座），
/// 且 serde 原生支持反序列化任意值类型（toml::map::Map 只对 Value 实现 trait）。
pub type OrderedMap<V> = indexmap::IndexMap<String, V>;

/// 已实现的协议族。配置字段保持字符串，校验时经此解析，
/// 以保留“语义错误一次收集全部”的行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    OpenAiChat,
    OpenAiResponses,
}

impl Protocol {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "openai_chat" => Some(Self::OpenAiChat),
            "openai_responses" => Some(Self::OpenAiResponses),
            _ => None,
        }
    }
}

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
    /// DoH 端点：配置后按声明顺序优先解析，全部失败才回退系统解析器。
    #[serde(default)]
    pub dns: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub protocol: String, // 经 Protocol::parse 解析；当前支持 openai_chat / openai_responses
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
        Self {
            timeout_seconds: default_timeout(),
            max_output_bytes: default_max_output(),
        }
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

/// 内置默认配置的 TOML 源（`--init` 原样写出）。
pub const BUILTIN_TOML: &str = include_str!("default.toml");

/// 默认配置文件路径：`$XDG_CONFIG_HOME/rho/config.toml`；
/// `XDG_CONFIG_HOME` 未设置或为空时用 `$HOME/.config/rho/config.toml`。
pub fn default_config_path() -> Option<PathBuf> {
    default_config_path_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

fn default_config_path_from(
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("rho").join("config.toml"));
    }
    home.filter(|v| !v.is_empty()).map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("rho")
            .join("config.toml")
    })
}

/// 加载配置：`--config` 优先；否则读默认路径（存在时）；都没有则用内置默认。
/// 任一来源都完全替换内置默认（不合并）。
pub fn load(path: &Option<String>) -> Result<Config, String> {
    match path {
        Some(p) => load_file(Path::new(p)),
        None => match default_config_path() {
            Some(p) if p.exists() => load_file(&p),
            _ => load_builtin(),
        },
    }
}

/// 把内置配置写到 `path`（`--init`）。目标已存在时报错，不覆盖。
pub fn write_builtin_config(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("无法创建目录 {}：{e}", parent.display()))?;
    }
    // create_new：目标已存在时以 AlreadyExists 失败，避免覆盖。
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => {
                format!("配置文件已存在，未覆盖：{}", path.display())
            }
            _ => format!("无法写入配置文件 {}：{e}", path.display()),
        })?;
    file.write_all(BUILTIN_TOML.as_bytes())
        .map_err(|e| format!("无法写入配置文件 {}：{e}", path.display()))
}

fn load_file(path: &Path) -> Result<Config, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| format!("无法读取配置文件 {}：{e}", path.display()))?;
    // 报错文案带文件路径而非通用标签。
    parse_and_validate(&source, &path.display().to_string())
}

fn load_builtin() -> Result<Config, String> {
    parse_and_validate(BUILTIN_TOML, "<内置默认配置>")
}

/// 从 TOML 字符串构建已验证的 Config（测试入口：无需临时文件）。
/// `label` 仅用于报错文案。
pub fn load_str(source: &str, label: &str) -> Result<Config, String> {
    parse_and_validate(source, label)
}

fn parse_and_validate(source: &str, label: &str) -> Result<Config, String> {
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
        warn_plaintext_secrets(&header_pairs(provider));
    }
    match validate(&cfg) {
        Ok(()) => Ok(cfg),
        Err(errs) => {
            let mut msg = format!("配置校验失败 {label}：");
            for e in errs {
                msg.push('\n');
                msg.push_str(&format!("  - {}：{}", e.path, e.message));
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

fn header_pairs(provider: &Provider) -> Vec<(String, String)> {
    provider
        .auth
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn expand_provider(provider: &Provider) -> Result<Provider, String> {
    let expanded = expand_headers(&header_pairs(provider))?;
    let mut out = provider.clone();
    out.auth.headers = expanded.into_iter().collect();
    Ok(out)
}

fn provider_unavailable(id: &str, err: String) -> String {
    format!("供应商 {id} 不可用：{err}")
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

/// 供应商启用与默认值冲突消解：
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

/// thinking 档位合法性判定：None = 合法；Some(错误文案) = 非法。
/// CLI（--thinking，exit 2）与 REPL（/thinking，展示错误）共用此判定，避免规则分叉。
pub fn thinking_level_err(model: &Model, model_id: &str, level: &str) -> Option<String> {
    let levels: Vec<&str> = model.thinking.keys().map(String::as_str).collect();
    if levels.is_empty() {
        return Some(format!("模型 {model_id} 未声明思考档位"));
    }
    if !model.thinking.contains_key(level) {
        return Some(format!(
            "无效思考档位 `{level}`（模型 {model_id}）；可选：{}",
            levels.join(", ")
        ));
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    static TMP_SEQ: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn protocol_parses_known_values() {
        assert_eq!(Protocol::parse("openai_chat"), Some(Protocol::OpenAiChat));
        assert_eq!(
            Protocol::parse("openai_responses"),
            Some(Protocol::OpenAiResponses)
        );
        assert_eq!(Protocol::parse("anthropic_messages"), None);
    }

    #[test]
    fn default_path_prefers_xdg_then_home() {
        assert_eq!(
            default_config_path_from(Some("/xdg".into()), Some("/home/u".into())),
            Some(PathBuf::from("/xdg/rho/config.toml"))
        );
        // XDG_CONFIG_HOME 为空字符串时回退 HOME。
        assert_eq!(
            default_config_path_from(Some("".into()), Some("/home/u".into())),
            Some(PathBuf::from("/home/u/.config/rho/config.toml"))
        );
        assert_eq!(default_config_path_from(None, None), None);
    }

    #[test]
    fn dns_endpoints_load_and_validate() {
        let src = format!(
            "dns = [\"https://223.5.5.5/dns-query\", \"https://[2400:3200::1]/dns-query\"]\n{BUILTIN_TOML}"
        );
        let cfg = load_str(&src, "t").unwrap();
        assert_eq!(cfg.dns.len(), 2);
        assert_eq!(cfg.dns[0], "https://223.5.5.5/dns-query");

        // 每个端点单独校验，路径写到 dns.<下标>。
        let bad = format!(
            "dns = [\"https://223.5.5.5/dns-query\", \"https://dns.alidns.com/dns-query\"]\n{BUILTIN_TOML}"
        );
        let err = load_str(&bad, "t").unwrap_err();
        assert!(err.contains("dns.1"), "error was {err}");
        assert!(err.contains("IP 字面量"), "error was {err}");
    }

    #[test]
    fn allowlist_config_is_rejected() {
        let src = format!("{BUILTIN_TOML}\nallow = []\n");
        let err = load_str(&src, "t").unwrap_err();
        assert!(err.contains("allow"), "error was {err}");
    }

    #[test]
    fn write_builtin_config_writes_once_then_refuses() {
        let dir = std::env::temp_dir().join(format!(
            "rho-init-{}-{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let path = dir.join("nested").join("config.toml");

        write_builtin_config(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), BUILTIN_TOML);

        let err = write_builtin_config(&path).unwrap_err();
        assert!(err.contains("已存在"), "error was {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
