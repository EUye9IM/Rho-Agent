//! header 值 `${VAR}` 展开；明文密钥告警。

use std::collections::BTreeSet;

/// `${VAR}` 展开失败的两种原因。
#[derive(Debug)]
pub enum ExpandError {
    /// 变量存在但环境里没有
    Missing(String),
    /// `${...}` 写法本身有问题
    Malformed(String),
}

/// 展开一个 header 值中的 `${VAR}`（环境查找由 `lookup` 提供，便于注入测试）。
/// 变量缺失 → Err（该 provider 标记不可用，非全局致命——由调用方处理）。
pub fn expand_var_with(template: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<String, ExpandError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(pos) = rest.find("${") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 2..];
        let Some(end) = after.find('}') else {
            return Err(ExpandError::Malformed(format!("未闭合的 ${{...}}：{template}")));
        };
        let name = &after[..end];
        if name.is_empty() {
            return Err(ExpandError::Malformed(format!("空的变量名：{template}")));
        }
        match lookup(name) {
            Some(v) => out.push_str(&v),
            None => return Err(ExpandError::Missing(name.to_string())),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// 生产入口：从进程环境展开单个模板值。
#[cfg(test)]
pub(crate) fn expand_var(template: &str) -> Result<String, ExpandError> {
    expand_var_with(template, |name| std::env::var(name).ok())
}

/// 展开整个 header 表（环境查找由 `lookup` 提供）。任一变量缺失即 Err（列出缺失变量）。
pub fn expand_headers_with(
    headers: &[(String, String)],
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Vec<(String, String)>, String> {
    let mut expanded = Vec::with_capacity(headers.len());
    let mut missing = BTreeSet::new();
    let mut malformed: Option<String> = None;
    for (k, v) in headers {
        match expand_var_with(v, &lookup) {
            Ok(value) => expanded.push((k.clone(), value)),
            Err(ExpandError::Missing(name)) => {
                missing.insert(name);
            }
            Err(ExpandError::Malformed(msg)) => {
                malformed.get_or_insert(msg);
            }
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "缺少环境变量：{}",
            missing.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if let Some(msg) = malformed {
        return Err(msg);
    }
    Ok(expanded)
}

/// 生产入口：从进程环境展开整个 header 表。
pub fn expand_headers(headers: &[(String, String)]) -> Result<Vec<(String, String)>, String> {
    expand_headers_with(headers, |name| std::env::var(name).ok())
}

/// 疑似明文密钥告警（只告警不阻断）。
pub fn warn_plaintext_secrets(headers: &[(String, String)]) {
    for (name, value) in headers {
        // 含 ${...} 的是模板，展开后才可能出现密钥，不在此告警。
        if value.contains("${") {
            continue;
        }
        let lower = value.trim().to_ascii_lowercase();
        // 只认常见密钥前缀，避免把普通值（如 task-xxx）误报为密钥。
        let looks_secret = lower.starts_with("sk-") || lower.starts_with("bearer ");
        if looks_secret {
            eprintln!("警告：header `{name}` 疑似包含明文密钥，建议改用 ${{VAR}} 形式");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_known_var() {
        // 规则测试用注入查找，不改进程环境（并行测试下 set_var 有竞态）。
        let lookup = |name: &str| (name == "RHO_TEST_VAR").then(|| "abc".to_string());
        assert_eq!(expand_var_with("x-${RHO_TEST_VAR}-y", lookup).unwrap(), "x-abc-y");
    }

    #[test]
    fn missing_var_is_error() {
        match expand_var("${RHO_TEST_MISSING_VAR}") {
            Err(ExpandError::Missing(name)) => assert_eq!(name, "RHO_TEST_MISSING_VAR"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn malformed_var_is_distinguished_from_missing() {
        assert!(matches!(expand_var("${"), Err(ExpandError::Malformed(_))));
        assert!(matches!(expand_var("${}"), Err(ExpandError::Malformed(_))));
        assert!(matches!(
            expand_var("${RHO_TEST_MISSING_VAR}"),
            Err(ExpandError::Missing(_))
        ));
    }

    #[test]
    fn headers_report_missing_variables_together() {
        let headers = vec![
            ("A".to_string(), "${RHO_TEST_MISSING_A}".to_string()),
            ("B".to_string(), "${RHO_TEST_MISSING_B}".to_string()),
        ];
        let err = expand_headers(&headers).unwrap_err();
        assert!(err.contains("RHO_TEST_MISSING_A"), "error was {err}");
        assert!(err.contains("RHO_TEST_MISSING_B"), "error was {err}");
    }
}
