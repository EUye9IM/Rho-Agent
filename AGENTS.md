# AGENTS.md

本文件写给在此仓库工作的编码 agent。开始改动前先读：

- `docs/architecture.md`：当前实现、模块边界、线程模型、测试分层
- `README.md` / `README.zh-CN.md`：用户可见行为与项目愿景

若代码与文档冲突，先指出冲突并确认，不要默默按其中一方改。

## 项目定位

`rho` 是运行在资源受限 Linux 设备上的本地 agent REPL：

- Rust 2021
- 单 crate、单二进制
- 阻塞 I/O + 线程，**不使用 async runtime**
- 默认 DeepSeek，供应商/模型/thinking 由 TOML 配置驱动
- 只有一个 `bash` 工具，带 allowlist、确认 gate、超时和进程组清理
- 分发目标是 Linux musl 静态二进制（x86_64 为主）

## 分支与发布

- 日常开发在 `dev` 分支，不要直接提交/推送到 `master`。功能分支从 `dev` 开，PR 目标也是 `dev`。
- `master` 作为稳定分支；发布时由用户确认后再合并 `dev` → `master`。
- 版本发布：更新 `Cargo.toml` / `Cargo.lock` 的版本，打 annotated tag `vX.Y.Z`，推送 tag。CI 会构建并创建 GitHub Release。
- 未经明确要求，不要：
  - `scp` 或安装新二进制到目标机
  - 替换目标机上的 `/usr/local/bin/rho`
  - 安装/切换 Rust toolchain、系统包、Docker 容器
  - 删除或重建 release/tag

## 常用命令

```sh
cargo build --all-targets --locked
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
```

不要随手执行 `cargo fmt --all`：当前 rustfmt 版本会重排大量未改动文件，制造无关 diff。只格式化本次改动的文件，或手工保持与周围代码一致。

## musl release 构建

```sh
CC_x86_64_unknown_linux_musl=musl-gcc \
CFLAGS_x86_64_unknown_linux_musl="-march=x86-64 -mtune=generic" \
CXXFLAGS_x86_64_unknown_linux_musl="-march=x86-64 -mtune=generic" \
RUSTFLAGS="-C target-cpu=x86-64" \
cargo build --release --locked --target x86_64-unknown-linux-musl
```

这些 flag 不是可选项：

- 目标机 Intel Celeron J1800 没有 AVX/AVX2。
- 曾经因为没固定基线，CI 的 Ubuntu GCC 把 C 依赖编出无条件 AVX，程序在 HTTPS 路径 SIGILL。
- `-march=x86-64` 会一次关掉 AVX/AVX2/AVX-512/FMA/BMI/AES/SHA 等可选扩展。
- 不要只加 `-mno-avx`；Rust 侧同时要 `-C target-cpu=x86-64`。

发布产物应为 static-pie。发布前至少验证：

```sh
file target/x86_64-unknown-linux-musl/release/rho
qemu-x86_64 -cpu qemu64 ./rho --config <https-config>
```

## 架构约束

模块方向大致为：

```text
main → config
     → repl → chat → provider → sse
                   → tools
                   → ui / render / term
```

关键规则：

- `render.rs` 只提供纯格式化函数，不直接写终端。
- `ui` 模块（`ui/mod.rs`）负责终端输出；生产实现 `TermUi`，测试用 `ScriptedUi`（`ui/scripted.rs`）。
- 不要在 `render` / `chat` 里直接 `println!`。
- `provider` 每轮请求在工作线程读取 HTTP 流，经 `mpsc` 回主线程；不要引入 tokio/reqwest。
- `chat::Msg` 是协议无关的内部消息模型；OpenAI 字段映射留在 `provider/openai.rs`。
- 工具循环必须保证历史协议合法：所有 `tool_calls` 都要有对应 `Tool` 消息占位。
- 取消语义、进程组清理、确认 gate 的行为以 `docs/architecture.md` 和现有代码为准，不要简化成“杀直接子进程”或“只 drop 连接”。

