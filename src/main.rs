//! rho — 轻量本地 agent REPL。库入口见 lib.rs；本文件只做参数解析、退出码、组装启动。

use rho::chat::Session;
use rho::config;
use rho::provider;
use rho::repl::{self, ReplExit};
use rho::term;
use rho::ui;
use std::process::ExitCode;

/// CLI 参数（手写解析，不用 clap）。见 design.md 四节。
pub struct Args {
    pub config: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub yes: bool,
}

/// 退出码：0 正常 / 1 REPL 无法继续的内部错误 / 2 配置或用法错误（含 stdin 非 TTY）。
const EXIT_OK: u8 = 0;
const EXIT_INTERNAL: u8 = 1;
const EXIT_USAGE: u8 = 2;

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1).collect()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("用法错误：{e}\n用法：rho [--config <path>] [--provider <id>] [--model <id>] [--thinking <level>] [--yes]");
            return ExitCode::from(EXIT_USAGE);
        }
    };

    let code = run(args);
    ExitCode::from(code)
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

    // 供应商启用与默认值冲突消解（design.md 五节）。
    let selection = match config::resolve_selection(&cfg, args.provider.as_deref(), args.model.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_USAGE;
        }
    };

    // --thinking 非法（不属于所选模型档位）在构造 Session 前校验（exit 2）。
    let thinking = match resolve_thinking(&cfg, &selection, args.thinking.as_deref()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_USAGE;
        }
    };

    let adapter = match provider::adapter_for_protocol(&selection.provider.protocol) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_USAGE;
        }
    };

    let mut term_ui = match ui::TermUi::new() {
        Ok(u) => u,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_USAGE;
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

    let session = Session::new(cfg, selection, adapter, args.yes, thinking);
    match repl::run(&mut term_ui, session, &cancel) {
        Ok(()) | Err(ReplExit::Quit) => EXIT_OK,
        Err(ReplExit::Fatal(e)) => {
            eprintln!("内部错误：{e}");
            EXIT_INTERNAL
        }
    }
}

/// 解析 --thinking：None → 所选模型 thinking_default；Some(档位) → 须属该模型档位表。
fn resolve_thinking(
    cfg: &config::Config,
    sel: &config::Selection,
    cli: Option<&str>,
) -> Result<Option<String>, String> {
    // TODO: 有 cli 时校验属所选模型 thinking 档位；无 thinking 表的模型不接受非空 --thinking
    let _ = (cfg, sel, cli);
    todo!()
}

/// 手写参数解析：支持 `--flag value` 与 `--flag=value`；未知参数/重复 flag/带值的 --yes 报错。
fn parse_args(argv: Vec<String>) -> Result<Args, String> {
    let mut args = Args { config: None, provider: None, model: None, thinking: None, yes: false };
    let mut it = argv.into_iter().peekable();
    while let Some(a) = it.next() {
        let (flag, inline): (&str, Option<String>) = match a.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (a.as_str(), None),
        };
        let mut take_value = || -> Result<String, String> {
            inline.clone().map(Ok).unwrap_or_else(|| {
                it.next().ok_or_else(|| format!("缺少 {flag} 的值"))
            })
        };
        match flag {
            "--config" => {
                if args.config.is_some() { return Err("--config 重复".into()); }
                args.config = Some(take_value()?);
            }
            "--provider" => {
                if args.provider.is_some() { return Err("--provider 重复".into()); }
                args.provider = Some(take_value()?);
            }
            "--model" => {
                if args.model.is_some() { return Err("--model 重复".into()); }
                args.model = Some(take_value()?);
            }
            "--thinking" => {
                if args.thinking.is_some() { return Err("--thinking 重复".into()); }
                args.thinking = Some(take_value()?);
            }
            "--yes" => {
                if inline.is_some() { return Err("--yes 不接受值".into()); }
                if args.yes { return Err("--yes 重复".into()); }
                args.yes = true;
            }
            other => return Err(format!("未知参数 {other}")),
        }
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let a = parse_args(["--provider".into(), "deepseek".into(), "--yes".into()].to_vec()).unwrap();
        assert_eq!(a.provider.as_deref(), Some("deepseek"));
        assert!(a.yes);
    }

    #[test]
    fn parse_inline_value() {
        let a = parse_args(["--config=/tmp/x.toml".into()].to_vec()).unwrap();
        assert_eq!(a.config.as_deref(), Some("/tmp/x.toml"));
    }

    #[test]
    fn parse_rejects_unknown() {
        assert!(parse_args(["--clap".into()].to_vec()).is_err());
    }

    #[test]
    fn parse_rejects_duplicate() {
        assert!(parse_args(["--yes".into(), "--yes".into()].to_vec()).is_err());
        assert!(parse_args(["--provider".into(), "a".into(), "--provider".into(), "b".into()].to_vec()).is_err());
    }

    #[test]
    fn parse_rejects_yes_with_value() {
        assert!(parse_args(["--yes=false".into()].to_vec()).is_err());
    }
}

