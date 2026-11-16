//! 文本工具函数：按 Unicode 字符截断。无内部依赖，可被任意层使用。

/// 按 Unicode 字符截断，超出时补省略号。
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
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
    fn truncate_keeps_short_text_as_is() {
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(truncate_chars("", 10), "");
    }

    #[test]
    fn truncate_appends_ellipsis_beyond_limit() {
        assert_eq!(truncate_chars("abcdef", 3), "abc…");
        // 恰好等于上限不截断。
        assert_eq!(truncate_chars("abc", 3), "abc");
    }

    #[test]
    fn truncate_counts_unicode_chars_not_bytes() {
        assert_eq!(truncate_chars("中文测试", 2), "中文…");
    }
}
