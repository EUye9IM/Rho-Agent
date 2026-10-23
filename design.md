# Rho-agent 轻量 agent 设计（定稿）

## 一、定位

一个运行在资源受限设备上的本地 Agent，以 REPL 交互为主，作为运维工具使用。核心诉求是**轻**：单二进制、功能克制、不引入重型运行时。供应商支持以配置驱动，优先服务 DeepSeek，同时保留接入其他供应商的能力。

## 二、设计原则

- **轻量优先**：功能少而明确，不为丰富度牺牲体积和内存。
- **配置驱动**：供应商差异通过声明式配置表达，Agent 本体不硬编码供应商分支。
- **简单可预测**：配置语义清晰，不做隐式合并或复杂继承；写错就报错。
- **按需扩展**：同协议族的新供应商只加配置；只有协议族本身变化时才动代码。

## 三、运行时与构建

- **语言**：Rust 2021，MSRV 1.75。
- **并发模型**：阻塞 I/O + 线程，**不使用 async 运行时**。HTTP 流在工作线程读取，经 `mpsc` 交给主线程渲染。
- **目标平台**：`x86_64` / `aarch64` 的 `-unknown-linux-musl` 与 `armv7-unknown-linux-musleabihf`，静态链接。不支持 Windows / macOS。
- **体积预算**：strip + LTO + `panic=abort` 后 ≤ 10MB；空闲常驻 RSS ≤ 30MB。
- **依赖**：`serde` / `serde_json` / `toml`（需 `preserve_order`）、`ureq` + `rustls`、`rustyline`、信号处理库。不引 tokio、reqwest、clap。`Cargo.lock` 入库，支持离线构建。
- **交互模型**：轮次制。流式输出期间不接受新输入。

### 取消语义

- 流式请求期间第一次 Ctrl-C（以轮次号 + 序号记录，非置位/清零标志，避免复位竞态）触发取消：渲染循环逐事件检查并立即停止显示；丢弃当前未完成的 assistant 响应及其中不完整的工具调用，不执行不完整调用。已完成的 assistant/tool 消息保留在对话历史中。
- 在途流线程被放弃（detach），不主动关闭 HTTP 连接；线程自然结束或在 socket 空闲读超时后退出。空闲超时不限制持续有数据时的总存活时间。
- 工具执行期间第一次 Ctrl-C 向整个子进程组发 SIGTERM；2s 后无论直接子进程状态如何，对整个进程组发 SIGKILL 并回收子进程（不以 bash 单体退出作为终止完成条件）。工具超时也采用同一升级顺序。
- 工具输出采集：stdout/stderr 各一个 drain 线程并行读取，有界缓冲，超限后丢弃并计数但不停止读取；bash 退出后进行有界的收尾读取（短超时上限，防止后台进程持续写入），随后 drop 读端、detach drain 线程，不等待后台进程关闭继承的管道写端。
- 当前操作取消后的第二次 Ctrl-C 请求退出程序；先完成工具进程清理，再退出。
- REPL 空闲且输入行为空时，Ctrl-C 退出；正在编辑输入时，Ctrl-C 清空当前输入行。

## 四、CLI 与交互

二进制名 `rho`，**只有 REPL 模式**。

### 参数

| 参数 | 含义 |
|---|---|
| `--config <path>` | 使用外部配置；**一旦提供，内置配置完全忽略，不做合并** |
| `--provider <id>` | 初始供应商（覆盖配置默认） |
| `--model <id>` | 初始模型（覆盖配置默认） |
| `--thinking <level>` | 初始思考档位（覆盖配置默认） |
| `--yes` | 跳过所有确认 |

- 配置只认 `--config`，不读环境变量、不做默认路径发现、不做合并。
- 无 `--cwd`：会话 cwd 恒为进程启动目录，全程不可修改（`working_directory` 工具已移除）。
- `stdin` 非 TTY 时拒绝启动，退出码 2。不提供 stdin 管道输入。
- 不提供一次性模式、`config validate` 子命令。

### REPL 命令

`/model [id]`、`/provider [id]`、`/thinking [level]`、`/clear`、`/help`、`/exit`。不带参数时列出可选项并标注当前值。切换只影响后续请求，不重放历史。三者联动规则：
- `/provider` 切换后，若当前模型不属于新 provider，模型重置为新 provider 声明顺序第一个，thinking 重置为该模型 `thinking_default`（无档位表则归零）；`/model` 切换后 thinking 同样重置。
- `/model` 切到不属于当前 provider 的模型：拒绝命令（提示先切 provider），状态不变。`/thinking` 切到当前模型不存在的档位：拒绝命令，状态不变。
- `/provider` 切到 key 变量不齐全的 provider：拒绝命令并列出缺失变量，状态不变（不退出进程）。
- 启动时：`--provider` 与 `--model` 同时指定且模型不属于该 provider，为用法错误（退出码 2）。只传 `--model`：直接采用（须带 provider 前缀且属于已声明 provider，否则用法错误），覆盖 `default_provider` 为其前缀对应 provider。只传 `--provider`：模型取该 provider 声明顺序第一个，thinking 取其默认档。
- 隐式选择时跳过缺 key 的 provider：若配置的 `default_model` 前缀指向不可用 provider，则按上文“默认值冲突消解”规则处理——`default_model` 效力高于隐式选择，指向的 provider key 不齐全时报错（不静默改用其他模型）。

