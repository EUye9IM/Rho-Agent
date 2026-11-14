//! read 工具：全文文本校验、按行读取、行号与双上限截断。

use super::{display_path, resolve_path, truncate_chars, Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

const DEFAULT_LIMIT: usize = 2000;
const MAX_LIMIT: usize = 2000;
const MAX_BODY_CHARS: usize = 50_000;

#[derive(Debug, Default)]
pub struct ReadTool;

impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".to_string(),
            description: "读取一个文本文件；返回带真实行号的内容。".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "文件路径；相对路径按 rho 启动目录解析"
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "default": 0,
                        "description": "0-based 行偏移，默认 0"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_LIMIT,
                        "default": DEFAULT_LIMIT,
                        "description": "最多返回行数，默认 2000，最大 2000"
                    }
                },
                "required": ["path"],
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
        let args: ReadArgs = match serde_json::from_value(args) {
            Ok(args) => args,
            Err(e) => return failure_text(None, format!("参数解析失败：{e}")),
        };
        let path = match resolve_path(ctx.cwd, &args.path) {
            Ok(path) => path,
            Err(e) => return failure_text(None, e),
        };
        let offset = match args.offset.unwrap_or(0) {
            n if n >= 0 => n as usize,
            n => return failure_text(Some(&path), format!("offset 不能为负数：{n}")),
        };
        let limit = match args.limit.unwrap_or(DEFAULT_LIMIT as i64) {
            n if n > 0 && n <= MAX_LIMIT as i64 => n as usize,
            n => {
                return failure_text(
                    Some(&path),
                    format!("limit 必须是 1 到 {MAX_LIMIT} 之间的整数，收到 {n}"),
                );
            }
        };

        if ctx.cancel.pending().is_some() {
            return ToolResult::interrupted("用户取消读取");
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => return failure_text(Some(&path), format!("无法读取文件：{e}")),
        };
        if bytes.contains(&0) {
            return failure_text(Some(&path), "文件包含 NUL 字节，不是文本文件".to_string());
        }
        let content = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => {
                return failure_text(Some(&path), "文件不是有效的 UTF-8 文本".to_string());
            }
        };

        if ctx.cancel.pending().is_some() {
            return ToolResult::interrupted("用户取消读取");
        }
        let lines: Vec<&str> = content.lines().collect();
        if offset > 0 && offset >= lines.len() {
            return failure_text(
                Some(&path),
                format!("offset {offset} 超出文件末尾（共 {} 行）", lines.len()),
            );
        }
        if offset == 0 && lines.is_empty() {
            let output = success_output(&path, 0, 0, 0, false, &[]);
            return ToolResult::success(output);
        }

        let mut body = String::new();
        let mut body_chars = 0usize;
        let mut selected = 0usize;
        let mut next_offset = offset;
        let mut selected_lines: Vec<(usize, &str)> = Vec::new();

        for (index, line) in lines.iter().enumerate().skip(offset).take(limit) {
            if selected.is_multiple_of(256) && ctx.cancel.pending().is_some() {
                return ToolResult::interrupted("用户取消读取");
            }
            let line_chars = line.chars().count();
            if line_chars > MAX_BODY_CHARS {
                return failure_text(
                    Some(&path),
                    format!(
                        "第 {} 行超过 {MAX_BODY_CHARS} 个字符；请用 bash 的 sed / cut / fold 处理",
                        index + 1
                    ),
                );
            }
            let separator = usize::from(selected > 0);
            if body_chars + separator + line_chars > MAX_BODY_CHARS {
                break;
            }
            if selected > 0 {
                body.push('\n');
                body_chars += 1;
            }
            body.push_str(line);
            body_chars += line_chars;
            selected += 1;
            next_offset = index + 1;
            selected_lines.push((index + 1, line));
        }

        let truncated = next_offset < lines.len();
        let output = success_output(
            &path,
            offset,
            selected,
            next_offset,
            truncated,
            &selected_lines,
        );
        ToolResult::success(output)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

fn success_output(
    path: &Path,
    offset: usize,
    selected: usize,
    next_offset: usize,
    truncated: bool,
    selected_lines: &[(usize, &str)],
) -> String {
    let start = offset + 1;
    let end = offset + selected;
    let range = if selected == 0 {
        "0-0".to_string()
    } else {
        format!("{start}-{end}")
    };
    let mut out = format!(
        "[status] success\n[path] {}\n[offset] {offset}\n[lines] {range}\n[truncated] {truncated}\n",
        display_path(path)
    );
    if truncated {
        out.push_str(&format!("[next_offset] {next_offset}\n"));
    }
    out.push_str("[output]\n");
    if selected > 0 {
        let width = end.to_string().len();
        for (line_no, line) in selected_lines {
            out.push_str(&format!("{line_no:>width$} | {line}\n"));
        }
    }
    out
}

