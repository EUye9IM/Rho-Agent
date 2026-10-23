//! header 值 `${VAR}` 展开；明文密钥告警。

/// 展开一个 header 值中的 `${VAR}`。
/// 变量缺失 → Err（该 provider 标记不可用，非全局致命——由调用方处理）。
pub fn expand_var(template: &str) -> Result<String, String> {
    // TODO: 扫描 ${...}，从进程 env 展开；缺失返回 Err(变量名)
    let _ = template;
    todo!()
}

/// 展开整个 header 表。任一变量缺失即 Err（列出缺失变量）。
pub fn expand_headers(headers: &[(String, String)]) -> Result<Vec<(String, String)>, String> {
    // TODO: 逐条 expand_var；聚合缺失变量名
    let _ = headers;
    todo!()
}

/// 疑似明文密钥告警（只告警不阻断）。
pub fn warn_plaintext_secrets(headers: &[(String, String)]) {
    // TODO: 值不含 ${VAR} 且长得像 Bearer/sk- 开头的密钥 → eprintln 告警
    let _ = headers;
    todo!()
}
