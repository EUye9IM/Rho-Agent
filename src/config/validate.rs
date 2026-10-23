//! 配置严格校验：悬空引用、前缀对应、必填字段；一次报全部，带 TOML 位置。

use super::{find_model, find_provider, model_provider_prefix, Config};

/// 单条校验错误（带位置信息，供一次报出）。
#[derive(Debug)]
pub struct ValidationError {
    pub path: String, // 如 providers.deepseek.protocol
    pub line: Option<usize>,
    pub message: String,
}

fn err(path: impl Into<String>, message: impl Into<String>) -> ValidationError {
    ValidationError { path: path.into(), line: None, message: message.into() }
}

/// 收集全部校验错误（不首个即停）：
/// - 未知字段、悬空引用、缺失必填字段一律硬报错
/// - 模型 id 全局唯一、必带 provider 前缀、前缀须对应已声明 provider
/// - thinking 档位名在同一模型内唯一
/// - providers/models 的 protocol 值须属已知协议族
pub fn validate(cfg: &Config) -> Result<(), Vec<ValidationError>> {
    let mut errs = Vec::new();

    if let Some(p) = &cfg.default_provider {
        if find_provider(cfg, p).is_none() {
            errs.push(err("default_provider", format!("悬空引用：未声明的供应商 `{p}`")));
        }
    }
    if let Some(m) = &cfg.default_model {
        if find_model(cfg, m).is_none() {
            errs.push(err("default_model", format!("悬空引用：未声明的模型 `{m}`")));
        }
    }

    for (id, provider) in &cfg.providers {
        let path = format!("providers.{id}");
        if provider.protocol.trim().is_empty() {
            errs.push(err(format!("{path}.protocol"), "必填字段不能为空"));
        } else if !matches!(provider.protocol.as_str(), "openai_chat") {
            errs.push(err(
                format!("{path}.protocol"),
                format!("未知协议族 `{}`（v1 只支持 openai_chat）", provider.protocol),
            ));
        }
        if provider.base_url.trim().is_empty() {
            errs.push(err(format!("{path}.base_url"), "必填字段不能为空"));
        } else if !provider.base_url.starts_with("http://")
            && !provider.base_url.starts_with("https://")
        {
            errs.push(err(
                format!("{path}.base_url"),
                "必须是以 http:// 或 https:// 开头的 URL",
            ));
        }
        for (header, value) in &provider.auth.headers {
            if header.trim().is_empty() {
                errs.push(err(format!("{path}.auth.headers"), "header 名不能为空"));
            }
            if value.trim().is_empty() {
                errs.push(err(
                    format!("{path}.auth.headers.{header}"),
                    "header 值不能为空",
                ));
            }
        }
    }

    for (id, model) in &cfg.models {
        let path = format!("models.{id}");
        match model_provider_prefix(id) {
            None => errs.push(err(
                &path,
                "模型 id 必须带 provider 前缀，形如 `provider/model`",
            )),
            Some(prefix) => {
                if find_provider(cfg, prefix).is_none() {
                    errs.push(err(
                        &path,
                        format!("模型 id 的前缀 `{prefix}` 没有对应已声明的供应商"),
                    ));
                }
            }
        }
        if model.context_window == 0 {
            errs.push(err(format!("{path}.context_window"), "必须大于 0"));
        }
        if model.max_output == 0 {
            errs.push(err(format!("{path}.max_output"), "必须大于 0"));
        }
        if let Some(default) = &model.thinking_default {
            if !model.thinking.contains_key(default) {
                errs.push(err(
                    format!("{path}.thinking_default"),
                    format!("档位 `{default}` 未在 thinking 表中声明"),
                ));
            }
        }
        for (level, fragment) in &model.thinking {
            if level.trim().is_empty() {
                errs.push(err(format!("{path}.thinking"), "档位名不能为空"));
            }
            if !fragment.is_object() {
                errs.push(err(
                    format!("{path}.thinking.{level}"),
                    "思考档位必须是 JSON 对象（请求时顶层合并）",
                ));
            }
        }
    }

    if let Some(steps) = cfg.max_tool_steps {
        if steps == 0 {
            errs.push(err("max_tool_steps", "必须大于 0"));
        }
    }
    if cfg.tools.bash.timeout_seconds == 0 {
        errs.push(err("tools.bash.timeout_seconds", "必须大于 0"));
    }
    if cfg.tools.bash.max_output_bytes == 0 {
        errs.push(err("tools.bash.max_output_bytes", "必须大于 0"));
    }

    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}
