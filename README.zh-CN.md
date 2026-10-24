# rho

[![test](https://github.com/EUye9IM/Rho-Agent/actions/workflows/test.yml/badge.svg?branch=master)](https://github.com/EUye9IM/Rho-Agent/actions/workflows/test.yml)

[English](README.md) | **简体中文**

`rho` 是一个跑在终端里的轻量本地 agent。单 crate、单二进制（分发建议使用 musl 静态构建），使用阻塞 I/O + 线程，面向资源受限的 Linux 设备。你在 REPL 里对话，它从兼容 OpenAI Chat Completions 的供应商（默认 DeepSeek）流式返回结果，并能通过 `bash` 工具执行命令——执行前要过 allowlist 判定和人工确认。

## 愿景

rho 要做一个运行在资源受限 Linux 设备上的本地 agent：单二进制、供应商由配置声明、只带一个 `bash` 工具；REPL 交互简单可预测，取消后对话历史仍然完整。

实现架构见 [docs/architecture.md](docs/architecture.md)。

## 特性

- **轻** —— 单 crate 单二进制，使用阻塞 I/O + 线程，release 开启 `strip` + LTO + `panic = "abort"`。
- **配置驱动** —— 供应商、模型、思考档位都由数据声明；同协议族接入新供应商只加配置。
- **流式 + 思考可见** —— 正文逐块输出；`reasoning_content` 以暗淡色实时展示，默认只留在本地，仅当供应商配置 `compat.echo_reasoning = true` 时随历史回传。
- **只有一个工具：`bash`** —— 前台执行 `bash -c <命令>`，每次调用启动新的 shell，单次超时后按进程组逐级终止。
- **可预测的 Ctrl-C** —— 第一次取消当前流式/工具/确认，第二次退出；对话历史在任何时刻都保持协议合法（工具调用必有对应结果）。
- **严格配置校验** —— 未知字段、悬空引用、缺失必填字段一律硬报错并给出配置路径（TOML 语法错误另带行号），退出码 2。

## 环境要求

- Linux：`x86_64`、`aarch64` 或 `armv7`（建议分发 musl 静态构建）。
- 构建需要 Rust 1.75+（MSRV）。CI 会同时测试 1.75 与 stable。
- 需要交互式 TTY：stdin 非 TTY 时以退出码 2 结束。rho 以 REPL 方式运行。

## 构建

```sh
cargo build --release --locked
./target/release/rho
```

静态 musl 构建：

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --locked --target x86_64-unknown-linux-musl

# aarch64 / armv7 需先安装 target 和对应的 cross linker/toolchain
# （如 aarch64-linux-gnu-gcc、arm-linux-gnueabihf-gcc），然后执行同样的命令：
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

指定供应商 / 模型 / 思考档位，或跳过确认：

```sh
rho --config ./rho.toml --model deepseek/deepseek-v4-pro --thinking low
rho --yes            # 自动放行所有 bash 调用
```

### 命令行参数

| 参数 | 含义 |
|---|---|
| `--config <path>` | 使用外部配置；它完全替换内置配置。 |
| `--provider <id>` | 初始供应商（覆盖配置默认）。 |
| `--model <id>` | 初始模型（须为 `provider/model` 形式，覆盖配置默认）。 |
| `--thinking <level>` | 初始思考档位（须属于所选模型）。 |
| `--yes` | 跳过所有 `bash` 确认。 |

用户配置只认 `--config`，未提供时使用内置配置。环境变量只用于 `${VAR}` header 展开。会话工作目录恒为进程启动目录，全程不可修改。

### REPL 命令

| 命令 | 含义 |
|---|---|
| `/model [id]` | 切换模型；不带参数时列出当前供应商的模型并标注当前值。 |
| `/provider [id]` | 切换供应商；不带参数时列出全部供应商（不可用者标注缺失变量）。 |
| `/thinking [level]` | 切换思考档位；模型未声明档位时从 `/help` 中省略该命令。 |
| `/clear` | 重置为配置的 `system_prompt`（未配置则为空）。 |
| `/help` | 显示帮助。 |
| `/exit` | 退出（Ctrl-D、空行 Ctrl-C 同样正常退出）。 |

切换只影响后续请求，已有历史保持不变。若 `/provider` 切换后当前模型不属于新供应商，模型重置为该供应商声明顺序第一个，thinking 重置为新模型的 `thinking_default`。

## 配置

内置配置包含 DeepSeek（`deepseek/deepseek-flash`、`deepseek/deepseek-v4-pro`）。用户配置必须自包含：

```toml
default_provider = "deepseek"
default_model    = "deepseek/deepseek-v4-pro"   # 与 default_provider 冲突时以模型为准
system_prompt    = "你是 rho，一个轻量本地运维助手。"  # 省略则请求不含 system 消息
max_tool_steps   = 25

[providers.deepseek]
protocol = "openai_chat"                        # 当前实现的协议族
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

[tools.bash]
allow            = ["ls", "cat /var/log", "systemctl status"]  # 留空表示全部需确认
timeout_seconds  = 120
max_output_bytes = 16384
```

要点：

- 模型 id 全局唯一且必须带供应商前缀（`deepseek/deepseek-v4-pro`），前缀须对应已声明的供应商；上游 `model` 字段默认取第一个 `/` 之后的部分。
- 认证 header 中的 `${VAR}` 在选择供应商时逐个展开：变量缺失只让该供应商不可用，不影响其他供应商——除非该供应商是被显式指定的。
- 思考档位的值是 JSON 对象片段，请求时顶层合并进 body；档位名由配置自定义。
- 校验严格：未知字段、悬空引用、缺失必填字段直接拒绝启动（退出码 2，并给出配置路径；TOML 语法错误另带行号）。

## `bash` 工具与安全模型

`rho` 前台执行 `bash -c <命令>`。每次调用启动新的 shell，cwd 和环境变量改动只留在当次调用内；命令启动的后台进程会继续运行。需要先 `cd` 再执行时，写在同一行命令里。

命令**同时**满足以下两点才免确认执行：

1. 与某个 `allow` 条目完全相同，或以该条目开头且其后紧跟一个空格；
2. 命令中不含 shell 元字符 `;` `&` `|` `<` `>` `$`、反引号或换行。

其余情况一律弹确认，显示当前工作目录与完整命令，默认 **N**；拒绝结果会作为工具结果喂回模型。`--yes` 跳过全部确认。

> **安全边界。** allowlist 做字符串匹配，是免确认的便捷门槛；真正的安全边界是运行 `rho` 的用户权限。子进程完全继承父进程环境变量，模型用一条 `env` 就能读出 `DEEPSEEK_API_KEY`。请以你愿意让其执行命令的用户身份运行 `rho`，并只在信任的主机上使用。

其他限制：每轮对话最多 `max_tool_steps` 次模型发起的工具调用（默认 25，拒绝/失败/参数错误的调用同样计数）；stdout 与 stderr 各自上限 `max_output_bytes`，保留头尾两段并标注省略字节数；超时后对整个进程组 SIGTERM，2 秒后 SIGKILL。HTTP 错误直接展示；历史按原样发送，会话超出模型窗口时上游会失败，此时用 `/clear` 重开。

## 测试

```sh
cargo test --all-targets --locked
```

CI 在每次 push 到 `master` 或 `dev`、以及每个 PR 上，用 Rust 1.75（MSRV）与 stable 跑测试。测试使用本地 mock：协议层用 `TcpListener` 手写本地 SSE mock，REPL/agent loop 用脚本化的 `Ui` 实现驱动真实主循环。`term.rs` 里从信号到记录这一段，以及真实终端交互，靠手工验收。

## 许可

[MIT](LICENSE)
