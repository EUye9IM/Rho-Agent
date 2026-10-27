# rho

[![test](https://github.com/EUye9IM/Rho-Agent/actions/workflows/test.yml/badge.svg?branch=master)](https://github.com/EUye9IM/Rho-Agent/actions/workflows/test.yml)

**English** | [简体中文](README.zh-CN.md)

`rho` is a small local agent that lives in your terminal. It is a single binary (musl static builds are the intended distribution form), uses blocking I/O and threads, and is built for resource-constrained Linux boxes. You talk to it in a REPL; it streams answers from an OpenAI Chat Completions–compatible provider (DeepSeek by default) and can run shell commands through a `bash` tool gated by an allowlist plus an explicit confirmation.

## Vision

rho should stay a small, predictable local agent for Linux boxes with limited resources: one binary, providers declared in TOML, one `bash` tool, and a REPL whose history stays valid through cancellation.

Implementation: [docs/architecture.md](docs/architecture.md).

## Features

- **Small by design** — one crate, one binary, blocking I/O + threads; `strip` + LTO + `panic = "abort"` in release.
- **Config-driven providers** — providers, models and thinking levels are declared as data. Same protocol family means configuration only.
- **Streaming with visible reasoning** — text streams as it arrives; `reasoning_content` is rendered dimmed and stays local unless the provider sets `compat.echo_reasoning = true`.
- **One tool: `bash`** — executed in the foreground as `bash -c <command>`, with a fresh shell per call and a per-call timeout that escalates to a process-group kill.
- **Predictable Ctrl-C** — first Ctrl-C cancels the current stream/tool/confirmation and returns you to the prompt (cancellation takes effect between chunks; if the connection stalls, the background reader ends once the next data arrives or the idle timeout hits), second one exits; at the idle prompt two empty-line Ctrl-C within two seconds exit (the first shows a gray hint). The conversation history is always left in a protocol-valid state.
- **Strict config validation** — unknown fields, dangling references and missing required fields fail loudly with the config path (TOML syntax errors also include a line number), exit code 2.

## Requirements

- Linux: `x86_64`, `aarch64` or `armv7` (musl static targets are the intended distribution form).
- A recent stable Rust toolchain to build (CI tests on stable).
- An interactive TTY — `rho` requires one and exits with code 2 when stdin is not a TTY. It runs as a REPL.

## Build

```sh
cargo build --release --locked
./target/release/rho
```

Static musl builds:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --locked --target x86_64-unknown-linux-musl

# aarch64 / armv7 use the same command after installing the target and a
# suitable cross linker/toolchain, e.g. aarch64-linux-gnu-gcc or
# arm-linux-gnueabihf-gcc:
rustup target add aarch64-unknown-linux-musl
cargo build --release --locked --target aarch64-unknown-linux-musl
rustup target add armv7-unknown-linux-musleabihf
cargo build --release --locked --target armv7-unknown-linux-musleabihf
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
| `--config <path>` | Use an external config. It replaces the built-in config entirely. |
| `--provider <id>` | Initial provider (overrides config default). |
| `--model <id>` | Initial model (must be `provider/model`, overrides config default). |
| `--thinking <level>` | Initial thinking level (must exist for the chosen model). |
| `--yes` | Skip all `bash` confirmations. |

User config comes only from `--config`; otherwise the built-in config is used. `${VAR}` in auth headers is expanded from the environment. `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` and `NO_PROXY` control proxy routing; `SSL_CERT_FILE`, when set, replaces the built-in TLS roots. The working directory is fixed to the directory `rho` was started in.

### REPL commands

| Command | Meaning |
|---|---|
| `/model [id]` | Switch model, or list models of the current provider with the active one marked. |
| `/provider [id]` | Switch provider, or list providers (unavailable ones are annotated with the missing variables). |
| `/thinking [level]` | Switch thinking level, or list levels for the current model. Omitted from `/help` when the model declares none. |
| `/clear` | Reset history to the configured `system_prompt` (or empty if none). |
| `/help` | Show help. |
| `/exit` | Quit (Ctrl-D also exits; at the prompt press Ctrl-C twice within two seconds — the first shows a gray hint). |

Switching only affects subsequent requests; earlier history stays as recorded. If a `/provider` switch leaves the current model outside the new provider, the model resets to the first model declared by that provider and thinking resets to that model's `thinking_default`.

## Configuration

The built-in config ships DeepSeek (`deepseek/deepseek-flash`, `deepseek/deepseek-v4-pro`). A user config must be self-contained:

```toml
default_provider = "deepseek"
default_model    = "deepseek/deepseek-v4-pro"   # wins over default_provider on conflict
system_prompt    = "You are rho, a lightweight local ops assistant."  # omit to send no system message
max_tool_steps   = 25

[providers.deepseek]
protocol = "openai_chat"                        # the protocol implemented today
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
- `${VAR}` in auth headers is expanded per provider. A missing variable marks that provider unavailable; other providers keep working, unless that provider was explicitly requested.
- Thinking levels are JSON object fragments merged into the top level of the request body. Level names are yours to choose.
- Validation is strict: unknown fields, dangling references and missing required fields abort startup with exit code 2 and the config path; TOML syntax errors also include a line number.

## The `bash` tool and its safety model

`rho` runs `bash -c <command>` in the foreground. Each call starts a fresh shell, so `cwd` and environment changes stay inside that call. Background processes started by a command keep running; chain `cd` and the command in one line if you need both.

A command runs without asking only when **both** hold:

1. It is exactly an `allow` entry, or starts with one immediately followed by a space; and
2. it contains none of these shell metacharacters: `;` `&` `|` `<` `>` `$`, a backtick, or a newline.

Everything else prompts with the working directory and the full command, defaulting to **N**; a rejection is fed back to the model as a tool result. `--yes` skips all prompts.

> **Safety boundary.** The allowlist matches strings and acts as a convenience gate; the host's user permissions remain the real security boundary. Child processes inherit the full environment, so the model can read `DEEPSEEK_API_KEY` via `env`. Run `rho` as a user you are willing to have execute commands, and use it only on hosts you trust it to read.

Other limits: at most `max_tool_steps` model-initiated tool calls per turn (default 25, counting rejected/failed calls too); stdout and stderr are each capped at `max_output_bytes` with the head and tail kept and the omitted byte count marked; a timeout kills the whole process group (SIGTERM, then SIGKILL after 2s). HTTP errors are reported directly. History is sent as-is; when a conversation exceeds the model window, the upstream request fails and you need `/clear`.

## Testing

```sh
cargo test --all-targets --locked
```

CI runs the tests on stable Rust, on every push to `master` or `dev` and every pull request. Tests run against local mocks: protocol tests use a hand-written local SSE mock over `TcpListener`, and the REPL/agent-loop tests drive the real loop with a scripted `Ui` implementation. The signal → record path of `term.rs` and live terminal behaviour are verified manually.

## License

[MIT](LICENSE)
