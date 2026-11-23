//! rho — 轻量本地 agent REPL。库入口见 lib.rs；本文件只做参数解析、退出码、组装启动。

use rho::chat::Session;
use rho::config;
use rho::provider;
use rho::render::{self, RenderEvent};
use rho::repl::{self, ReplExit};
use rho::store;
use rho::term;
use rho::ui::{self, Ui};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// CLI 参数（手写解析）。
pub struct Args {
    pub config: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub continue_session: bool,
    pub init: bool,
    pub version: bool,
}

/// 退出码：0 正常 / 1 REPL 无法继续的内部错误 / 2 配置或用法错误（含 stdin 非 TTY）。
const EXIT_OK: u8 = 0;
const EXIT_INTERNAL: u8 = 1;
const EXIT_USAGE: u8 = 2;

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1).collect()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("用法错误：{e}\n用法：rho [--config <path>] [--provider <id>] [--model <id>] [--thinking <level>] [-c|--continue] [--init] [--version]");
            return ExitCode::from(EXIT_USAGE);
        }
    };

    if args.version {
        println!("rho {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    if args.init {
        return match init_config(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::from(EXIT_USAGE)
            }
        };
    }

    let code = run(args);
    ExitCode::from(code)
}

/// `--init`：把内置配置写到 `--config` 指定路径或默认路径；目标已存在则报错不覆盖。
fn init_config(args: &Args) -> Result<(), String> {
    let path = match &args.config {
        Some(p) => PathBuf::from(p),
        None => config::default_config_path()
            .ok_or_else(|| "无法确定默认配置路径：HOME 与 XDG_CONFIG_HOME 均未设置".to_string())?,
    };
    config::write_builtin_config(&path)?;
    println!("已生成配置文件：{}", path.display());
    Ok(())
}

/// 组装启动：加载配置 → 选择 provider/model → 构造 TermUi（TTY 检查在此）→ 进 REPL。
fn run(args: Args) -> u8 {
    let cfg = match config::load(&args.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_USAGE;
        }
    };

    // DoH 解析器：配置了 dns 时优先使用（端点在配置里声明，校验已通过）。
    provider::init_dns(&cfg.dns);

    // --thinking 非法（不属于所选模型档位）在构造 Session 前校验（exit 2）；
    // 合法则写入 selection.thinking（唯一真值源）。
    let mut selection =
        match config::resolve_selection(&cfg, args.provider.as_deref(), args.model.as_deref()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{e}");
                return EXIT_USAGE;
            }
        };
    if let Err(e) = resolve_thinking(&mut selection, args.thinking.as_deref()) {
        eprintln!("{e}");
        return EXIT_USAGE;
    }

    let adapter = match provider::adapter_for(&selection.provider) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_USAGE;
        }
    };

    let mut term_ui = match ui::TermUi::new() {
        Ok(u) => u,
        Err(ui::TermUiInitError::NotTty(e)) => {
            eprintln!("{e}");
            return EXIT_USAGE;
        }
        Err(ui::TermUiInitError::Init(e)) => {
            eprintln!("{e}");
            return EXIT_INTERNAL;
        }
    };

    // 信号线程（self-pipe 方案）在进入 REPL 前启动。
    let cancel = match term::init() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_INTERNAL;
        }
    };

    let mut session = Session::new(cfg, selection, adapter);
    if let Some(path) = store::default_path() {
        if args.continue_session {
            resume(&mut session, &mut term_ui, &path);
        }
        session = session.with_store(path);
    } else if args.continue_session {
        warn(
            &mut term_ui,
            "无法确定会话文件路径：HOME 与 XDG_STATE_HOME 均未设置",
        );
    }

    match repl::run(&mut term_ui, session, &cancel) {
        Ok(()) | Err(ReplExit::Quit) => EXIT_OK,
        Err(ReplExit::Fatal(e)) => {
            eprintln!("内部错误：{e}");
            EXIT_INTERNAL
        }
    }
}

/// `-c`：加载唯一会话文件并重放。恢复失败不阻塞启动。
fn resume(session: &mut Session, ui: &mut dyn Ui, path: &Path) {
    match store::load(path) {
        Ok(history) => {
            let total = history.len();
            let turns = history
                .iter()
                .filter(|msg| matches!(msg, rho::message::Msg::User(_)))
                .count();
            session.replace_history(history);
            session.replay(ui);
            warn(ui, &format!("已恢复 {total} 条消息（{turns} 轮）"));
        }
        Err(store::LoadError::Missing) => warn(ui, "没有可恢复的会话，已开始新会话"),
        Err(store::LoadError::Corrupt(e)) => {
            let detail = match store::backup_corrupt(path) {
                Ok(backup) => format!("{e}；已备份为 {}", backup.display()),
                Err(backup_err) => format!("{e}；{backup_err}"),
            };
            warn(ui, &detail);
        }
    }
}

/// 不以退出码表达的状态提示：暗淡一行，走正常输出流。
fn warn(ui: &mut dyn Ui, message: &str) {
    ui.print(&RenderEvent::TextDelta(format!(
        "{}\n",
        render::dim(&format!("[{message}]"))
    )));
}

