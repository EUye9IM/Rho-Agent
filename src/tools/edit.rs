//! edit 工具：顺序 edits、精确匹配、replace_all、换行归一化与失败相似行提示。

use super::{display_path, resolve_path, truncate_chars, Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::path::Path;

const MAX_EDIT_FILE_BYTES: u64 = 10 * 1024 * 1024;
const CLOSEST_THRESHOLD: f64 = 0.6;
const CLOSEST_SNIPPET_CHARS: usize = 200;
const MAX_MATCH_LINES: usize = 10;

#[derive(Debug, Default)]
pub struct EditTool;

impl Tool for EditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit".to_string(),
            description: "对已有文本文件做精确字符串替换；edits 按顺序应用。".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "文件路径；相对路径按 rho 启动目录解析"
                    },
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_text": {
                                    "type": "string",
                                    "description": "要匹配的原文，默认必须恰好出现一次"
                                },
                                "new_text": {
                                    "type": "string",
                                    "description": "替换后的文本，可以为空"
                                },
                                "replace_all": {
                                    "type": "boolean",
                                    "default": false,
                                    "description": "为 true 时替换全部匹配"
                                }
                            },
                            "required": ["old_text", "new_text"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["path", "edits"],
                "additionalProperties": false
            }),
        }
    }

    fn summary(&self, args: &Value) -> String {
        args.get("path")
            .and_then(Value::as_str)
            .map(|s| truncate_chars(s, 200))
            .unwrap_or_else(|| "…".to_string())
    }

    fn invoke(&self, args: Value, ctx: &ToolContext<'_>) -> ToolResult {
        let args: EditArgs = match serde_json::from_value(args) {
            Ok(args) => args,
            Err(e) => return failure(None, 0, 0, None, format!("参数解析失败：{e}"), None),
        };
        let path = match resolve_path(ctx.cwd, &args.path) {
            Ok(path) => path,
            Err(e) => return failure(None, 0, 0, None, e, None),
        };

        if args.edits.is_empty() {
            return failure(Some(&path), 0, 0, None, "edits 不能为空".to_string(), None);
        }
        let total = args.edits.len();
        for (index, item) in args.edits.iter().enumerate() {
            if item.old_text.is_empty() {
                return failure(
                    Some(&path),
                    0,
                    total,
                    Some(index),
                    "old_text 不能为空".to_string(),
                    None,
                );
            }
            if item.old_text.contains('\0') || item.new_text.contains('\0') {
                return failure(
                    Some(&path),
                    0,
                    total,
                    Some(index),
                    "old_text / new_text 不能包含 NUL 字节".to_string(),
                    None,
                );
            }
        }

        if ctx.cancel.pending().is_some() {
            return interrupted(0, total);
        }
        let bytes = match read_limited(&path) {
            Ok(bytes) => bytes,
            Err(e) => return failure(Some(&path), 0, total, None, e, None),
        };
        if bytes.contains(&0) {
            return failure(
                Some(&path),
                0,
                total,
                None,
                "文件包含 NUL 字节，不是文本文件".to_string(),
                None,
            );
        }
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => {
                return failure(
                    Some(&path),
                    0,
                    total,
                    None,
                    "文件不是有效的 UTF-8 文本".to_string(),
                    None,
                );
            }
        };
        let crlf = detect_crlf(&text);
        if ctx.cancel.pending().is_some() {
            return interrupted(0, total);
        }
        let mut content = normalize_newlines(&text);
        let mut applied = 0usize;
        let mut replacements = 0usize;

        for (index, item) in args.edits.iter().enumerate() {
            if ctx.cancel.pending().is_some() {
                return interrupted(applied, total);
            }
            let old_text = normalize_newlines(&item.old_text);
            let new_text = normalize_newlines(&item.new_text);
            let count = content.match_indices(&old_text).count();
            if count == 0 {
                let closest = closest_line(&content, &old_text);
                return failure(
                    Some(&path),
                    applied,
                    total,
                    Some(index),
                    "old_text 未找到".to_string(),
                    closest,
                );
            }
            if !item.replace_all && count > 1 {
                let (total_matches, lines) = match_line_numbers(&content, &old_text);
                let shown = lines
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                return failure(
                    Some(&path),
                    applied,
                    total,
                    Some(index),
                    format!(
                        "old_text 匹配到 {total_matches} 处（行 {shown}）；请增加上下文或设置 replace_all=true"
                    ),
                    None,
                );
            }

            if item.replace_all {
                content = content.replace(&old_text, &new_text);
                replacements += count;
            } else {
                content = content.replacen(&old_text, &new_text, 1);
                replacements += 1;
            }
            if let Err(e) = write_file(&path, &content, crlf) {
                return failure(
                    Some(&path),
                    applied,
                    total,
                    Some(index),
                    format!("写入文件失败：{e}"),
                    None,
                );
            }
            applied += 1;
            if ctx.cancel.pending().is_some() {
                return interrupted(applied, total);
            }
        }

        let bytes = encoded_len(&content, crlf);
        let path_text = display_path(&path);
        let summary =
            format!("已编辑 {path_text}（{applied} 个 edit，{replacements} 处替换，{bytes} 字节）");
        let output = format!(
            "[status] success\n[path] {path_text}\n[edits] {applied}\n[replacements] {replacements}\n[bytes] {bytes}\n[output]\n{summary}"
        );
        ToolResult::success_with_display(output, summary)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    edits: Vec<EditItem>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditItem {
    old_text: String,
    new_text: String,
    #[serde(default)]
    replace_all: bool,
}