## 配置语义

- `--config <path>` 是唯一外部配置入口；不读环境变量配置文件，不做默认路径发现。
- 外部配置**完全替换**内置配置，不合并。
- 严格校验：未知字段、悬空引用、缺失必填项都报错；语义错误一次收集全部，TOML 语法/反序列化错误按首个报告。
- `${VAR}` 只用于 header 值展开；缺失变量报错；明文密钥要告警。
- 退出码：`0` 正常退出，`1` REPL 内部错误，`2` 参数/配置/供应商不可用等用法错误。
- 不做隐式供应商分支；同协议族新供应商只加配置。

## 测试要求

改动行为时补对应层测试：

- 纯单元：`sse.rs`、`config`、`tools` 判定/截断、`render`
- 协议映射：`provider::openai`
- agent loop + 工具：`ScriptedUi` + fake adapter
- HTTP/SSE：`tests/e2e_mock.rs` 的本地 mock 服务
- REPL：`tests/repl_flow.rs`

约束：

- CI 不调用真实 API；不要提交真实 key。
- 默认测试套件不依赖 TTY；终端行为由 `tests/pty.rs` 用 openpty 起真实二进制驱动
  （屏幕断言走 vt100 网格）。真实终端的手工抽查保留用于布局细节与体感回归。
- 当前测试是可并行的本地 mock，不要引入固定端口或共享临时文件冲突。
- 提交前跑完整 `cargo test --all-targets --locked` 和 clippy。

## 终端/REPL 注意点

- `rustyline` 每次 `readline` 前会清当前行（`\r\x1b[K`）。如果上一段输出没有换行，最后一行会被擦掉。
- 正常回合结束后由 `repl.rs` 在 `TurnEnd::Done` 后补 `RenderEvent::LineBreak`；不要删掉，除非你同时换成等价的“确保行首”机制。
- `TermUi::confirm` 用 `libc::read(0)`，以便 Ctrl-C 的 SIGINT 能打断；不要改回 `std::io::stdin().read_line()`。
- 流式输出中 `TextDelta` 原样写出；不要在每个 delta 上强制加换行。

## 工具与安全

- `bash` allowlist 是字符串匹配，不是 shell 解析或安全沙箱；不要把它描述成沙箱。
- 确认 gate 默认拒绝；`--yes` 跳过全部确认（对任意命令直接放行），allowlist 只是免确认的便捷门槛。若打算改成“`--yes` 仍需过 allowlist”，必须先确认，因为这会改变行为。
- 工具输出 stdout/stderr 各自有上限，只截断一次并保留省略字节数。
- 超时和取消都走进程组 SIGTERM → 2s → SIGKILL；不要只杀直接子进程。
- 不要新增第二个工具，除非用户明确要求。

## 文档规则

- `docs/` 只放当前实现文档（目前是 `docs/architecture.md`）；不要新增设计文档、功能清单、流程/工作流文档，除非用户明确要求。
- 文档只描述当前代码的实际行为。用正向陈述；不要用“不支持 / 不使用 / 没有 / 不做”来定义功能。必要的错误条件（非 TTY、超限、非法输入等）可以直接写。
- 不写版本历史、变更记录、里程碑、状态横幅或“已完成 / 待发布”之类的过程信息。
- 产品愿景和定位写在 `README.md` / `README.zh-CN.md`；两份 README 保持同步，并只链接 `docs/architecture.md`。
- 行为或配置语义变化时，同步更新 README 和 architecture；不要留下指向已删除文档的引用。

## 提交

- 提交信息用英文、祈使句，保持小而聚焦。
- 行为改动同步更新 `README.md`、`README.zh-CN.md` 或 `docs/architecture.md` 中受影响的部分。
- 注释和文档用中文；用户可见文案目前也是中文。
- 提交前检查 `git status`，不要提交 `target/`、临时脚本、测试日志或密钥。
- 不要 force-push `master`。
