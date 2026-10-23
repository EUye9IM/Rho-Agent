//! 配置严格校验：悬空引用、前缀对应、必填字段；一次报全部，带 TOML 位置。

use super::Config;

/// 单条校验错误（带位置信息，供一次报出）。
#[derive(Debug)]
pub struct ValidationError {
    pub path: String, // 如 providers.deepseek.protocol
    pub line: Option<usize>,
    pub message: String,
}

/// 收集全部校验错误（不首个即停）：
/// - 未知字段、悬空引用、缺失必填字段一律硬报错
/// - 模型 id 全局唯一、必带 provider 前缀、前缀须对应已声明 provider
/// - thinking 档位名在同一模型内唯一
/// - providers/models 的 protocol 值须属已知协议族
pub fn validate(cfg: &Config) -> Result<(), Vec<ValidationError>> {
    // TODO: 遍历校验上述规则，聚合到 Vec 一次返回
    let _ = cfg;
    todo!()
}