fn read_limited(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = fs::metadata(path).map_err(|e| format!("无法读取文件：{e}"))?;
    if !metadata.is_file() {
        return Err(format!("不是普通文件：{}", path.display()));
    }
    if metadata.len() > MAX_EDIT_FILE_BYTES {
        return Err(format!(
            "文件超过 {} MiB 上限：{}",
            MAX_EDIT_FILE_BYTES / (1024 * 1024),
            path.display()
        ));
    }
    let file = fs::File::open(path).map_err(|e| format!("无法读取文件：{e}"))?;
    let mut bytes = Vec::new();
    file.take(MAX_EDIT_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("无法读取文件：{e}"))?;
    if bytes.len() as u64 > MAX_EDIT_FILE_BYTES {
        return Err(format!(
            "文件超过 {} MiB 上限：{}",
            MAX_EDIT_FILE_BYTES / (1024 * 1024),
            path.display()
        ));
    }
    Ok(bytes)
}

fn detect_crlf(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut crlf = 0usize;
    let mut lf = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            if i > 0 && bytes[i - 1] == b'\r' {
                crlf += 1;
            } else {
                lf += 1;
            }
        }
    }
    crlf > lf
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn encode(content: &str, crlf: bool) -> String {
    if crlf {
        content.replace('\n', "\r\n")
    } else {
        content.to_string()
    }
}

fn encoded_len(content: &str, crlf: bool) -> usize {
    if crlf {
        content
            .chars()
            .map(|ch| if ch == '\n' { 2 } else { ch.len_utf8() })
            .sum()
    } else {
        content.len()
    }
}

fn write_file(path: &Path, content: &str, crlf: bool) -> std::io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)?;
    file.write_all(encode(content, crlf).as_bytes())
}

fn match_line_numbers(content: &str, old_text: &str) -> (usize, Vec<usize>) {
    let mut total = 0usize;
    let mut line = 1usize;
    let mut last = 0usize;
    let mut lines = Vec::new();
    for (pos, _) in content.match_indices(old_text) {
        if total < MAX_MATCH_LINES {
            line += content[last..pos].matches('\n').count();
            lines.push(line);
            last = pos;
        }
        total += 1;
    }
    (total, lines)
}

fn closest_line(content: &str, old_text: &str) -> Option<(usize, String)> {
    let probe = old_text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .max_by_key(|line| line.trim().chars().count())?
        .trim();
    if probe.is_empty() {
        return None;
    }

    let mut best: Option<(f64, usize, &str)> = None;
    for (index, line) in content.lines().enumerate() {
        let score = similarity(probe, line.trim());
        if best.is_none_or(|(best_score, _, _)| score > best_score) {
            best = Some((score, index + 1, line));
        }
    }
    let (score, line_no, line) = best?;
    (score >= CLOSEST_THRESHOLD).then(|| {
        (
            line_no,
            truncate_chars(line.trim_end_matches('\r'), CLOSEST_SNIPPET_CHARS),
        )
    })
}

