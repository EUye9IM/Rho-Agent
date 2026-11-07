//! 适配器共用的请求辅助：模型名、默认 header、thinking 片段合并。

use crate::config::{Model, Provider};
use serde_json::Value;

/// 上游 model 字段：默认取模型 id 第一个 '/' 之后的部分，`api_model` 可覆盖。
pub(super) fn model_name(model: &Model, model_id: &str) -> String {
    model.api_model.clone().unwrap_or_else(|| {
        model_id
            .split_once('/')
            .map(|(_, m)| m)
            .unwrap_or(model_id)
            .to_string()
    })
}

/// 配置 header 原样透传，缺失时补 JSON / SSE 默认值；大小写不敏感判重。
pub(super) fn default_headers(p: &Provider) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = p
        .auth
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let has_content_type = out
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("Content-Type"));
    let has_accept = out.iter().any(|(k, _)| k.eq_ignore_ascii_case("Accept"));
    if !has_content_type {
        out.push(("Content-Type".into(), "application/json".into()));
    }
    if !has_accept {
        out.push(("Accept".into(), "text/event-stream".into()));
    }
    out
}

/// thinking 片段顶层合并（对象浅合并；片段须为对象，否则配置校验报错）。
pub(super) fn merge_thinking(body: &mut Value, fragment: &Value) {
    if let (Some(body), Some(fragment)) = (body.as_object_mut(), fragment.as_object()) {
        for (k, v) in fragment {
            body.insert(k.clone(), v.clone());
        }
    }
}