/// 解析 --thinking：None → 保持 selection.thinking（模型 thinking_default）；/// Some(档位) → 须属该模型档位表，写入 selection.thinking。
fn resolve_thinking(selection: &mut config::Selection, cli: Option<&str>) -> Result<(), String> {
    let Some(level) = cli else { return Ok(()) };
    // 与 Session::switch_thinking 共用判定（config::thinking_level_err）；CLI 文案附加提示。
    match config::thinking_level_err(&selection.model, &selection.model_id, level) {
        Some(e) => Err(format!("{e}，不能使用 --thinking {level}")),
        None => {
            selection.thinking = Some(level.to_string());
            Ok(())
        }
    }
}

/// 手写参数解析：支持 `--flag value` 与 `--flag=value`；未知参数/重复 flag/带值的 flag 报错。
fn parse_args(argv: Vec<String>) -> Result<Args, String> {
    let mut args = Args {
        config: None,
        provider: None,
        model: None,
        thinking: None,
        continue_session: false,
        init: false,
        version: false,
    };
    let mut it = argv.into_iter().peekable();
    while let Some(a) = it.next() {
        let (flag, inline): (&str, Option<String>) = match a.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (a.as_str(), None),
        };
        let mut take_value = || -> Result<String, String> {
            inline
                .clone()
                .map(Ok)
                .unwrap_or_else(|| it.next().ok_or_else(|| format!("缺少 {flag} 的值")))
        };
        match flag {
            "--config" => {
                if args.config.is_some() {
                    return Err("--config 重复".into());
                }
                args.config = Some(take_value()?);
            }
            "--provider" => {
                if args.provider.is_some() {
                    return Err("--provider 重复".into());
                }
                args.provider = Some(take_value()?);
            }
            "--model" => {
                if args.model.is_some() {
                    return Err("--model 重复".into());
                }
                args.model = Some(take_value()?);
            }
            "--thinking" => {
                if args.thinking.is_some() {
                    return Err("--thinking 重复".into());
                }
                args.thinking = Some(take_value()?);
            }
            "--init" => {
                if inline.is_some() {
                    return Err("--init 不接受值".into());
                }
                if args.init {
                    return Err("--init 重复".into());
                }
                args.init = true;
            }
            "-c" | "--continue" => {
                if inline.is_some() {
                    return Err("--continue 不接受值".into());
                }
                if args.continue_session {
                    return Err("--continue 重复".into());
                }
                args.continue_session = true;
            }
            "--version" | "-V" => {
                if inline.is_some() {
                    return Err("--version 不接受值".into());
                }
                if args.version {
                    return Err("--version 重复".into());
                }
                args.version = true;
            }
            other => return Err(format!("未知参数 {other}")),
        }
    }
    if args.init && args.continue_session {
        return Err("--init 与 --continue 不能同时使用".into());
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let a = parse_args(["--provider".into(), "deepseek".into()].to_vec()).unwrap();
        assert_eq!(a.provider.as_deref(), Some("deepseek"));
    }

    #[test]
    fn parse_inline_value() {
        let a = parse_args(["--config=/tmp/x.toml".into()].to_vec()).unwrap();
        assert_eq!(a.config.as_deref(), Some("/tmp/x.toml"));
    }

    #[test]
    fn parse_version_flags() {
        assert!(parse_args(["--version".into()].to_vec()).unwrap().version);
        assert!(parse_args(["-V".into()].to_vec()).unwrap().version);
        assert!(parse_args(["--version=1".into()].to_vec()).is_err());
        assert!(parse_args(["--version".into(), "-V".into()].to_vec()).is_err());
    }

    #[test]
    fn parse_init_flag() {
        assert!(parse_args(["--init".into()].to_vec()).unwrap().init);
        assert!(parse_args(["--init=1".into()].to_vec()).is_err());
        assert!(parse_args(["--init".into(), "--init".into()].to_vec()).is_err());
    }

    #[test]
    fn init_writes_config_and_refuses_overwrite() {
        let dir = std::env::temp_dir().join(format!("rho-main-init-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("rho.toml");
        let args = Args {
            config: Some(path.display().to_string()),
            provider: None,
            model: None,
            thinking: None,
            continue_session: false,
            init: true,
            version: false,
        };

        init_config(&args).unwrap();
        assert!(path.exists());
        assert!(init_config(&args).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_continue_flags() {
        assert!(parse_args(["-c".into()].to_vec()).unwrap().continue_session);
        assert!(
            parse_args(["--continue".into()].to_vec())
                .unwrap()
                .continue_session
        );
        assert!(parse_args(["--continue=1".into()].to_vec()).is_err());
        assert!(parse_args(["-c".into(), "--continue".into()].to_vec()).is_err());
    }

    #[test]
    fn continue_conflicts_with_init() {
        match parse_args(["--init".into(), "-c".into()].to_vec()) {
            Err(e) => assert!(e.contains("--init"), "error was {e}"),
            Ok(_) => panic!("expected --init 与 --continue 互斥"),
        }
    }

    #[test]
    fn parse_rejects_unknown() {
        assert!(parse_args(["--clap".into()].to_vec()).is_err());
        assert!(parse_args(["--yes".into()].to_vec()).is_err());
    }

    #[test]
    fn parse_rejects_duplicate() {
        assert!(parse_args(
            [
                "--provider".into(),
                "a".into(),
                "--provider".into(),
                "b".into()
            ]
            .to_vec()
        )
        .is_err());
    }
}
