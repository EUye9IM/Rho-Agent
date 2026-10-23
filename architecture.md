# Rho-agent 架构设计

对应 `design.md` 定稿。目标：单 crate、单二进制、模块边界少而清晰，阻塞 I/O + 线程。

## 一、总体结构

```
┌─────────────────────────── main thread ───────────────────────────┐
│                                                                    │
│  repl ──► agent loop ──► adapter(openai_chat) ──► http(ureq)      │
│    ▲         │              ▲                        │             │
│    │         │              │ events (mpsc)          │             │
│    │         ▼              │                        │             │
│  render ◄────┴──────────────┘                        │             │
│    │                                                 │             │
│    └── confirm(y/N) ◄── tools::bash(前台执行)          │             │
└─────────────────────────────────────────────────────┼─────────────┘
                                                      │ worker thread
                                              流式读取 → 逐块下发
```

- **主线程**：REPL、agent loop、渲染、确认交互、工具执行（阻塞在前台，Ctrl-C 由信号处理转取消）。
- **工作线程**：每轮请求一个，阻塞读 HTTP 流，把增量事件经 `mpsc` 发回主线程；主线程收到取消标志后放弃该线程（detach）。

## 二、Crate 与模块布局

单 crate `rho`，扁平模块，不上 workspace：

```
src/
  main.rs        参数解析（手写，不用 clap）、退出码、组装启动
  config/        配置加载与严格校验
    mod.rs       结构定义、内置默认配置（include_str! + 解析）
    validate.rs  悬空引用 / 前缀对应 / 必填字段校验，错误带位置
    expand.rs    header 值 ${VAR} 展开；明文密钥告警
  chat.rs        内部消息模型 Msg、会话历史 History、工具循环
  provider/
    mod.rs       ProviderAdapter trait、按 protocol 分发
    openai.rs    openai_chat：请求体组装、thinking 片段顶层合并、
                 reasoning_content 处理（echo_reasoning）
  sse.rs         SSE 行解析器（纯函数，可夹具测试）
  tools.rs       bash 执行：进程组 spawn、超时升级、双 drain 管道读取、
                 输出截断、allowlist + 元字符判定、确认
  render.rs      渲染：思考暗淡色、正文原样、工具行、截断标注
                 （无颜色开关，暗色终端下暗淡色仍可用 ANSI 固定转义）
  repl.rs        斜杠命令、Ctrl-C 语义；循环本体依赖 Ui trait
  term.rs        终端信号处理（SIGINT、进程组信号）
  ui.rs          Ui trait + 生产实现（rustyline + 终端 y/N）
```

依赖方向单向向下：`repl → chat → provider → sse`；`config` 被所有层读，但不反向依赖任何层。`render` 只被 `repl`/`chat` 调用。

## 三、关键数据类型

```rust
// chat.rs — 内部消息模型（与协议解耦，适配层负责映射）
enum Msg {
    System(String),
    User(String),
    Assistant { text: Option<String>, reasoning: Option<String>, tool_calls: Vec<ToolCall> },
    Tool { tool_call_id: String, content: String },
}
struct ToolCall { id: String, name: String, arguments: String } // arguments 保持原始 JSON 字符串
```

```rust
// provider/mod.rs
enum StreamEvent {
    Delta { text: Option<String>, reasoning: Option<String> }, // 渲染用增量
    ToolDelta { index: usize, id: Option<String>, name: Option<String>, args_frag: String },
    Usage { prompt: u64, completion: u64 },
    Done { finish_reason: Option<String> },   // [DONE] 触发；finish_reason=stop/None 才有提交资格
    Error(String),                             // HTTP 错误 / 断流 / 非法 JSON / finish_reason=length|content_filter 等截断；异常 EOF 不当成功
}
trait ProviderAdapter {
    fn endpoint(&self, p: &Provider) -> String;
    fn build_body(&self, hist: &[Msg], model: &Model, thinking: Option<&Value>) -> Value;
    fn headers(&self, p: &Provider) -> Vec<(String, String)>;
    fn parse_chunk(&self, chunk: &Value, acc: &mut TurnAcc) -> Vec<StreamEvent>;
}
```

- **`TurnAcc` 归 worker 线程所有**：chunk 解析与累加都在工作线程内完成；channel 只传渲染用增量（Delta）与终态（Done/Error + 组装完成的 assistant 消息）。主线程不重复累加，取消时无需跨线程回收累加器。

