//! header 值 `${VAR}` 展开；明文密钥告警。

use std::collections::BTreeSet;

/// 展开一个 header 值中的 `${VAR}`。
/// 变量缺失 → Err（该 provider 标记不可用，非全局致命——由调用方处理）。
pub fn expand_var(template: &str) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(pos) = rest.find("${") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 2..];
        let Some(end) = after.find('}') else {
            return Err(format!("未闭合的 ${{...}}：{template}"));
        };
        let name = &after[..end];
        if name.is_empty() {
            return Err(format!("空的变量名：{template}"));
        }
        match std::env::var(name) {
            Ok(v) => out.push_str(&v),
            Err(_) => return Err(name.to_string()),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// 展开整个 header 表。任一变量缺失即 Err（列出缺失变量）。
pub fn expand_headers(headers: &[(String, String)]) -> Result<Vec<(String, String)>, String> {
    let mut expanded = Vec::with_capacity(headers.len());
    let mut missing = BTreeSet::new();
    let mut other: Option<String> = None;
    for (k, v) in headers {
        match expand_var(v) {
            Ok(value) => expanded.push((k.clone(), value)),
            Err(err) => {
                // expand_var 对未闭合/空名返回整句错误；否则 Err 就是缺失变量名。
                if err.starts_with("未闭合") || err.starts_with("空的变量名") {
                    other.get_or_insert(err);
                } else {
                    missing.insert(err);
                }
            }
        }
    }
    if !missing.is_empty() {
        return Err(missing.into_iter().collect::<Vec<_>>().join(", "));
    }
    if let Some(e) = other {
        return Err(e);
    }
    Ok(expanded)
}

/// 疑似明文密钥告警（只告警不阻断）。
pub fn warn_plaintext_secrets(headers: &[(String, String)]) {
    for (name, value) in headers {
        // 含 ${...} 的是模板，展开后才可能出现密钥，不在此告警。
        if value.contains("${") {
            continue;
        }
        let lower = value.trim();
        let looks_secret = lower.starts_with("sk-")
            || lower.contains("sk-")
            || lower.starts_with("Bearer ")
            || lower.starts_with("bearer ")
            || lower.starts_with("Bearer\t");
        if looks_secret && !lower.is_empty() {
            eprintln!("警告：header `{name}` 疑似包含明文密钥，建议改用 ${{VAR}} 形式");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_known_var() {
        std::env::set_var("RHO_TEST_VAR", "abc");
        assert_eq!(expand_var("x-${RHO_TEST_VAR}-y").unwrap(), "x-abc-y");
        std::env::remove_var("RHO_TEST_VAR");
    }

    #[test]
    fn missing_var_is_error() {
        let err = expand_var("${RHO_TEST_MISSING_VAR}").unwrap_err();
        assert!(err.contains("RHO_TEST_MISSING_VAR"));
    }
}