fn similarity(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let max = a.len().max(b.len());
    if max == 0 {
        return 0.0;
    }
    let prefix = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    (prefix + suffix) as f64 / max as f64
}

fn interrupted(applied: usize, total: usize) -> ToolResult {
    ToolResult::interrupted(format!("已应用 {applied}/{total} 个 edit；用户取消执行"))
}

fn failure(
    path: Option<&Path>,
    applied: usize,
    total: usize,
    edit_index: Option<usize>,
    message: String,
    closest: Option<(usize, String)>,
) -> ToolResult {
    let path_text = path
        .map(display_path)
        .unwrap_or_else(|| "<未解析>".to_string());
    let summary = match edit_index {
        Some(index) => format!(
            "已应用 {applied}/{total} 个 edit；第 {} 个失败：{message}",
            index + 1
        ),
        None => format!("已应用 {applied}/{total} 个 edit；失败：{message}"),
    };
    let mut output = format!(
        "[status] failed\n[path] {path_text}\n[edits_applied] {applied}\n[edits_total] {total}\n[error] {message}\n"
    );
    let mut display = summary.clone();
    if let Some((line_no, text)) = &closest {
        output.push_str(&format!(
            "[closest_line] {line_no}\n[closest_text] {text}\n"
        ));
        display.push_str(&format!("\n最相似行 {line_no}：{text}"));
    }
    output.push_str("[output]\n");
    output.push_str(&summary);
    ToolResult::failure_with_display(output, display)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::Cancel;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir() -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("rho-edit-test-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn invoke(cwd: &Path, args: Value) -> ToolResult {
        let cancel = Cancel::new();
        let ctx = ToolContext {
            cwd,
            cancel: &cancel,
        };
        EditTool.invoke(args, &ctx)
    }

    #[test]
    fn low_similarity_does_not_report_closest_line() {
        let dir = temp_dir();
        fs::write(dir.join("a.txt"), "alpha\nbeta\n").unwrap();
        let out = invoke(
            &dir,
            json!({"path": "a.txt", "edits": [{"old_text": "zzzzzz", "new_text": "x"}]}),
        );
        assert_eq!(out.status, super::super::ToolStatus::Failure);
        assert!(!out.output.contains("[closest_line]"));
        assert!(!out.display.as_deref().unwrap().contains("最相似行"));
    }

    #[test]
    fn replaces_unique_text() {
        let dir = temp_dir();
        fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let out = invoke(
            &dir,
            json!({"path": "a.txt", "edits": [{"old_text": "two", "new_text": "TWO"}]}),
        );
        assert_eq!(out.status, super::super::ToolStatus::Success);
        assert_eq!(
            fs::read_to_string(dir.join("a.txt")).unwrap(),
            "one\nTWO\nthree\n"
        );
        assert!(out.output.contains("[replacements] 1"));
    }

    #[test]
    fn multiple_matches_fail_without_writing() {
        let dir = temp_dir();
        fs::write(dir.join("a.txt"), "x\nx\n").unwrap();
        let out = invoke(
            &dir,
            json!({"path": "a.txt", "edits": [{"old_text": "x", "new_text": "y"}]}),
        );
        assert_eq!(out.status, super::super::ToolStatus::Failure);
        assert!(out.output.contains("匹配到 2 处"));
        assert!(out.output.contains("行 1, 2"));
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "x\nx\n");
    }

    #[test]
    fn replace_all_replaces_every_match() {
        let dir = temp_dir();
        fs::write(dir.join("a.txt"), "x x x").unwrap();
        let out = invoke(
            &dir,
            json!({"path": "a.txt", "edits": [{"old_text": "x", "new_text": "y", "replace_all": true}]}),
        );
        assert_eq!(out.status, super::super::ToolStatus::Success);
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "y y y");
        assert!(out.output.contains("[replacements] 3"));
    }

    #[test]
    fn sequential_edits_see_previous_result() {
        let dir = temp_dir();
        fs::write(dir.join("a.txt"), "a").unwrap();
        let out = invoke(
            &dir,
            json!({"path": "a.txt", "edits": [
                {"old_text": "a", "new_text": "b"},
                {"old_text": "b", "new_text": "c"}
            ]}),
        );
        assert_eq!(out.status, super::super::ToolStatus::Success);
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "c");
    }

    #[test]
    fn later_failure_keeps_earlier_write() {
        let dir = temp_dir();
        fs::write(dir.join("a.txt"), "a").unwrap();
        let out = invoke(
            &dir,
            json!({"path": "a.txt", "edits": [
                {"old_text": "a", "new_text": "b"},
                {"old_text": "missing", "new_text": "c"}
            ]}),
        );
        assert_eq!(out.status, super::super::ToolStatus::Failure);
        assert!(out.output.contains("[edits_applied] 1"));
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "b");
    }

    #[test]
    fn crlf_is_normalized_and_written_back() {
        let dir = temp_dir();
        fs::write(dir.join("a.txt"), "one\r\ntwo\r\n").unwrap();
        let out = invoke(
            &dir,
            json!({"path": "a.txt", "edits": [{"old_text": "one\ntwo", "new_text": "ONE\nTWO"}]}),
        );
        assert_eq!(out.status, super::super::ToolStatus::Success);
        assert_eq!(
            fs::read_to_string(dir.join("a.txt")).unwrap(),
            "ONE\r\nTWO\r\n"
        );
    }

    #[test]
    fn missing_text_reports_closest_line() {
        let dir = temp_dir();
        fs::write(dir.join("a.txt"), "alpha\nfn foo() {\nbeta\n").unwrap();
        // 只差一个字符的探针：精确匹配失败，但第 2 行相似度足够高。
        let out = invoke(
            &dir,
            json!({"path": "a.txt", "edits": [{"old_text": "fn foo() [", "new_text": "x"}] }),
        );
        assert_eq!(out.status, super::super::ToolStatus::Failure);
        assert!(out.output.contains("[closest_line] 2"));
        assert!(out.output.contains("fn foo() {"));
        // 文件未被失败调用改写。
        assert_eq!(
            fs::read_to_string(dir.join("a.txt")).unwrap(),
            "alpha\nfn foo() {\nbeta\n"
        );
    }

    #[test]
    fn empty_old_text_is_rejected() {
        let dir = temp_dir();
        fs::write(dir.join("a.txt"), "a").unwrap();
        let out = invoke(
            &dir,
            json!({"path": "a.txt", "edits": [{"old_text": "", "new_text": "x"}]}),
        );
        assert_eq!(out.status, super::super::ToolStatus::Failure);
        assert!(out.output.contains("old_text 不能为空"));
    }

    #[test]
    fn rejects_unknown_fields_and_empty_edits() {
        let dir = temp_dir();
        fs::write(dir.join("a.txt"), "a").unwrap();
        let out = invoke(&dir, json!({"path": "a.txt", "edits": [], "extra": 1}));
        assert_eq!(out.status, super::super::ToolStatus::Failure);
        assert!(out.output.contains("unknown field"));

        let out = invoke(&dir, json!({"path": "a.txt", "edits": []}));
        assert_eq!(out.status, super::super::ToolStatus::Failure);
        assert!(out.output.contains("edits 不能为空"));
    }

    #[test]
    fn follows_symlink_and_enforces_size_limit() {
        let dir = temp_dir();
        fs::write(dir.join("target.txt"), "a").unwrap();
        std::os::unix::fs::symlink(dir.join("target.txt"), dir.join("link.txt")).unwrap();
        let out = invoke(
            &dir,
            json!({"path": "link.txt", "edits": [{"old_text": "a", "new_text": "b"}]}),
        );
        assert_eq!(out.status, super::super::ToolStatus::Success);
        assert_eq!(fs::read_to_string(dir.join("target.txt")).unwrap(), "b");

        let big = dir.join("big.txt");
        let file = fs::File::create(&big).unwrap();
        file.set_len(MAX_EDIT_FILE_BYTES + 1).unwrap();
        let out = invoke(
            &dir,
            json!({"path": "big.txt", "edits": [{"old_text": "a", "new_text": "b"}]}),
        );
        assert_eq!(out.status, super::super::ToolStatus::Failure);
        assert!(out.output.contains("上限"));
    }

    #[test]
    fn similarity_prefers_prefix_and_suffix() {
        assert!(similarity("fn foo() {", "fn foo() {") > 0.99);
        assert!(similarity("fn foo() {", "fn bar() {") > 0.6);
        assert!(similarity("abc", "xyz") < 0.6);
    }
}
