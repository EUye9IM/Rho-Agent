# rho

[![test](https://github.com/EUye9IM/Rho-Agent/actions/workflows/test.yml/badge.svg?branch=master)](https://github.com/EUye9IM/Rho-Agent/actions/workflows/test.yml)

**English** | [简体中文](README.zh-CN.md)

`rho` is a small local agent that lives in your terminal. It is a single static binary, has no async runtime, and is built for resource-constrained Linux boxes. You talk to it in a REPL; it streams answers from an OpenAI Chat Completions–compatible provider (DeepSeek by default) and can run shell commands through a `bash` tool gated by an allowlist plus an explicit confirmation.

Design docs: [design.md](design.md) and [architecture.md](architecture.md) (both in Chinese).

## Features

- **Small by design** — one crate, one binary, blocking I/O + threads instead of tokio/reqwest; `strip` + LTO + `panic = "abort"` in release.
- **Config-driven providers** — providers, models and thinking levels are data, not code. Same protocol family means config only; no hardcoded vendor branches.
- **Streaming with visible reasoning** — text streams as it arrives; `reasoning_content` is rendered dimmed and is **not** sent back to the API unless the provider sets `compat.echo_reasoning = true`.
- **One tool: `bash`** — executed in the foreground as `bash -c <command>`, no persistent shell, per-call timeout with escalating process-group kill.
- **Predictable Ctrl-C** — first Ctrl-C cancels the current stream/tool/confirmation, second one exits; the conversation history is always left in a protocol-valid state.
- **Strict config validation** — unknown fields, dangling references and missing required fields fail loudly with a location, exit code 2.

## Requirements

- Linux: `x86_64`, `aarch64` or `armv7` (musl static targets are the intended distribution form). Windows and macOS are not supported.
- Rust 1.75 or newer (MSRV) to build.
- An interactive TTY — `rho` refuses to start when stdin is not a TTY (exit code 2). There is no pipe/stdin mode.

## Build

```sh
cargo build --release --locked
./target/release/rho
```

Static musl builds:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --locked --target x86_64-unknown-linux-musl
# aarch64-unknown-linux-musl and armv7-unknown-linux-musleabihf work the same way
```

## Usage

```sh
export DEEPSEEK_API_KEY=sk-...
rho
```

```
rho> check disk usage under /var/log
→ bash: du -sh /var/log/* | sort -h
...
```

Pick a non-default provider, model or thinking level, or skip confirmations:

```sh
rho --config ./rho.toml --model deepseek/deepseek-v4-pro --thinking low
rho --yes            # auto-approve every `bash` call
```

### CLI

| Flag | Meaning |
|---|---|
| `--config <path>` | Use an external config. The built-in config is **ignored entirely** — no merging. |
| `--provider <id>` | Initial provider (overrides config default). |
| `--model <id>` | Initial model (must be `provider/model`, overrides config default). |
| `--thinking <level>` | Initial thinking level (must exist for the chosen model). |
| `--yes` | Skip all `bash` confirmations. |

Config comes only from `--config`: no environment-variable config, no default-path discovery. The working directory is fixed to the directory `rho` was started in.

### REPL commands

| Command | Meaning |
|---|---|
| `/model [id]` | Switch model, or list models of the current provider with the active one marked. |
| `/provider [id]` | Switch provider, or list providers (unavailable ones are annotated with the missing variables). |
| `/thinking [level]` | Switch thinking level, or list levels for the current model. Hidden when the model declares none. |
| `/clear` | Drop the conversation history. |
| `/help` | Show help. |
| `/exit` | Quit (Ctrl-D and Ctrl-C on an empty line also exit cleanly). |

Switching only affects subsequent requests; history is never replayed. If a `/provider` switch leaves the current model outside the new provider, the model resets to the first model declared by that provider.

## Configuration

The built-in config only contains DeepSeek (`deepseek/deepseek-flash`, `deepseek/deepseek-v4-pro`). A user config must be self-contained:

```toml
default_provider = "deepseek"
default_model    = "deepseek/deepseek-v4-pro"   # wins over default_provider on conflict
system_prompt    = "You are rho, a lightweight local ops assistant."  # omit to send no system message
max_tool_steps   = 25

[providers.deepseek]
protocol = "openai_chat"                        # the only protocol implemented in v1
base_url = "https://api.deepseek.com"

[providers.deepseek.auth]
headers = { Authorization = "Bearer ${DEEPSEEK_API_KEY}" }

[providers.deepseek.compat]
echo_reasoning = false

[models."deepseek/deepseek-v4-pro"]
context_window   = 1000000
max_output       = 65536
thinking_default = "high"
# api_model = "..."  # optional provider-side name override

[models."deepseek/deepseek-v4-pro".thinking]
off  = { thinking = { type = "disabled" } }
low  = { thinking = { type = "enabled" }, reasoning_effort = "low" }
high = { thinking = { type = "enabled" }, reasoning_effort = "high" }
max  = { thinking = { type = "enabled" }, reasoning_effort = "max" }

[tools.bash]
allow            = ["ls", "cat /var/log", "systemctl status"]  # empty = confirm everything
timeout_seconds  = 120
max_output_bytes = 16384
```

Rules worth knowing:

- Model ids are global and must carry a provider prefix (`deepseek/deepseek-v4-pro`); the prefix must match a declared provider. The upstream `model` field defaults to the part after the first `/`.
- `${VAR}` in auth headers is expanded per provider. A missing variable only marks that provider unavailable; it is not fatal for the rest, unless the provider was explicitly requested.
- Thinking levels are arbitrary JSON fragments merged into the top level of the request body. Level names are yours to choose.
- Validation is strict: unknown fields, dangling references and missing required fields abort startup with exit code 2 and a TOML location.

## The `bash` tool and its safety model

`rho` runs `bash -c <command>` in the foreground, with no persistent shell between calls (no shared `cwd`, no shared env, no surviving background state — chain `cd` and the command in one line if you need both).

A command runs without asking only when **both** hold:

1. It is exactly an `allow` entry, or starts with one immediately followed by a space; and
2. it contains none of these shell metacharacters: `;` `&` `|` `<` `>` `$`, a backtick, or a newline.

Everything else prompts with the working directory and the full command, defaulting to **N**; a rejection is fed back to the model as a tool result. `--yes` skips all prompts.

> **This is not a sandbox.** The allowlist is a temporary string-matching heuristic, not shell parsing or a security boundary. Child processes inherit the full environment, so the model can read `DEEPSEEK_API_KEY` via `env`. Run `rho` as a user you are willing to have commands executed as, and do not point it at hosts you do not trust to be read.

Other limits: at most `max_tool_steps` model-initiated tool calls per turn (default 25, counting rejected/failed calls too); output is capped at `max_output_bytes` with the head and tail kept and the omitted byte count marked; a timeout kills the whole process group (SIGTERM, then SIGKILL after 2s). No retries on HTTP errors, no context-budget management — an over-long conversation fails upstream and you should `/clear`.

## Testing

```sh
cargo test --all-targets --locked
```

CI runs the same command on every push to `master` and every pull request. Tests never touch the network: protocol tests use a hand-written local SSE mock over `TcpListener`, and the REPL/agent-loop tests drive the real loop with a scripted `Ui` implementation. The signal → record path of `term.rs` and live terminal behaviour are verified manually.

## Not in v1

No one-shot mode, no stdin piping, no session persistence, no file-editing tools, no MCP, no sub-agents, no context compaction/tokenizer, no HTTP retries, no protocol families beyond OpenAI Chat Completions, no Windows/macOS support.

## License

MIT
