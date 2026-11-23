//! 会话文件：单文件全量快照，写入走「临时文件 + rename」。
//!
//! 文件内容就是 `[Msg]` 的 serde 表示（见 `message`），没有版本字段；
//! 读取失败时调用方决定是否备份原文件。

use crate::message::Msg;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 默认会话文件：`$XDG_STATE_HOME/rho/session.json`；
/// `XDG_STATE_HOME` 未设置或为空时用 `$HOME/.local/state/rho/session.json`。
pub fn default_path() -> Option<PathBuf> {
    default_path_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

fn default_path_from(xdg_state_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_state_home.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("rho").join("session.json"));
    }
    home.filter(|v| !v.is_empty()).map(|home| {
        PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("rho")
            .join("session.json")
    })
}

/// 读取会话文件。文件不存在与内容损坏分开返回，让调用方决定是否提示。
#[derive(Debug)]
pub enum LoadError {
    /// 文件不存在（首次启动、上次没有产生历史）
    Missing,
    /// 读取或解析失败
    Corrupt(String),
}

/// 加载会话文件；成功时返回完整历史（含 `Msg::System` 与 `Msg::Summary`）。
pub fn load(path: &Path) -> Result<Vec<Msg>, LoadError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(LoadError::Missing),
        Err(e) => {
            return Err(LoadError::Corrupt(format!(
                "读取会话文件失败（{}）：{e}",
                path.display()
            )))
        }
    };
    serde_json::from_slice(&bytes)
        .map_err(|e| LoadError::Corrupt(format!("会话文件格式错误（{}）：{e}", path.display())))
}

/// 写入全量快照：先写同目录临时文件，再 rename 覆盖。
/// rename 在同一文件系统内是原子的，最差情况是丢失本次写入，不会留下半份文件。
pub fn save(path: &Path, history: &[Msg]) -> Result<(), String> {
    let json = serde_json::to_vec(history).map_err(|e| format!("序列化会话失败：{e}"))?;
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        fs::create_dir_all(dir)
            .map_err(|e| format!("创建会话目录失败（{}）：{e}", dir.display()))?;
    }
    let tmp = with_suffix(path, ".tmp");
    fs::write(&tmp, &json)
        .map_err(|e| format!("写入会话临时文件失败（{}）：{e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| {
        // rename 失败时临时文件没有意义，尽量清掉，避免下次误用。
        let _ = fs::remove_file(&tmp);
        format!("替换会话文件失败（{}）：{e}", path.display())
    })
}

/// 把无法解析的会话文件改名为 `<原名>.bad`，返回备份路径。
pub fn backup_corrupt(path: &Path) -> Result<PathBuf, String> {
    let backup = with_suffix(path, ".bad");
    fs::rename(path, &backup)
        .map_err(|e| format!("备份损坏的会话文件失败（{}）：{e}", path.display()))?;
    Ok(backup)
}

/// 在完整路径后追加后缀：`session.json` → `session.json.tmp`。
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{AssistantMsg, ToolCall, ToolStatus};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rho-store-{}-{}-{}.json",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst),
            name
        ))
    }

    fn sample() -> Vec<Msg> {
        vec![
            Msg::System("sys".into()),
            Msg::User("你好".into()),
            Msg::Summary("早先的摘要".into()),
            Msg::Assistant(AssistantMsg {
                text: Some("回答".into()),
                reasoning: Some("思考".into()),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "bash".into(),
                    arguments: "{\"command\":\"ls\"}".into(),
                }],
            }),
            Msg::Tool {
                tool_call_id: "call_1".into(),
                name: "bash".into(),
                content: "ok".into(),
                display: Some("ok".into()),
                status: ToolStatus::Failure,
            },
        ]
    }

    #[test]
    fn round_trips_every_message_variant() {
        let path = temp_path("roundtrip");
        let history = sample();
        save(&path, &history).unwrap();

        let loaded = load(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(loaded.len(), history.len());
        assert!(matches!(&loaded[0], Msg::System(s) if s == "sys"));
        assert!(matches!(&loaded[2], Msg::Summary(s) if s == "早先的摘要"));
        match &loaded[3] {
            Msg::Assistant(a) => {
                assert_eq!(a.text.as_deref(), Some("回答"));
                assert_eq!(a.reasoning.as_deref(), Some("思考"));
                assert_eq!(a.tool_calls[0].name, "bash");
            }
            other => panic!("expected assistant, got {other:?}"),
        }
        assert!(matches!(
            &loaded[4],
            Msg::Tool { name, status: ToolStatus::Failure, .. } if name == "bash"
        ));
    }

    #[test]
    fn save_leaves_no_temp_file() {
        let path = temp_path("notmp");
        save(&path, &sample()).unwrap();

        assert!(path.exists());
        assert!(!with_suffix(&path, ".tmp").exists());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn creates_parent_directory() {
        let dir = std::env::temp_dir().join(format!(
            "rho-store-dir-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let path = dir.join("nested").join("session.json");
        save(&path, &sample()).unwrap();
        assert!(path.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_its_own_error() {
        let path = temp_path("missing");
        assert!(matches!(load(&path), Err(LoadError::Missing)));
    }

    #[test]
    fn corrupt_file_reports_path() {
        let path = temp_path("corrupt");
        std::fs::write(&path, b"{not json").unwrap();

        match load(&path) {
            Err(LoadError::Corrupt(e)) => {
                assert!(e.contains(&path.display().to_string()), "error was {e}")
            }
            other => panic!("expected corrupt error, got {other:?}"),
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn backup_moves_corrupt_file_aside() {
        let path = temp_path("backup");
        std::fs::write(&path, b"broken").unwrap();

        let backup = backup_corrupt(&path).unwrap();
        assert!(!path.exists());
        assert_eq!(backup, with_suffix(&path, ".bad"));
        assert_eq!(std::fs::read(&backup).unwrap(), b"broken");

        let _ = std::fs::remove_file(&backup);
    }

    #[test]
    fn default_path_prefers_xdg_state_home() {
        let path = default_path_from(Some("/xdg".into()), Some("/home/u".into())).unwrap();
        assert_eq!(path, PathBuf::from("/xdg/rho/session.json"));
    }

    #[test]
    fn default_path_falls_back_to_home() {
        let path = default_path_from(None, Some("/home/u".into())).unwrap();
        assert_eq!(path, PathBuf::from("/home/u/.local/state/rho/session.json"));

        // 空字符串按未设置处理。
        let path = default_path_from(Some("".into()), Some("/home/u".into())).unwrap();
        assert_eq!(path, PathBuf::from("/home/u/.local/state/rho/session.json"));
    }

    #[test]
    fn default_path_requires_some_home() {
        assert!(default_path_from(None, None).is_none());
        assert!(default_path_from(Some("".into()), Some("".into())).is_none());
    }
}
