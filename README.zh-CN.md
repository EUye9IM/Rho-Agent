# rho

[![test](https://github.com/EUye9IM/Rho-Agent/actions/workflows/test.yml/badge.svg?branch=master)](https://github.com/EUye9IM/Rho-Agent/actions/workflows/test.yml)

[English](README.md) | **简体中文**

`rho` 是一个跑在终端里的轻量本地 agent。单 crate、单二进制（分发建议使用 musl 静态构建），使用阻塞 I/O + 线程，面向资源受限的 Linux 设备。你在 REPL 里对话，它从兼容 OpenAI Chat Completions 的供应商（默认 DeepSeek）流式返回结果，也可通过配置使用 OpenAI Responses 协议，并能通过内置工具执行命令、读写文件。

## 愿景

rho 要做一个运行在资源受限 Linux 设备上的本地 agent：单二进制、供应商由配置声明、内置 shell 与文件工具；REPL 交互简单可预测，取消后对话历史仍然完整。

实现架构见 [docs/architecture.md](docs/architecture.md)。

## 特性

- **轻** —— 单 crate 单二进制，使用阻塞 I/O + 线程，release 开启 `strip` + LTO + `panic = "abort"`。
- **配置驱动** —— 供应商、模型、思考档位都由数据声明；支持 `openai_chat` 与 `openai_responses`，切换协议只改配置。
- **流式 + 思考可见** —— 正文逐块输出；`reasoning_content` 以暗淡色实时展示，默认只留在本地，仅当供应商配置 `compat.echo_reasoning = true` 时随历史回传。
- **内置工具** —— `bash` 前台执行 `bash -c <命令>`，每次调用启动新的 shell，支持单次超时并按进程组逐级终止；`read` 返回带行号的文本；`write` 新建文件和父目录；`edit` 按顺序做精确替换，并保留文件的 CRLF/LF 风格。
- **可预测的 Ctrl-C** —— 第一次取消当前流式/工具，立即回到提示符（取消标志在逐块之间轮询生效；连接停滞无数据时，后台读取在下一块数据到达或空闲超时后结束），第二次退出；空闲提示符下两次空行 Ctrl-C（2 秒内）同样退出，第一次会在输入行尾显示灰色提示。对话历史在任何时刻都保持协议合法（工具调用必有对应结果）。
- **Tab 补全** —— 补全斜杠命令名以及 `/model`、`/provider`、`/thinking` 的参数，候选来自当前会话状态；切换供应商或模型后自动刷新。
- **严格配置校验** —— 未知字段、悬空引用、缺失必填字段一律硬报错并给出配置路径（TOML 语法错误另带行号），退出码 2。

## 环境要求

- Linux：`x86_64`、`aarch64` 或 `armv7`（建议分发 musl 静态构建）。
- 构建需要较新的 stable Rust 工具链（CI 在 stable 上测试）。
- 需要交互式 TTY：stdin 非 TTY 时以退出码 2 结束。rho 以 REPL 方式运行。

## 安装

安装最新 musl 产物到 `~/.local/bin`（支持 `x86_64` 与 `aarch64`）：

```sh
curl -fsSL https://raw.githubusercontent.com/EUye9IM/Rho-Agent/master/install.sh | sh
```

安装脚本会先校验 release 的 SHA-256，再安装二进制。若 `~/.local/bin` 不在 `PATH` 中，请自行加入。下载使用 `curl`，遵循标准代理环境变量。

## 构建

```sh
cargo build --release --locked
./target/release/rho
```

静态 musl 构建：

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --locked --target x86_64-unknown-linux-musl

# aarch64 / armv7 需先安装 target 和对应的 musl 交叉工具链
# （如 musl.cc 的 aarch64-linux-musl-gcc、arm-linux-musleabihf-gcc），再执行同样的命令：
rustup target add aarch64-unknown-linux-musl
cargo build --release --locked --target aarch64-unknown-linux-musl
rustup target add armv7-unknown-linux-musleabihf
cargo build --release --locked --target armv7-unknown-linux-musleabihf
```

## 使用

```sh
export DEEPSEEK_API_KEY=sk-...
rho
```

```
rho> 看看 /var/log 下的磁盘占用
→ bash: du -sh /var/log/* | sort -h
...
```

指定供应商 / 模型 / 思考档位：

```sh
rho --config ./rho.toml --model deepseek/deepseek-v4-pro --thinking low
rho --init           # 生成默认配置文件到 ~/.config/rho/config.toml
```

### 命令行参数

| 参数 | 含义 |
|---|---|
| `--config <path>` | 使用外部配置；它完全替换内置配置。 |
| `--provider <id>` | 初始供应商（覆盖配置默认）。 |
| `--model <id>` | 初始模型（须为 `provider/model` 形式，覆盖配置默认）。 |
| `--thinking <level>` | 初始思考档位（须属于所选模型）。 |
| `--init` | 把内置配置写到 `--config` 路径或默认路径；目标已存在时报错不覆盖。 |
| `--version`、`-V` | 打印 rho 版本并退出。 |

配置来源优先级：`--config <path>` > 默认路径 `$XDG_CONFIG_HOME/rho/config.toml`（未设置时 `$HOME/.config/rho/config.toml`）> 内置配置。默认路径文件不存在时回退内置配置，存在但解析/校验失败则报错（退出码 2）。`rho --init` 一次性写出完整的内置配置，供你在此基础上增量修改。认证 header 中的 `${VAR}` 从环境变量展开；`HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY` 与 `NO_PROXY` 控制代理路由。会话工作目录恒为进程启动目录，全程不可修改。

### REPL 命令

| 命令 | 含义 |
|---|---|
| `/model [id]` | 切换模型；不带参数时列出当前供应商的模型并标注当前值。 |
| `/provider [id]` | 切换供应商；不带参数时列出全部供应商（不可用者标注缺失变量）。 |
| `/thinking [level]` | 切换思考档位；模型未声明档位时从 `/help` 中省略该命令。 |
| `/clear` | 重置为配置的 `system_prompt`（未配置则为空）。 |
| `/help` | 显示帮助。 |
| `/exit` | 退出（Ctrl-D 同样正常退出；提示符下 2 秒内连按两次空行 Ctrl-C 也退出，第一次会显示灰色提示）。 |

切换只影响后续请求，已有历史保持不变。若 `/provider` 切换后当前模型不属于新供应商，模型重置为该供应商声明顺序第一个，thinking 重置为新模型的 `thinking_default`。

## 配置

内置配置包含 DeepSeek（`deepseek/deepseek-flash`、`deepseek/deepseek-v4-pro`）。用户配置必须自包含：

```toml
default_provider = "deepseek"
default_model    = "deepseek/deepseek-v4-pro"   # 与 default_provider 冲突时以模型为准
system_prompt    = "你是 rho，一个轻量本地运维助手。"  # 省略则请求不含 system 消息
max_tool_steps   = 25
dns              = ["https://223.5.5.5/dns-query"]  # 可选：优先使用的 DoH 解析器

[providers.deepseek]
protocol = "openai_chat"                        # openai_chat 或 openai_responses
base_url = "https://api.deepseek.com"

[providers.deepseek.auth]
headers = { Authorization = "Bearer ${DEEPSEEK_API_KEY}" }

[providers.deepseek.compat]
echo_reasoning = false

[models."deepseek/deepseek-v4-pro"]
context_window   = 1000000
max_output       = 65536
thinking_default = "high"
# api_model = "..."  # 可选：覆盖上游 model 字段

[models."deepseek/deepseek-v4-pro".thinking]
off  = { thinking = { type = "disabled" } }
low  = { thinking = { type = "enabled" }, reasoning_effort = "low" }
high = { thinking = { type = "enabled" }, reasoning_effort = "high" }
max  = { thinking = { type = "enabled" }, reasoning_effort = "max" }
```

要点：

- 模型 id 全局唯一且必须带供应商前缀（`deepseek/deepseek-v4-pro`），前缀须对应已声明的供应商；上游 `model` 字段默认取第一个 `/` 之后的部分，`max_output` 作为输出 token 上限一并发给上游。
- `protocol` 支持 `openai_chat`（Chat Completions，`/chat/completions`）与 `openai_responses`（Responses，`/responses`）；后者把历史映射为 `input` items，并用 `max_output_tokens` 限制输出。
- 认证 header 中的 `${VAR}` 在选择供应商时逐个展开：变量缺失只让该供应商不可用，不影响其他供应商——除非该供应商是被显式指定的。
- 思考档位的值是 JSON 对象片段，请求时顶层合并进 body；档位名由配置自定义。
- `dns` 是可选的 DoH 端点列表，配置后按声明顺序优先解析，端点全部失败才回退系统解析器；端点主机必须是 IP 字面量（形如 `https://223.5.5.5/dns-query`，IPv6 写成 `https://[2400:3200::1]/dns-query`）。成功结果按应答 TTL 缓存。Android/Termux 没有 `/etc/resolv.conf`，musl 静态构建的域名解析依赖这一项。
- 校验严格：未知字段、悬空引用、缺失必填字段直接拒绝启动（退出码 2，并给出配置路径；TOML 语法错误另带行号）。

## 工具与安全模型

`rho` 内置四个工具。工具名、描述和 JSON Schema 会发给模型；模型用 JSON 参数调用，每个调用都会在对话历史中配一条工具结果。

- **`bash`** 前台执行 `bash -c <命令>`。每次调用启动新的 shell，cwd 和环境变量改动只留在当次调用内。`timeout` 可选（秒，默认 120，最大 300）；`cwd` 指定初始目录（绝对路径或相对 rho 启动目录）。stdout 与 stderr 共用同一个 pipe，只返回末尾 50,000 个 Unicode 字符，丢掉的字符数写在 `[omitted_chars]`。超时或 Ctrl-C 会终止整个进程组（SIGTERM，2 秒后 SIGKILL）。命令启动的后台进程会继续运行。
- **`read`** 返回带真实 1-based 行号的文本。每次最多读取 2,000 行、50,000 个正文字符；`offset` 是 0-based，`limit` 最大 2,000。整个文件必须是有效 UTF-8 且不含 NUL。单行超过 50,000 字符会报错，建议改用 `bash` 的 `sed`、`cut` 或 `fold`。
- **`write`** 新建文件，并自动创建缺失的父目录。路径已存在（普通文件、目录或符号链接）时拒绝；内容不能含 NUL；按原样写入，不追加换行，也不转换行尾。
- **`edit`** 按 `edits` 数组顺序做精确字符串替换。默认 `old_text` 必须恰好出现一次；`replace_all: true` 时替换全部匹配。文件上限 10 MiB，必须是有效 UTF-8 且不含 NUL。匹配前把 CRLF 归一化为 LF，写回时按检测到的主要行尾风格恢复。每个 edit 成功后立即写盘；后续 edit 失败时，之前的修改保留。匹配失败时，如果存在足够相似的行，会返回行号和片段。

模型发出的每个合法工具调用都会执行。真正的安全边界是运行 `rho` 的用户权限：`bash` 继承完整环境变量，文件工具也能读写该用户可访问的任何路径。请以你愿意让其执行命令的用户身份运行 `rho`，并只在信任的主机上使用。

其他限制：每轮对话最多 `max_tool_steps` 次模型发起的工具调用（默认 25，非法调用同样计数）。每次请求把模型的 `max_output` 作为上游输出 token 上限发送。SSE 单行上限 1 MiB，单轮响应体累计上限 32 MiB，超出即按错误结束本轮。HTTP 错误直接展示；历史按原样发送，会话超出模型窗口时上游会失败，此时用 `/clear` 重开。

## 测试

```sh
cargo fmt --all --check
cargo test --all-targets --locked
```

CI 在每次 push 到 `master` 或 `dev`、以及每个 PR 上，用 stable Rust 跑格式检查、clippy 和测试。测试使用本地 mock：协议层用 `TcpListener` 手写本地 SSE mock，REPL/agent loop 用脚本化的 `Ui` 实现驱动真实主循环。`term.rs` 里从信号到记录这一段，以及真实终端交互，由 PTY 集成测试覆盖（openpty 驱动真实二进制，屏幕断言走 vt100 网格）。

## 许可

[MIT](LICENSE)
