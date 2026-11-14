//! write 工具：仅新建文件、自动创建父目录、拒绝 NUL、直接写入。

use super::{display_path, resolve_path, truncate_chars, Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Default)]
pub struct WriteTool;

impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".to_string(),
            description: "新建一个文本文件；路径已存在时失败，父目录会自动创建。".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "新文件路径；相对路径按 rho 启动目录解析"
                    },
                    "content": {
                        "type": "string",
                        "description": "要写入的 UTF-8 文本；不自动追加换行"
                    }
                },
                "required": ["path", "content"],
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
        let args: WriteArgs = match serde_json::from_value(args) {
            Ok(args) => args,
            Err(e) => return failure_text(None, format!("参数解析失败：{e}")),
        };
        let path = match resolve_path(ctx.cwd, &args.path) {
            Ok(path) => path,
            Err(e) => return failure_text(None, e),
        };
        if args.content.contains('\0') {
            return failure_text(Some(&path), "content 不能包含 NUL 字节".to_string());
        }
        if ctx.cancel.pending().is_some() {
            return ToolResult::interrupted("用户取消写入");
        }

        match fs::symlink_metadata(&path) {
            Ok(_) => {
                return failure_text(Some(&path), format!("路径已存在：{}", path.display()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return failure_text(Some(&path), format!("无法检查路径：{e}"));
            }
        }

        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            if let Err(e) = fs::create_dir_all(parent) {
                return failure_text(
                    Some(&path),
                    format!("无法创建父目录 {}：{e}", parent.display()),
                );
            }
        }
        if ctx.cancel.pending().is_some() {
            return ToolResult::interrupted("用户取消写入");
        }

        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return failure_text(Some(&path), format!("路径已存在：{}", path.display()));
            }
            Err(e) => return failure_text(Some(&path), format!("无法创建文件：{e}")),
        };
        if let Err(e) = file.write_all(args.content.as_bytes()) {
            return failure_text(Some(&path), format!("写入文件失败：{e}"));
        }

        let bytes = args.content.len();
        let path_text = display_path(&path);
        if ctx.cancel.pending().is_some() {
            return ToolResult::interrupted(format!(
                "已写入 {path_text}（新建，{bytes} 字节；用户取消执行）"
            ));
        }
        let summary = format!("已写入 {path_text}（新建，{bytes} 字节）");
        let output = format!(
            "[status] success\n[path] {path_text}\n[action] created\n[bytes] {bytes}\n[output]\n{summary}"
        );
        ToolResult::success_with_display(output, summary)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
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
        let dir = std::env::temp_dir().join(format!("rho-write-test-{}-{id}", std::process::id()));
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
        WriteTool.invoke(args, &ctx)
    }

    #[test]
    fn writes_empty_content() {
        let dir = temp_dir();
        let out = invoke(&dir, json!({"path": "empty.txt", "content": ""}));
        assert_eq!(out.status, super::super::ToolStatus::Success);
        assert_eq!(fs::read(dir.join("empty.txt")).unwrap(), b"");
        assert!(out.output.contains("[bytes] 0"));
    }

    #[test]
    fn creates_parent_directories_and_file() {
        let dir = temp_dir();
        let out = invoke(&dir, json!({"path": "a/b/c.txt", "content": "hello"}));
        assert_eq!(out.status, super::super::ToolStatus::Success);
        assert_eq!(fs::read_to_string(dir.join("a/b/c.txt")).unwrap(), "hello");
        assert!(out.output.contains("[action] created"));
        assert_eq!(fs::read_dir(dir.join("a/b")).unwrap().count(), 1);
    }

    #[test]
    fn refuses_existing_path() {
        let dir = temp_dir();
        fs::write(dir.join("x"), "old").unwrap();
        let out = invoke(&dir, json!({"path": "x", "content": "new"}));
        assert_eq!(out.status, super::super::ToolStatus::Failure);
        assert!(out.output.contains("路径已存在"));
        assert_eq!(fs::read_to_string(dir.join("x")).unwrap(), "old");
    }

    #[test]
    fn rejects_nul_before_creating_directories() {
        let dir = temp_dir();
        let out = invoke(&dir, json!({"path": "new/dir/x", "content": "a\u{0}b"}));
        assert_eq!(out.status, super::super::ToolStatus::Failure);
        assert!(!dir.join("new").exists());
    }

    #[test]
    fn rejects_unknown_fields() {
        let dir = temp_dir();
        let out = invoke(&dir, json!({"path": "x", "content": "a", "extra": 1}));
        assert_eq!(out.status, super::super::ToolStatus::Failure);
        assert!(out.output.contains("unknown field"));
        assert!(!dir.join("x").exists());
    }

    #[test]
    fn rejects_broken_symlink_as_existing() {
        let dir = temp_dir();
        std::os::unix::fs::symlink(dir.join("missing"), dir.join("link")).unwrap();
        let out = invoke(&dir, json!({"path": "link", "content": "x"}));
        assert_eq!(out.status, super::super::ToolStatus::Failure);
        assert!(out.output.contains("路径已存在"));
    }
}