### 退出码

`0` 正常退出；`1` 运行时或网络致命错误；`2` 配置或用法错误。

## 五、配置模型

TOML。内置一份精简默认配置编译进二进制；用户配置通过 `--config` 提供，且必须自包含。

### 结构

```toml
default_provider = "deepseek"           # 可选；缺省取声明顺序第一个
default_model    = "deepseek/deepseek-v4-pro"  # 可选
system_prompt    = """..."""            # 可选；整段替换内置英文提示词
max_tool_steps   = 25                   # 可选

[providers.deepseek]
protocol = "openai_chat"                # v1 只实现该协议族
base_url = "https://api.deepseek.com"

[providers.deepseek.auth]
headers = { Authorization = "Bearer ${DEEPSEEK_API_KEY}" }

[providers.deepseek.compat]
echo_reasoning = false

[models."deepseek/deepseek-flash"]
context_window   = 1000000
max_output       = 65536
thinking_default = "high"

[models."deepseek/deepseek-flash".thinking]
off  = { thinking = { type = "disabled" } }
low  = { thinking = { type = "enabled" }, reasoning_effort = "low" }
high = { thinking = { type = "enabled" }, reasoning_effort = "high" }
max  = { thinking = { type = "enabled" }, reasoning_effort = "max" }

[models."deepseek/deepseek-v4-pro"]
context_window   = 1000000
max_output       = 65536
thinking_default = "high"
# thinking 档位同上

[tools.bash]
allow             = ["ls", "cat /var/log", "systemctl status"]
timeout_seconds   = 120
max_output_bytes  = 16384
```

### 规则

- **模型 id 全局唯一且必须带 provider 前缀**（`deepseek/deepseek-v4-pro`）。前缀段必须对应已声明的 provider，否则配置错误。不再有独立的 `provider` 字段。
- **上游 `model` 字段**：默认取 id 第一个 `/` 之后的部分；需要别名时用可选字段 `api_model` 覆盖。
- **思考档位**：值是任意 JSON 片段，请求时**顶层合并**进 body。档位名由配置自定义，不做统一枚举。没有 `thinking` 表的模型不显示 `/thinking`。
- **认证**：一张通用 header 表，值中 `${VAR}` 在选择供应商时逐个 provider 展开。变量缺失不是全局致命错误：该 provider 标记为不可用，不影响其他 provider。不枚举认证类型。配置中出现疑似明文密钥只告警、不阻断。
- **校验**：未知字段、悬空引用、缺失必填字段一律硬报错，退出码 2，指出位置。`toml` 必须启用 `preserve_order`，否则"声明顺序即优先级"会退化成字母序。
- **供应商启用**：按声明顺序取第一个 key 变量齐全的 provider；配置的 `default_provider` 优先；`--provider` 覆盖。显式指定的 provider（`default_provider` 或 `--provider`）若 key 不齐全，报错并列出缺失的变量，不回退到其他 provider。隐式选择时没有任何可用 provider 才报错（列出每个 provider 各缺什么）。
- **默认值冲突消解**：启动时无 CLI 指定时，若配置同时给了 `default_provider` 与 `default_model` 且二者前缀不一致，或未配 `default_provider` 而隐式选中的 provider 与 `default_model` 前缀不一致，一律以**模型为准**：采用 `default_model`，provider 取其前缀对应者（该 provider 须 key 齐全，否则按上文显式指定规则报错）。thinking 取 `default_model` 的 `thinking_default`。即 `default_model` 的效力高于 `default_provider` 与隐式选择。

### 内置默认

只收 DeepSeek：`deepseek/deepseek-flash`、`deepseek/deepseek-v4-pro`，两个模型的 `context_window` 均为 1,000,000，`max_output` 均为 65536，思考档位 off/low/high/max，`echo_reasoning = false`，base_url 为 `https://api.deepseek.com`（适配器自行拼接 `/chat/completions`）。不内置其他供应商、不内置 Ollama。

## 六、供应商适配

- v1 只实现 **OpenAI Chat Completions 族**。`protocol` 的其余可选值（Responses / Anthropic Messages）在代码中预留适配器接口，v1 遇到即报"不支持"。
- 适配层职责：把标准内部消息映射为协议字段、把 SSE 增量归一化、展开思考档位片段、处理认证 header。
- **思考内容**：`reasoning_content` 仅在本地展示，**默认不回传**；`compat.echo_reasoning = true` 时才随历史回传（兼容要求回传的第三方实现）。
- **工具**：无论模型是否声明支持，始终发送 `tools` 定义（用户决策：先简单做，上游报错就报错）。不设置 `tool_choice`（默认 `auto`；`required` 在思考模式下不被上游支持）。
- **流式**：解析 `data: {...}` 与 `data: [DONE]`；按 `index` 组装多段 `tool_calls`；`usage` 取自最后一个 chunk。

