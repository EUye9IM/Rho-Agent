# rho

[![test](https://github.com/EUye9IM/Rho-Agent/actions/workflows/test.yml/badge.svg?branch=master)](https://github.com/EUye9IM/Rho-Agent/actions/workflows/test.yml)

**English** | [简体中文](README.zh-CN.md)

`rho` is a small local agent that lives in your terminal. It is a single binary (musl static builds are the intended distribution form), uses blocking I/O and threads, and is built for resource-constrained Linux boxes. You talk to it in a REPL; it streams answers from an OpenAI Chat Completions–compatible provider (DeepSeek by default), with OpenAI Responses support available by configuration, and can run shell commands and read, write, or edit files through built-in tools.

## Vision

rho should stay a small, predictable local agent for Linux boxes with limited resources: one binary, providers declared in TOML, four built-in tools for shell and file work, and a REPL whose history stays valid through cancellation.

Implementation: [docs/architecture.md](docs/architecture.md).

## Features

- **Small by design** — one crate, one binary, blocking I/O + threads; `strip` + LTO + `panic = "abort"` in release.
- **Config-driven providers** — providers, models and thinking levels are declared as data. `openai_chat` and `openai_responses` are both supported; switching protocol is configuration only.
- **Markdown in the terminal** — assistant text and reasoning both render inline bold, italic, strikethrough, inline code and links as ANSI, plus headings, lists, blockquotes, fenced code blocks and rules. Tables are buffered until the block ends and then rendered with aligned columns (CJK-aware widths, left/center/right from the delimiter row); images and HTML pass through as plain text. Output stays append-only: only the part of a line whose markers are closed is written, so nothing is ever redrawn. Command output and session notices are written verbatim.
- **Streaming with visible reasoning** — text streams as it arrives; the prompt and input are colored; tool calls have their own color, with successful results green, failures red, and interruptions yellow; `reasoning_content` is rendered dimmed and is sent back with history only when the provider sets `compat.echo_reasoning = true` (the built-in DeepSeek config enables it, because DeepSeek asks for reasoning content to be returned alongside tool-enabled requests).
- **Built-in tools** — `bash` runs in the foreground as `bash -c <command>` with a fresh shell, a per-call timeout, and process-group cleanup. `read` returns line-numbered text; `write` creates new files and parent directories; `edit` applies exact, sequential replacements and preserves the file's CRLF/LF style.
- **Predictable Ctrl-C** — first Ctrl-C cancels the current stream/tool and returns you to the prompt (cancellation takes effect between chunks; if the connection stalls, the background reader ends once the next data arrives or the idle timeout hits), second one exits; at the idle prompt two empty-line Ctrl-C within two seconds exit (the first shows a gray hint). The conversation history is always left in a protocol-valid state.
- **Tab completion** — slash command names and the arguments of `/model`, `/provider` and `/thinking` complete from the current session state; candidates refresh after a provider or model switch.
- **Manual context compaction** — `/compact` asks the model for a structured checkpoint of the older turns and replaces them with it, keeping the most recent turns verbatim; the instruction is configurable.
- **Per-turn status line** — each turn ends with one dim line showing the turn number, model, thinking level, prompt-cache hit rate and context usage.
- **One resumable session** — every turn is snapshotted to `$XDG_STATE_HOME/rho/session.json`; `rho -c` reloads it and replays the history (prompt/input lines, dimmed reasoning, colored tool results) before the first prompt.
- **Strict config validation** — unknown fields, dangling references and missing required fields fail loudly with the config path (TOML syntax errors also include a line number), exit code 2.

## Requirements

- Linux: `x86_64`, `aarch64` or `armv7` (musl static targets are the intended distribution form).
- A recent stable Rust toolchain to build (CI tests on stable).
- An interactive TTY — `rho` requires one and exits with code 2 when stdin is not a TTY. It runs as a REPL.

## Install

Install the latest stable musl release to `~/.local/bin` (`x86_64` and `aarch64`):

```sh
curl -fsSL https://raw.githubusercontent.com/EUye9IM/Rho-Agent/master/install.sh | sh
```

Install the nightly build (a rolling build from the `dev` branch):

```sh
curl -fsSL https://raw.githubusercontent.com/EUye9IM/Rho-Agent/master/install.sh | sh -s -- --nightly
```

The installer verifies the release checksum before installing. Add `~/.local/bin` to `PATH` if it is not already there. Download traffic uses `curl`, so standard proxy environment variables apply.

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
# suitable musl cross toolchain, e.g. aarch64-linux-musl-gcc or
# arm-linux-musleabihf-gcc from musl.cc:
rustup target add aarch64-unknown-linux-musl
cargo build --release --locked --target aarch64-unknown-linux-musl
rustup target add armv7-unknown-linux-musleabihf
cargo build --release --locked --target armv7-unknown-linux-musleabihf
```

## Usage

```sh
export DEEPSEEK_API_KEY=sk-...
rho
rho -c     # continue the last session
```

```
rho> check disk usage under /var/log
→ bash: du -sh /var/log/* | sort -h
...
```

Pick a non-default provider, model or thinking level:

```sh
rho --config ./rho.toml --model deepseek/deepseek-v4-pro --thinking low
rho --init           # write the default config to ~/.config/rho/config.toml
```

### CLI

| Flag | Meaning |
|---|---|
| `--config <path>` | Use an external config. It replaces the built-in config entirely. |
| `--provider <id>` | Initial provider (overrides config default). |
| `--model <id>` | Initial model (must be `provider/model`, overrides config default). |
| `--thinking <level>` | Initial thinking level (must exist for the chosen model). |
| `-c`, `--continue` | Load the saved session and replay it before the first prompt. Missing file or unreadable JSON starts a fresh session; a corrupt file is renamed to `session.json.bad` first. |
| `--init` | Write the built-in config to the `--config` path or the default path; refuses to overwrite an existing file. Mutually exclusive with `-c`. |
| `--version`, `-V` | Print the rho version and exit. |

Config source priority: `--config <path>` > the default path `$XDG_CONFIG_HOME/rho/config.toml` (or `$HOME/.config/rho/config.toml` when unset) > the built-in config. A missing default-path file falls back to the built-in config; an existing one that fails to parse or validate is an error (exit code 2). `rho --init` writes the full built-in config for you to edit incrementally. `${VAR}` in auth headers is expanded from the environment. `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` and `NO_PROXY` control proxy routing. The working directory is fixed to the directory `rho` was started in.

The session file lives at `$XDG_STATE_HOME/rho/session.json` (`$HOME/.local/state/rho/session.json` when unset). It holds the whole history, including reasoning and tool output, and is rewritten as a single snapshot after each turn; SQLite-style append-only logs are not used. One session exists at a time: starting rho without `-c` begins a fresh one and the first write replaces the old file, as does `/clear`.

### REPL commands

| Command | Meaning |
|---|---|
| `/model [id]` | Switch model, or list models of the current provider with the active one marked. |
| `/provider [id]` | Switch provider, or list providers (unavailable ones are annotated with the missing variables). |
| `/thinking [level]` | Switch thinking level, or list levels for the current model. Omitted from `/help` when the model declares none. |
| `/compact` | Replace the older turns with a checkpoint summary; the most recent `compact.keep_turns` turns stay verbatim. |
| `/clear` | Reset history to the configured `system_prompt` (or empty if none). |
| `/help` | Show help. |
| `/exit` | Quit (Ctrl-D also exits; at the prompt press Ctrl-C twice within two seconds — the first shows a gray hint). |

Switching only affects subsequent requests; earlier history stays as recorded. If a `/provider` switch leaves the current model outside the new provider, the model resets to the first model declared by that provider and thinking resets to that model's `thinking_default`.

A line is parsed as a command only when `/` sits in the very first column. Leave one space in front and the whole line is sent to the model as an ordinary message (leading and trailing whitespace is stripped before sending), so `/home/user/file` can be passed literally.

### Status line

Every turn ends with one dim line:

```
── #3 · deepseek/deepseek-flash · high · 53% · 12.4k/1M (1.2%)
```

| Field | Meaning |
|---|---|
| `#3` | The turn number: how many user inputs this session has handled. `/clear` resets the history, so the next turn is `#1` again. |
| model id | The active model (`/model` switch applies to the next turn). |
| thinking level | The active level; omitted when the model declares no levels. |
| `53%` | Prompt-cache hit rate: cached input tokens over input tokens. Omitted when the provider reports no cache statistics (`compat.stream_usage = false`, or a gateway that does not send `usage`). |
| `12.4k/1M (1.2%)` | Tokens used by the last completed request of the turn over the model's `context_window`. `上下文 —` when the turn produced no completed request. |

Failed and cancelled turns also end with a status line, using the last completed request of that turn (or `上下文 —` if the turn was cancelled before any request finished). Slash commands, `/exit`, Ctrl-D and the second Ctrl-C do not print one. Token counts use `k`/`M` units above a thousand.

## Configuration

The built-in config ships DeepSeek (`deepseek/deepseek-flash`, `deepseek/deepseek-v4-pro`). A user config must be self-contained:

```toml
default_provider = "deepseek"
default_model    = "deepseek/deepseek-v4-pro"   # wins over default_provider on conflict
system_prompt    = "You are rho, a lightweight local ops assistant."  # omit to send no system message
max_tool_steps   = 100
dns              = ["https://223.5.5.5/dns-query"]  # optional: preferred DoH resolver

[compact]
keep_turns = 2                                   # verbatim turns kept when /compact runs
# prompt = "..."                                 # checkpoint instruction; the built-in one is used when omitted

[providers.deepseek]
protocol = "openai_chat"                        # openai_chat or openai_responses
base_url = "https://api.deepseek.com"

[providers.deepseek.auth]
headers = { Authorization = "Bearer ${DEEPSEEK_API_KEY}" }

[providers.deepseek.compat]
# Non-standard behavior of this provider. Everything here defaults to the OpenAI
# standard; the built-in config turns on what DeepSeek needs.
legacy_max_tokens = true                         # send max_tokens instead of max_completion_tokens
echo_reasoning    = true                         # echo reasoning_content back with tool-enabled requests
stream_usage      = true                         # send stream_options.include_usage (default true)

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
```

Rules worth knowing:

- Model ids are global and must carry a provider prefix (`deepseek/deepseek-v4-pro`); the prefix must match a declared provider. The upstream `model` field defaults to the part after the first `/`.
- `protocol` accepts `openai_chat` (Chat Completions, `/chat/completions`) or `openai_responses` (Responses, `/responses`). The Responses adapter maps history to `input` items and uses `max_output_tokens`.
- `${VAR}` in auth headers is expanded per provider. A missing variable marks that provider unavailable; other providers keep working, unless that provider was explicitly requested.
- `compat` holds wire-format differences from the OpenAI standard, per provider: `legacy_max_tokens` sends the old `max_tokens` field instead of `max_completion_tokens`, `echo_reasoning` sends `reasoning_content` back with history (OpenAI reasoning models return no chain-of-thought and have no such field), `stream_usage` requests `stream_options.include_usage` so the status line has usage data. Omitting the section means OpenAI-standard behavior; the built-in DeepSeek config enables the first two.
- Thinking levels are JSON object fragments merged into the top level of the request body. Level names are yours to choose.
- `/compact` replaces every turn older than `compact.keep_turns` with one checkpoint message, produced by a request shaped like a normal turn: a copy of that history slice plus `compact.prompt` as the final user message. Tool results and assistant text are trimmed in that copy so the request fits the window; the history itself changes only when a summary comes back. A failed, empty or cancelled compaction leaves the history untouched.
- `dns` lists optional DoH endpoints used first, in order; the system resolver takes over only when every endpoint fails. The endpoint host must be an IP literal (`https://223.5.5.5/dns-query`; IPv6 uses `https://[2400:3200::1]/dns-query`). Answers are cached for their TTL. Android/Termux has no `/etc/resolv.conf`, so musl static builds rely on this entry to resolve names.
- Validation is strict: unknown fields, dangling references and missing required fields abort startup with exit code 2 and the config path; TOML syntax errors also include a line number.

## Tools and safety model

`rho` ships four tools. Their names, descriptions and JSON schemas are sent to the model; the model calls them with JSON arguments, and every call is paired with a tool result in the conversation history.

- **`bash`** runs `bash -c <command>` in the foreground. Each call starts a fresh shell, so `cwd` and environment changes stay inside that call. `timeout` is optional (seconds, default 120, maximum 300); `cwd` sets the initial directory (absolute or relative to rho's startup directory). stdout and stderr share one pipe, and only the last 50,000 Unicode characters are returned, with `[omitted_chars]` recording what was dropped. On timeout or Ctrl-C the whole process group is terminated (SIGTERM, then SIGKILL after 2s). Background processes started by a command keep running.
- **`read`** returns text with real 1-based line numbers. It reads at most 2,000 lines and 50,000 characters of file content per call; `offset` is 0-based and `limit` is capped at 2,000. The whole file must be valid UTF-8 without NUL bytes. A single line over 50,000 characters is an error; use `sed`, `cut` or `fold` from `bash` for that case.
- **`write`** creates a new file and any missing parent directories. It refuses to overwrite an existing file, directory or symlink, rejects NUL bytes, and writes the content as-is (no added newline, no line-ending conversion).
- **`edit`** applies exact string replacements from an `edits` array in order. By default `old_text` must occur exactly once; `replace_all: true` replaces every match. Files are capped at 10 MiB and must be valid UTF-8 without NUL. The tool normalizes CRLF to LF for matching and writes back the detected dominant line-ending style. Each successful edit is written immediately; if a later edit fails, earlier edits stay on disk. A failed match reports the closest line and snippet when it is similar enough.

Every valid tool call the model emits is executed. The host's user permissions are the only security boundary: `bash` inherits the full environment, and the file tools can read or modify anything that user can. Run `rho` as a user you are willing to have execute commands, and use it only on hosts you trust it to read.

Other limits: at most `max_tool_steps` model-initiated tool calls per turn (default 100, counting invalid calls too). Each request sends the model's `max_output` as the upstream output token limit. SSE lines are capped at 1 MiB and a single response body at 32 MiB; exceeding either ends the turn with an error. HTTP errors are reported directly. History is sent as-is; when a conversation exceeds the model window, the upstream request fails and you need `/clear`.

## Testing

```sh
cargo fmt --all --check
cargo test --all-targets --locked
```

CI runs formatting, clippy and tests on stable Rust, on every push to `master` or `dev` and every pull request. Tests run against local mocks: protocol tests use a hand-written local SSE mock over `TcpListener`, and the REPL/agent-loop tests drive the real loop with a scripted `Ui` implementation. The signal → record path of `term.rs` and live terminal behaviour are covered by PTY integration tests that drive the real binary through a pty and assert on the parsed screen grid.

## License

[MIT](LICENSE)