```rust
// ui.rs — 用户交互抽象，repl / chat / tools 依赖此接口而非终端
type UiResult<T> = Result<T, QuitRequested>;   // Err = 用户请求退出
struct QuitRequested;

trait Ui {
    fn read_line(&mut self) -> UiResult<ReadOutcome>;         // REPL 输入 / 斜杠命令
    fn confirm(&mut self, cwd: &str, cmd: &str) -> UiResult<ConfirmOutcome>; // bash 确认
    fn print(&mut self, ev: &RenderEvent);                    // 渲染输出
}
enum ReadOutcome {
    Line(String),                     // 普通输入或斜杠命令
    Interrupted { line_empty: bool }, // Ctrl-C：编辑中（false）清空输入；空行（true）退出
    Eof,                              // Ctrl-D
}
enum ConfirmOutcome {
    Yes,        // 执行
    No,         // 拒绝，喂回模型
    Cancelled,  // 第一次 Ctrl-C：取消当前回合（agent loop 丢 TurnAcc，回 REPL）
}
```

- 退出语义统一走 `UiResult`：`read_line` 的 `Err` 覆盖空闲态/空行 Ctrl-C 与 `/exit`；`confirm` 的 `Err` 是确认界面第二次 Ctrl-C；`ConfirmOutcome::Cancelled` 是确认界面第一次 Ctrl-C，取消整轮。
- agent loop 收到 `Err(Quit)` 直接终止回合，不自己判断第几次 Ctrl-C。

- 生产实现 `TermUi`：rustyline 后端，`confirm` 走 stdin y/N；`stdin 非 TTY 拒绝启动`的检查在 `TermUi::new` 里做，不进 main 流程。
- 测试实现 `ScriptedUi`：预排输入队列 + 预排确认结果 + 收集渲染事件，驱动 `repl → agent loop → tools` 全链路，无网络无 TTY。

- `TurnAcc`（agent loop 内的每轮累加器）：按 `index` 组装多段 `tool_calls`、累积 text/reasoning。取消时直接丢弃整个 `TurnAcc`，已完成的 `Msg` 已进 `History`，天然满足"保留已完成、丢弃未完成"。
- 响应 / Anthropic 协议：v1 只留 trait，`protocol` 值分发时报"不支持"。

## 四、Agent 工具循环（chat.rs）

```
用户输入 → push User msg
loop {
    body = adapter.build_body(history, model, thinking)
    worker 线程发请求并持有 TurnAcc，主线程 recv_timeout 短轮询：
        Delta     → 渲染
        Done      → 流结束（finish_reason=stop/None 才有提交资格）
        Error     → 展示错误，回合结束（保留已完成历史）
        超时无事件 → 检查取消标志，未取消则继续轮询
    流式期间 Ctrl-C → drop Receiver，回合终止，回 REPL
    流结束后：
        无 tool_calls → push Assistant msg，回合结束
        提交前校验：tool_call_id 唯一且非空；非法则整条按 Error 处理
        有 tool_calls → push Assistant msg；逐个（每个执行前先查 steps < max_tool_steps）：
            超限 → 当前及剩余调用补“超限未执行”Tool 结果，回合结束
            参数解析失败/未知工具名/spawn 失败 →
                当前调用产出真实错误文本的 Tool 结果，剩余调用继续逐个判定
                （单调用失败不终止回合，除非同时超限/取消）
            判定(allowlist/元字符/确认/--yes)：
                ConfirmOutcome::Yes → 执行 bash（前台，超时升级，Ctrl-C 升级）
                No → “用户拒绝执行”Tool 结果，steps += 1
                Cancelled → 已执行的保留真实结果，当前及剩余补
                    “用户取消执行”Tool 结果，回 REPL
                UiResult::Err(Quit) → 同 Cancelled，但退出程序
            执行中被取消/超时 → “已部分执行（被取消/超时）”Tool 结果
                （副作用已发生，不伪装成未执行），steps += 1
            正常完成 → 真实结果 Tool 结果，steps += 1
        全部执行完 → continue（带上 tool 结果再请求）
}
```