## 七、工具与执行

v1 只有一个工具：`bash`。不做文件读写工具、不做图像、不做 MCP、不做子代理、不做 `working_directory`。

### `bash`

- 每次调用 `bash -c <command>`，**无持久 shell**；前后两次 bash 调用之间没有任何关联（无共享 cwd、无共享环境变量、无后台状态延续），需要跨目录操作时必须在同一条命令内完成。
- 子进程环境变量**完全继承**父进程（已知取舍：模型可通过 `env` 看到 API key）。
- 子进程 stdin 接 `/dev/null`；stdout / stderr 分别捕获。
- 超时默认 120s（可配）：超时杀整个进程组，结果标记为超时。
- 正常返回后**不清理**残留子进程——允许 `nohup ... &` 启动长期服务。
- 输出上限 16384 字节：保留头 8192 + 尾 8192，中间标注省略字节数。
- 结果格式为纯文本：stdout，若 stderr 非空则附 `[stderr]` 段，末尾 `[Command finished with exit code N]`。非零退出**不算**错误；只有 spawn 失败（命令不存在、cwd 不存在）才是错误。

### 确认与 allowlist

- `bash` 命令满足以下两个条件才自动执行：命令字符串与 allowlist 中某项完全相同，或以该项为前缀且其后紧接一个空格；且命令中不含任何一个 shell 控制字符 `; & | < > $` 与反引号、换行。此字符串匹配是原型阶段的临时简化方案，不是 shell 语法解析或安全沙箱。
- 否则弹出确认：显示会话 cwd 与完整命令，提示 `[y/N]`，默认 N。
- 被拒绝时，把"用户拒绝执行"作为工具结果喂回模型。
- `--yes` 跳过所有确认。
- 每次用户输入触发的一轮对话最多 `max_tool_steps` 次模型发起的工具调用（默认 25）；所有模型发起的调用都计数——拒绝、取消、参数解析失败、未知工具名、spawn 失败均同。超限停止并提示。

### 多工具调用

模型一次返回多个 tool call 时，**顺序执行**、逐个确认，全部结果按协议放回同一条消息组。

## 八、上下文

**v1 不做预算管理，不做压缩，不做截断。**

请求超出模型窗口时，API 会返回错误；本层如实展示错误并提示 `/clear`。`context_window` 字段保留，仅用于错误提示与未来扩展。

后果已知：工具密集的长会话可能在单轮内撞上 1M 窗口而上限失败，此时只能重开会话。

## 九、HTTP 与认证

- **不重试**（429 / 5xx / 断流均直接报错）。
- 超时：连接 10s、首字节 60s、流式空闲 120s。
- 代理：读取 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY`（大小写均认）。
- TLS：`webpki-roots` 编入二进制；支持 `SSL_CERT_FILE` 覆盖以兼容企业自签证书。

## 十、渲染

- 思考内容：暗淡色实时输出。
- 正文：原样输出，不渲染 markdown。
- 工具调用：单行 `→ bash: <命令>`，结果缩进显示，截断处标注省略字节数。
- 不提供颜色开关：不做 `--no-color` 参数，不读 `NO_COLOR` 环境变量。

## 十一、明确不做

- 一次性执行模式、stdin 管道、会话持久化、`config validate`。
- 文件读写/编辑工具、图像、MCP、子代理。
- 上下文预算管理、摘要压缩、tokenizer。
- HTTP 重试、async 运行时。
- 除 OpenAI Chat Completions 以外的协议族实现。
- Windows / macOS。

## 十二、已知取舍

1. **环境变量完全继承**：bash 能看到 `DEEPSEEK_API_KEY`，一条 `env` 即可读出。按"先简单做"接受。
2. **始终发送 tools**：不支持工具调用的模型会由上游报错，而不是在本地优雅降级。
3. **无上下文管理**：长会话会硬失败，只能 `/clear` 重来。
4. **无持久化**：退出即丢失全部历史。
5. **allowlist 是临时字符串匹配**：规则完全匹配，或命令以规则开头且后接空格；再由元字符规则兜底。它不是 shell 语法解析或安全沙箱，规则之外仍有误判空间。

## 十三、测试与里程碑

### 测试

- 单元：配置解析与严格校验、SSE 解析器（录制 chunk 夹具）、allowlist + 元字符判定、输出截断、思考档位展开、header `${VAR}` 展开。
- 集成：本地 mock HTTP 服务跑完整工具循环（流式 + 多次 tool call + 拒绝确认）；mock 的工具调用响应使用真实 `finish_reason: "tool_calls"`。
- CI 不调用真实 API；真实 DeepSeek 仅用于手工验收。

### 里程碑

- **M1**：配置解析/校验 + 供应商适配 + REPL 流式对话（无工具）。
- **M2**：`bash` + allowlist + 确认 + 超时/取消。
- **M3**：思考档位、斜杠命令、错误展示。
- **M4**：三目标交叉编译产物 `rho-{x86_64,aarch64}-unknown-linux-musl.tar.gz`、`rho-armv7-unknown-linux-musleabihf.tar.gz` + sha256 + README。