fn failure_text(path: Option<&Path>, message: String) -> ToolResult {
    let path_text = path
        .map(display_path)
        .unwrap_or_else(|| "<未解析>".to_string());
    let output =
        format!("[status] failed\n[path] {path_text}\n[error] {message}\n[output]\n{message}");
    ToolResult::failure_with_display(output, message)
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
        let dir = std::env::temp_dir().join(format!("rho-read-test-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn invoke(path: &Path, args: Value) -> ToolResult {
        let cancel = Cancel::new();
        let ctx = ToolContext {
            cwd: path.parent().unwrap(),
            cancel: &cancel,
        };
        ReadTool.invoke(args, &ctx)
    }

    #[test]
    fn handles_crlf_and_rejects_overlong_single_line() {
        let dir = temp_dir();
        let path = dir.join("crlf.txt");
        fs::write(&path, "a\r\nb\r\n").unwrap();
        let out = invoke(&path, json!({"path": path.to_str().unwrap()}));
        assert_eq!(out.status, super::super::ToolStatus::Success);
        assert!(out.output.contains("1 | a"));
        assert!(out.output.contains("2 | b"));
        assert!(!out.output.contains('\r'));

        let long = dir.join("long.txt");
        fs::write(&long, "x".repeat(MAX_BODY_CHARS + 1)).unwrap();
        let out = invoke(&long, json!({"path": long.to_str().unwrap()}));
        assert_eq!(out.status, super::super::ToolStatus::Failure);
        assert!(out.output.contains("超过 50000 个字符"));
    }

    #[test]
    fn reads_with_real_line_numbers_and_offset() {
        let dir = temp_dir();
        let path = dir.join("a.txt");
        fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let out = invoke(
            &path,
            json!({"path": path.file_name().unwrap().to_str().unwrap(), "offset": 1, "limit": 1}),
        );
        assert_eq!(out.status, super::super::ToolStatus::Success);
        assert!(out.output.contains("[lines] 2-2"));
        assert!(out.output.contains("2 | two"));
        assert!(out.display.is_none());
    }

    #[test]
    fn full_file_validation_rejects_binary() {
        let dir = temp_dir();
        let path = dir.join("b.bin");
        fs::write(&path, b"ok\0bad").unwrap();
        let out = invoke(&path, json!({"path": path.to_str().unwrap()}));
        assert_eq!(out.status, super::super::ToolStatus::Failure);
        assert!(out.output.contains("NUL"));
    }

    #[test]
    fn truncates_by_line_limit_and_sets_next_offset() {
        let dir = temp_dir();
        let path = dir.join("c.txt");
        fs::write(&path, "a\nb\nc\n").unwrap();
        let out = invoke(&path, json!({"path": path.to_str().unwrap(), "limit": 2}));
        assert!(out.output.contains("[truncated] true"));
        assert!(out.output.contains("[next_offset] 2"));
        assert!(out.output.contains("1 | a"));
        assert!(out.output.contains("2 | b"));
        assert!(!out.output.contains("3 | c"));
    }

    #[test]
    fn empty_file_is_success() {
        let dir = temp_dir();
        let path = dir.join("empty.txt");
        fs::write(&path, "").unwrap();
        let out = invoke(&path, json!({"path": path.to_str().unwrap()}));
        assert_eq!(out.status, super::super::ToolStatus::Success);
        assert!(out.output.contains("[lines] 0-0"));
    }

    #[test]
    fn rejects_limit_over_max_and_unknown_fields() {
        let dir = temp_dir();
        let path = dir.join("e.txt");
        fs::write(&path, "a\n").unwrap();
        let out = invoke(
            &path,
            json!({"path": path.to_str().unwrap(), "limit": 2001}),
        );
        assert_eq!(out.status, super::super::ToolStatus::Failure);
        assert!(out.output.contains("1 到 2000"));
        let out = invoke(
            &path,
            json!({"path": path.to_str().unwrap(), "extra": true}),
        );
        assert_eq!(out.status, super::super::ToolStatus::Failure);
        assert!(out.output.contains("unknown field"));
    }

    #[test]
    fn offset_past_end_is_failure() {
        let dir = temp_dir();
        let path = dir.join("d.txt");
        fs::write(&path, "a\n").unwrap();
        let out = invoke(&path, json!({"path": path.to_str().unwrap(), "offset": 1}));
        assert_eq!(out.status, super::super::ToolStatus::Failure);
        assert!(out.output.contains("超出文件末尾"));
    }
}