关键点：
- **历史永远配对完整**：Assistant 的全部 tool_calls 一经提交，每个调用必有对应 Tool 消息——真实结果、真实错误文本、或“拒绝/取消/超限/部分执行”占位结果。任何单调用失败不中断后续调用的判定。
- **提交资格**：只有 `Done{finish_reason: stop|None}` 才提交 assistant 消息；`length`/`content_filter` 等截断原因按 Error 处理，不把被截断的 tool_calls 提交执行。
- **限额逐个前置检查**：在执行每个调用前检查，不执行满批后才停，保证硬上限不被突破。
- **历史即上下文**：`History` 原样序列化进请求，无预算管理；窗口超限错误原样展示并提示 `/clear`。
- **reasoning 不回传**：序列化时丢弃 `reasoning`，除非 `echo_reasoning = true`（适配层职责）。
- **退出由 Ui 统一承载**：`read_line`/`confirm` 返回 `Err(Quit)`；空闲态/空行 Ctrl-C 与 `/exit` 同路径。

## 五、线程与取消（term.rs / repl.rs）

```rust
struct Cancel {
    cancel:   AtomicBool,   // 第一次 Ctrl-C：取消当前操作
    quit:     AtomicBool,   // 第二次 Ctrl-C：请求退出
}
```

- 信号处理回调只置标志（cancel 置位时后续 Ctrl-C 置 quit），不做复杂工作。
- 消费点：渲染/接收循环（`recv_timeout` 短轮询，超时即查标志）、工具等待循环（触发进程组升级终止；发现 quit 则终止后退出程序）、agent loop（丢弃回合回 REPL；发现 quit 则退出程序）。工具执行/清理期间无 Ui 阻塞点，第二次 Ctrl-C 的退出由 quit 标志承接。
- 流线程取消：主线程 drop Receiver → worker 下一次 `send` 报错即退出循环（无需继续排空 HTTP 流，不主动 close 连接）。channel 为无界 `mpsc`：无界使发送永不阻塞，配合 drop-Receiver 早退语义最简单。
- 工具执行与进程组终止：`setsid` 建进程组；stdout/stderr 各起一个 drain 线程并行读取，写入有界环形缓冲（超限后丢弃并计数，不停止读取）；bash 退出后即停止采集，不等待后台进程关闭继承的写端。终止升级不依赖 bash 单体退出状态：SIGTERM 发 pgid → 2s → 无论直接子进程状态如何，对 pgid 发 SIGKILL → 回收 bash（直接子进程回收条件与进程组清理条件分离）。
- quit 标志在启动下一轮前清零；cancel 置位时后续 SIGINT 置 quit。

## 六、错误与退出码

```
exit 2：参数用法错误、配置解析/校验失败、显式指定的供应商 key 不齐全、隐式选择时无任何可用供应商（含缺失变量列表）
exit 1：运行中不可恢复的网络/协议错误（一轮内 SSE 中断等按轮内错误展示，不退出）
exit 0：/exit、空闲态 Ctrl-C
```

- 轮内错误（断流、上游 4xx/5xx）**展示后回到 REPL**，保留已完成历史；仅启动阶段错误直接退出。
- 所有配置错误统一走 `config::validate` 收集，一次报全部（带 TOML 位置）。

## 七、测试分层

| 层 | 方式 |
|---|---|
| `sse.rs` / `config` / `tools` 判定与截断 / `expand` | 纯单元测试，录制夹具 |
| `provider::openai` 解析 | 单元测试：构造 JSON chunk → StreamEvent |
| agent loop + tools | 集成：fake adapter 吐预编排 `StreamEvent` 序列 + `ScriptedUi` 驱动输入/确认，无网络无 TTY |
| provider + HTTP 层 | 集成：本地 mock SSE 服务（std TcpListener 手写），测协议映射与流读取 |
| 端到端工具循环 | 集成：mock SSE 服务驱动多轮真实 HTTP 请求，验证占位 Tool 消息的配对/顺序/第二次请求序列化（含 echo_reasoning） |
| 取消 | 单元/集成：直接置 `Cancel` 标志（等效 Ctrl-C）、置 quit 验证退出路径；Ui 退出路径用 `ScriptedUi` 注入 `Err(Quit)` 验证；信号→标志一段（`term.rs`）手工验收 |
| 终端交互 / 真实信号 | 手工验收 |

## 八、与里程碑对应

- M1 = `main` + `config` + `provider` + `sse` + `repl`（无工具路径）
- M2 = `tools` + `term`（确认、超时、取消）
- M3 = thinking 档位展开、斜杠命令补全、错误展示打磨
- M4 = 交叉编译产物与体积/RSS 核查
