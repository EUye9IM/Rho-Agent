# rho 实现架构

本文描述 rho 当前的实现结构：单 crate、单二进制、模块边界少而清晰，阻塞 I/O + 线程。

## 一、总体结构

```
┌─────────────────────────── main thread ───────────────────────────┐
│                                                                    │
│  repl ──► agent loop ──► adapter(protocol) ──► http(ureq)         │
│    ▲         │              ▲                        │             │
│    │         │              │ events (mpsc)          │             │
│    │         ▼              │                        │             │
│  render ◄────┴──────────────┘                        │             │
│    │                                                 │             │
│    └── tools: bash / read / write / edit             │             │
│        (前台执行，Ctrl-C 由信号处理转取消)              │             │
└─────────────────────────────────────────────────────┼─────────────┘
                                                      │ worker thread
                                              流式读取 → 逐块下发
```

- **主线程**：REPL、agent loop、渲染、工具执行（阻塞在前台，Ctrl-C 由信号处理转取消）。
- **工作线程**：每轮请求一个，阻塞读 HTTP 流，把增量事件经 `mpsc` 发回主线程；主线程收到取消标志后放弃该线程（detach）。
- **信号线程 / 提示期限线程**：`rho-sigint` 消费 self-pipe 记录 Ctrl-C；`rho-hint` 在双按窗口到期后撤掉提示符行尾的灰提示（见三）。

## 二、Crate 与模块布局

单 crate `rho`，扁平模块：

```
src/
  lib.rs         库入口（集成测试经此访问 crate；bin 只是壳）
  main.rs        参数解析（手写）、退出码、组装启动
  config/        配置加载与严格校验
    mod.rs       结构定义、配置来源解析（--config > 默认路径 > 内置）、--init 写出内置配置、内置默认配置（include_str! + 解析）
    validate.rs  悬空引用 / 前缀对应 / 取值校验，一次收集全部语义错误并报告配置路径
    expand.rs    header 值 ${VAR} 展开；明文密钥告警
  message.rs     与协议解耦的消息模型 Msg / AssistantMsg / ToolCall（chat 与 provider 共用）
  commands.rs    斜杠命令元数据、解析、执行与补全候选；
                 执行只修改 Session 并返回 CommandEffect，输出由 repl 交给 Ui
  completion.rs  补全协议：CompletionProvider trait、Candidate / Completion；
                 不依赖终端、命令或 Session
  chat.rs        会话历史（Vec<Msg>，对外只读）、通用工具循环与批次编排；
                 StreamLauncher 注入边界（生产 worker 线程，测试可替换）；
                 工具注册表由 Session 持有
  provider/
    mod.rs       ProviderAdapter trait（Send+Sync，Arc 共享）、FinishReason / ChunkOutcome、
                 按 Provider 构造、流式请求工作线程（取消/超时/字节上限、首字节看门狗）
    common.rs    适配器共用：模型名解析、默认 header、thinking 片段合并
    http.rs      代理环境变量分流（NO_PROXY）、
                 按 base_url 缓存 ureq::Agent（连接池复用）、错误体截断、
                 给 Agent 注入 DoH 解析器
    dns.rs       DoH 解析器：配置的端点优先，按顺序查 A/AAAA；端点全部失败
                 才回退系统解析器，结果按应答 TTL 缓存
                 （Android/Termux 等系统解析器不可用的环境靠它解析域名）
    openai.rs    openai_chat：/chat/completions、messages、max_tokens、reasoning_content
                 （echo_reasoning 构造时注入）、`[DONE]` 结束标记
    openai_responses.rs
                 openai_responses：/responses、input items、max_output_tokens、
                 function_call / function_call_output、reasoning summary 与最终 response 事件
  sse.rs         SSE 行解析器（纯函数，可夹具测试；单行有字节上限，完整行与未终结残留同样适用）
  tools/         工具中间层与内置工具：
    mod.rs       Tool trait、ToolSpec、ToolResult、ToolStatus、ToolContext、注册表
    bash.rs      bash schema/参数/进程组执行；单 pipe 捕获与末尾字符截断
    read.rs      全文文本校验、行偏移、行号、50k 字符与 2000 行上限
    write.rs     仅新建、自动建父目录、拒绝 NUL、直接写入
    edit.rs      顺序 edits、精确匹配、replace_all、换行归一化、失败相似行提示
  render.rs      有状态渲染器 Renderer + RenderEvent：思考块暗淡色、正文原样、工具行、
                 带左边栏的工具结果；返回待写文本，不直接写终端
                 （颜色固定使用 ANSI 转义，暗色终端下仍可用）
  repl.rs        REPL 循环、Ctrl-C 语义；斜杠命令委托给 commands，输出经 Ui
  term.rs        终端信号处理（SIGINT、进程组信号）
  ui/
    mod.rs       Ui trait + 生产实现 TermUi（rustyline）；负责终端输出
    helper.rs    rustyline helper：Ctrl-C 灰提示 + 补全候选适配
    hint.rs      Ctrl-C 双按灰提示：提示状态、期限线程与到期擦除
    scripted.rs  测试实现 ScriptedUi（预排输入、收集渲染事件）
```

依赖方向为 `repl → commands/chat/ui`；`commands → chat/completion`，`chat → provider → sse/message`，`chat → tools → term`，`ui → completion/render → tools`。`completion` 只定义补全协议，`ui` 只把候选适配到 rustyline；`commands` 持有命令与候选语义，`repl` 负责按调用注入当前快照。`Msg`/`AssistantMsg`/`ToolCall` 放在 `message.rs`，chat 与 provider 都只依赖这个共享模型，provider 不再反向依赖 chat。`config` 被各层读取，处于依赖关系底层。`render` 提供有状态渲染器（`Renderer`）与 `RenderEvent`：`chat` 只产生 `RenderEvent`，`ui` 持有 `Renderer` 把事件序列化为待写文本，按标准输出/标准错误分流写入终端。

## 三、关键数据类型

```rust
// message.rs — 内部消息模型（与协议解耦，适配层负责映射）
enum Msg {
    System(String),
    User(String),
    Assistant(AssistantMsg),   // 载荷见下；流终态与历史共用同一类型
    Tool { tool_call_id: String, content: String },
}
struct AssistantMsg { text: Option<String>, reasoning: Option<String>, tool_calls: Vec<ToolCall> }
struct ToolCall { id: String, name: String, arguments: String } // arguments 保持原始 JSON 字符串
```

```rust
// tools/mod.rs
struct ToolSpec { name: String, description: String, parameters: Value }
enum ToolStatus { Success, Failure, Interrupted }
struct ToolResult {
    output: String,             // 给模型
    display: Option<String>,    // 给用户；None 表示不展示结果体
    status: ToolStatus,
}
struct ToolContext<'a> { cwd: &'a Path, cancel: &'a Cancel }
trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn invoke(&self, args: Value, ctx: &ToolContext<'_>) -> ToolResult;
    fn summary(&self, args: &Value) -> String; // 调用行摘要，默认紧凑 JSON
}
```

- `ToolRegistry` 按声明顺序保存工具，`Session.tools` 默认注册 `bash` / `read` / `write` / `edit`。provider 的 `build_body` 接收 `&[ToolSpec]`，Chat Completions 与 Responses 只负责映射成各自的 tools 数组，不再引用固定工具名。
- 每个工具自己持有 schema、`#[serde(deny_unknown_fields)]` 参数结构、语义校验和执行逻辑；公共层只做注册、JSON object 校验、调用、结果提交和历史配对。
- `ToolStatus`：`Success` 继续；`Failure`（参数错误、非零退出、超时、文件错误等）继续让模型自纠；`Interrupted` 记录当前结果、剩余调用补占位并停止本轮。取消/超限/终端失败的占位仍由 agent 写入历史。

```rust
// provider/mod.rs
enum StreamEvent {
    Delta { text: Option<String>, reasoning: Option<String> }, // 渲染用增量（正文/思考文本）
    Final(Box<AssistantMsg>),                   // worker 组装完成的完整 Assistant 消息
    Error(String),                              // HTTP 错误 / 断流 / 非法 JSON / finish_reason=length|content_filter 等截断；异常 EOF 按错误处理
}
// 终态只有 Final/Error 两种：结束标记与 finish_reason 的提交资格判定完全内化在 worker，
// 主线程收到即定论。流式请求的主线程侧句柄 StreamHandle 绑定接收器与取消标志，
// abandon（置位取消标志 + drop Receiver）是唯一放弃路径。
trait ProviderAdapter: Send + Sync {
    fn endpoint(&self, p: &Provider) -> String;
    fn build_body(
        &self,
        hist: &[Msg],
        model: &Model,
        model_id: &str,
        thinking: Option<&Value>,
        tools: &[ToolSpec],
    ) -> Value;
    fn headers(&self, p: &Provider) -> Vec<(String, String)>;
    // 原始载荷是否表示流结束（OpenAI Chat 的 [DONE]）。
    fn is_stream_end(&self, payload: &str) -> bool;
    // 解析单个 JSON chunk：增量与工具调用累积进 acc；协议最终事件返回 Done。
    fn parse_chunk(&self, chunk: &Value, acc: &mut TurnAcc) -> ChunkOutcome;
}
// 适配器按 Provider 构造（adapter_for(&Provider) -> Result<Arc<dyn ProviderAdapter>, String>）：
// compat 开关（echo_reasoning）构造时注入；Session 长期持有 Arc，每轮 clone 一份按值传给 worker
// （thread::spawn + detach 取消语义需 'static，不能借引用）；/provider 切换时重建。
// Protocol::parse 决定 openai_chat / openai_responses；未知协议返回可展示的 Err。
```

- **`TurnAcc` 归 worker 线程所有**：chunk 解析与累加都在工作线程内完成；channel 传渲染增量（Delta）与终态（Final/Error）。主线程只消费 Final，提交历史/执行工具都依赖它；取消时 drop Receiver，累加器随 worker 结束丢弃。`tool_calls` 按 index 归位到具名的 `PartialToolCall`（id/name/arguments 可分批到达）。`finish_reason` 是内部 `FinishReason`，适配器把协议字符串（stop/tool_calls/length/content_filter/max_output_tokens 等）归一化后写入；缺失 reason 仍按可提交处理。

```rust
// ui — 用户交互抽象，repl / chat 通过此接口访问终端
// 退出语义由 REPL 层负责：/exit、Eof、空行 Ctrl-C 双按映射为退出；
// 活动操作中信号路径的 quit 由 agent loop 检测（见五）

trait Ui {
    fn read_line(&mut self, completer: Arc<dyn CompletionProvider>) -> ReadOutcome;
    fn print(&mut self, ev: &RenderEvent);                        // 渲染输出
    fn set_ctrl_c_hint(&mut self, armed: bool) {}                 // 双按灰提示开关
    fn take_fatal(&mut self) -> Option<String> { None }           // 渲染/读取 I/O 失败（REPL 映射 exit 1）
}
enum ReadOutcome {
    Line(String),                     // 普通输入或斜杠命令
    Interrupted { line_empty: bool }, // Ctrl-C：编辑中（false）清空输入；空行（true）进入双按窗口
    Eof,                              // Ctrl-D
}
```

- 退出语义：`read_line` 只返回 `Line`/`Interrupted`/`Eof`；`/exit` 作为普通 `Line` 由 REPL 层解析为退出（斜杠命令由 commands.rs 解析）。空行 Ctrl-C 返回 `Interrupted`；REPL 层按 2 秒双按窗口判定——窗口内第二次才退出，第一次只武装并在提示符行尾显示灰提示，编辑中（`line_empty: false`）按 Ctrl-C 清行并撤销武装，任何提交的行（含空行）都清掉武装。
- **Ctrl-C 的唯一记录生产者是专用信号线程**（活动操作）：终端保持 ISIG，Ctrl-C 产生 SIGINT，经 self-pipe（socketpair）由信号线程在锁内追加记录；记录只由信号线程追加，计数只有一份。
- **rustyline 输入路径**：rustyline 原始模式会接管 SIGINT，Ctrl-C 由 rustyline 返回 `Interrupted`，REPL 层直接处理（清行/进入双按窗口）。这条路径属于按键输入；信号线程只处理活动操作（流式/工具）的 SIGINT。两套路径在时间上互斥，计数保持唯一；`readline` 返回后到下一次关掉 ISIG 之间有一段 cooked 模式缝隙，落在缝隙里的 Ctrl-C 会被记成当前轮的 cancel，切轮时按旧轮单条记录作废（见五）。
- **Ui trait 只负责输入和输出**：退出有两条路径——① REPL 层判定（`/exit`、`Eof`、空行 Ctrl-C 双按）发生在回合之间的输入阶段，此时没有进行中的调用，直接退出；② 活动操作中信号路径的 quit（当前轮第二次 Ctrl-C）由 agent loop/工具等待循环直接检测，按占位规则收尾、完成工具进程清理后以 `ChatResult` 的 `Err(Quit)` 上抛退出。`Quit` 只承载路径 ② 的回合内传播。

- 生产实现 `TermUi`：rustyline 后端。stdin 是否为 TTY 的检查在 `TermUi::new` 里做，`main` 只接收检查结果。`TermUi` 持有 `Renderer` 驱动事件渲染，并在每次 `read_line` 开头（画提示符前）调用 `Renderer::reset` 收尾上一轮可能残留的思考块，避免暗淡色跨到提示符；Ctrl-C 双按退出不再经过 `read_line`，由 `Drop` 再做一次收尾。
- 双按提示：`ui::helper::ReplHelper` 实现 `Hinter`/`Highlighter`，武装时在空行上返回暗淡色提示（`render::dim`），由下一次 `readline` 的重绘显示；`CTRL_C_WINDOW` 常量由 `ui` 模块提供，`repl` 用同一常量判定退出。到期由 `rho-hint` 线程撤掉提示：它只向 stdout 写一个“擦到屏幕末尾”的序列（`\x1b[J`），不移动光标，因此 rustyline 的位置模型（提示符末尾、输入行空）保持不变，也不会把提示符顶到下一行；用 `\x1b[J` 而不是 `\x1b[K` 是因为窄终端下提示会换到下一行，只擦当前行会留下尾巴。动手前提由 `should_erase_hint` 判定——处于 `readline` 期间、提示行当前为空（`Hinter` 每次重绘时更新）、stdout 是 TTY，否则只撤销武装不碰终端。判定与写入之间主线程可能已经开始重绘，因此这是尽力而为的擦除，两个方向（没擦干净 / 擦掉了刚画出的新提示）都可能发生；下一次按键重绘会自愈。
- 被 Ctrl-C 中断的输入不提交，而 rustyline 每次 `readline` 结束都会补一个换行（`Editor::readline_with` 的 `term.writeln()`），于是会在回滚区留下提示符 + 已输入内容（以及布局里的灰提示）组成的块。`CtrlCHandler` 在中断时记下输入行的显示宽度（`unicode-width`），块宽再加上 `Hinter` 记下的“最后一次布局里是否含灰提示”；`TermUi` 结合 `TIOCGWINSZ` 的列数算出回收时要上移的行数：`1 + 宽度 / 列数`——宽度刚好是列数整数倍时 rustyline 会自己补一个换行（`calculate_position` 的 `col == cols` 分支 → `refresh` 的 “generate our own newline on line wrap”），所以行数比内容高度多一，不能用向上取整。下一次 `read_line` 开始时用“`\x1b[{N}A\r\x1b[J`（上移 N 行、擦到屏幕末尾）”把这块收回重用；双按退出时在 `Drop` 里同样收回。这样打字后按 Ctrl-C 是原地清空，不多占行。`\x1b[J` 依赖“提示符块永远是最后一段输出”这个不变式：块高于屏幕时视口里全是块自己的行，`\x1b[{N}A` 被终端钳制到顶行，擦掉的仍只是块内容。stdout 不是 TTY 时不动光标。宽字符或制表符正好落在换列边界时行数会少算一行（`unicode-width` 不算 tab 的制表位展开），宁可留残影也不多上移（多上移会擦掉历史输出）。
- 到期擦除只动屏幕、不改 rustyline 缓存的 layout，所以“布局里含不含灰提示”的标志不能在到期时清零：下次中断的 `move_cursor_to_end` 仍会把光标移到含提示的 `layout.end`，回收行数要跟着布局走，否则会少上移一行。
- 补全：`completion::CompletionProvider` 定义 `complete(line, pos) -> Completion`；`commands::completion_provider(&session)` 从当前 provider/model/thinking 生成快照，`repl` 在每次 `read_line` 前传入。模型或供应商切换发生在命令执行后，下一次读取自然拿到新候选，因此 UI 不持久保存 completion 状态。`ui::helper::ReplHelper` 只把 `Candidate` 映射为 rustyline 的 `Pair`，命令名与参数语义留在 `commands`。rustyline 使用 `CompletionType::List`：先补公共前缀，再列出候选。
- 测试实现 `ScriptedUi`：预排输入队列 + 收集渲染事件，驱动 `repl → agent loop → tools` 全链路。

- `TurnAcc`（worker 线程的每流累加器）：按 `index` 组装多段 `tool_calls`、累积 text/reasoning；流正常结束时组装为完整 Assistant 消息经 `Final` 交付主线程。取消时主线程 drop Receiver，累加器随 worker 结束一起丢弃。
- OpenAI Responses：`openai_responses` 适配器把历史映射为 `input` items（user/assistant 文本、`function_call`、`function_call_output`），系统提示写入顶层 `instructions`；流式事件按 `response.output_text.delta` / reasoning summary / `function_call_arguments.delta` 累积，`response.completed` / `response.incomplete` 作为终态，`response.failed` / `error` 直接报错。
- Anthropic Messages：尚未实现；`protocol` 指向未知协议时返回可展示的错误。

## 四、Agent 工具循环（chat.rs）

```
用户输入 → push User msg
loop {
    body = adapter.build_body(history, model, model_id, thinking, tools.specs())
    worker 线程发请求并持有 TurnAcc，主线程 recv_timeout 短轮询：
        每次成功收到事件后先查标志再处理（持续有事件也逐块可取消）
        Delta     → 渲染
        Final     → 收到完整 Assistant 消息才退出接收、进入流后处理；
                    通道断开（未收到 Final）按 Error 收尾
        Error     → 展示错误，回合结束（保留已完成历史）
        Disconnected → worker 契约要求任何结束路径必以 Final 或 Error 收尾后
                    才退出；收到 Disconnected 即防御性按 Error 收尾
        超时无事件 → 查取消标志，未取消则继续轮询
    流式期间 Ctrl-C → drop Receiver，回合终止，回 REPL（若 quit 则退出）
    流结束后：
        tool_calls 为空 → push Assistant msg，回合结束
        提交前校验：tool_call_id 唯一且非空；非法则整条按 Error 处理
        有 tool_calls → push Assistant msg；run_tool_calls 逐个处理：
            限额/取消/终端 fatal 前置检查，命中时当前及剩余调用补占位
            agent 解析 arguments：必须是 JSON object
            非法 JSON / 非 object / 未知工具 → 直接构造 Failure ToolResult
            其余交给 registry 中对应 Tool::invoke(args, ToolContext { cwd, cancel })
            所有调用都计数（含非法调用），当前结果的 output 写入 Tool 消息
            status == Interrupted → 当前结果已记录，剩余补“用户取消执行”，
                停止本轮；成功或 Failure 都继续
        全部执行完 → continue（带上 tool 结果再请求）
}
```

关键点：
- **历史始终配对完整**：Assistant 的全部 tool_calls 一经提交，每个调用必有对应 Tool 消息——真实结果、参数/执行错误文本、或“取消/超限/终端失败”占位结果。任何单调用失败，后续调用仍逐个判定；取消/退出在任何粒度发生，剩余调用必补占位后收尾。
- **统一工具层**：`chat` 不再识别 `bash`、不再解析 `command`，也不直接持有 bash 专属配置；工具名、schema、参数结构、语义校验和执行归各自工具。`read` 成功时返回 `display = None`；`write` / `edit` 成功时给出一行摘要，失败时展示错误。
- **提交资格（worker 侧）**：`finish_reason` 为 `stop`/`tool_calls`/空才发 `Final`；`length`/`content_filter` 等截断原因在 worker 内直接发 `Error`，被截断的 tool_calls 丢弃。主线程收到 `Final` 才提交，收到 `Error`（含通道断开）就按错误收尾。
- **取消在两个粒度检查**：接收循环逐事件检查（含持续有事件时）；工具序列逐个调用前检查。任一处发现取消即按占位规则收尾。终端 I/O 失败（fatal）同样在流收尾与逐个调用前检查：流收尾后发现则不提交 Assistant 直接上抛；批次中途发现则剩余调用补“终端输出失败”占位后上抛；两者均映射 exit 1。
- **限额逐个前置检查**：每个调用执行前先检查限额；所有模型发起的调用都计数（含参数解析失败/未知工具/工具内部错误），防止错误调用无限循环。
- **历史即上下文**：历史（`Vec<Msg>`）原样序列化进请求；窗口超限时展示错误并提示 `/clear`。
- **reasoning 默认留在本地**：`echo_reasoning = true` 时随历史回传，其余情况序列化时丢弃（适配层职责）。
- **两条退出路径**：REPL 层将 `/exit` 命令、空行 Ctrl-C 双按、`Eof`（Ctrl-D）映射为退出——判定发生在输入阶段（回合之间），直接退出；活动操作中信号路径的 quit（当前轮第二次 Ctrl-C）由 agent loop/工具等待循环直接检测，先完成剩余调用占位补齐与工具进程清理再退出。

## 五、线程与取消（term.rs / repl.rs）

```rust
struct Cancel {
    inner: Mutex<State>, // 计数追加、查询、切轮共用同一把锁
}
struct State { count: u32 /*当前轮 Ctrl-C 次数*/, prev_quit: bool }
```

- **异步信号安全**：信号处理器只做最小动作（向 socketpair 写一个字节）；取锁和追加计数都由**专用信号线程**在普通线程上下文完成。活动操作（流式/工具）期间，Ctrl-C 记录只由这个线程产生；REPL 输入态的 Ctrl-C 走 rustyline 按键路径（见 ui 模块一节）。专用线程与主线程切轮在同一锁内串行化——信号线程先拿锁则记入旧轮并被结转，切轮先拿锁则记入新轮，两种顺序新轮都能看到，旧轮尚未消费的 quit 会结转，下一轮仍触发退出。
- 主线程查询：锁内看 `prev_quit`（则退出）或当前轮计数——1 为 cancel，≥2 为 quit；切轮（`advance_epoch`）时计数清零，旧轮 quit 结转为 `prev_quit`，旧轮单次 cancel 作废。
- 消费点：渲染/接收循环（`recv_timeout` 短轮询，超时即查；**成功收到事件也先查再处理**）、工具等待循环（触发进程组升级终止；发现 quit 只终止进程组并**返回“quit 已请求”给 agent loop**，由 agent loop 收尾）、agent loop（按占位规则补齐当前及剩余调用后回 REPL 或统一退出程序）。工具执行/清理期间由当前轮信号记录承接第二次 Ctrl-C。
- 流线程取消：主线程 drop Receiver 并置位取消标志（`Arc<AtomicBool>`）→ worker 在读取循环中每次 `read` 返回后、以及每次 `send` 前轮询它，立即退出并静默返回（主线程已放弃接收）。读超时是 socket 级的 `timeout_read`：ureq 只有一个旋钮且同样作用于读取响应头，所以取空闲上限 120s，不用短超时轮询——上游“思考”数秒才回头是常态。取消后若连接完全无数据，worker 最多滞留到该读超时；因此取消的及时性以“下一块数据到达”为界。另有一条整轮响应体累计上限 32MiB（超限按流错误收尾），channel 为无界 `mpsc`：无界使发送永不阻塞，配合 drop-Receiver 早退语义最简单。
- 工具执行与进程组终止：`bash` 用 `setsid` 建进程组，stdout/stderr 指向同一个 pipe，只起一个 drain 线程。输出按到达顺序捕获，只保留末尾 50,000 个 Unicode 字符；UTF-8 跨 read 边界由 pending 字节处理，非法字节按替换字符计数，`omitted_chars` 记录丢弃字符数。**主线程是唯一 reaper**：`waitpid` 只由主线程执行，退出状态存入共享标志；drain 线程轮询共享退出标志，观察到退出后进入 500ms 有界收尾，防止后台进程持续写入拖住返回。终止升级以进程组为准：SIGTERM 发 pgid → 2s → 对 pgid 发 SIGKILL → 主线程回收 bash。`read` / `write` / `edit` 在主线程同步执行；`edit` 每成功应用一个 edit 就写回磁盘，后续失败不回退。
- **Ctrl-C 的两套记录互不复用**：活动操作（流式/工具）的计数归 `Cancel`（信号线程写入，只看次数，按轮清零，没有时间概念）；提示符态的双按武装归 `repl` 的一个 `Option<Instant>`，窗口常量 `CTRL_C_WINDOW` 与 `ui` 模块共用，从不写入 `Cancel`。rustyline 在 `readline` 期间清掉 ISIG，两套记录在时间上互斥；唯一的交叠是 `readline` 返回到下一次关 ISIG 之间的 cooked 模式缝隙，那里收到的 Ctrl-C 会被记成当前轮的一次 cancel，而切轮会丢弃旧轮的单条 cancel，用户可见行为不变（只有凑巧与下一次请求竞争的 cancel 才会取消那一轮）。
- quit（当前轮第二次 Ctrl-C）被消费即走退出路径；切轮时旧轮未消费的 quit 结转为 `prev_quit`，下一轮查询时仍触发退出。

## 六、错误与退出码

```
exit 2：参数用法错误（含 stdin 非 TTY、`--init` 目标已存在）、配置解析/校验失败、显式指定的供应商 key 不齐全、隐式选择时无任何可用供应商（含缺失变量列表）
exit 1：REPL 无法继续的内部运行时错误（终端 I/O 失败等——agent loop 在流收尾与逐个工具调用前检查，发现即停止后续请求/执行并按占位规则收尾）；panic=abort 下 panic 直接 abort，exit 1 用于其他 REPL 内部错误
exit 0：/exit、空闲态 Ctrl-C 双按、Ctrl-D（均正常退出）
```

- 轮内错误（断流、上游 4xx/5xx）**展示后回到 REPL**，保留已完成历史；exit 1 用于 REPL 本身无法继续的内部错误。
- 语义校验错误（悬空引用、前缀不对应、取值非法等）由 `config::validate` 一次收集全部，逐条给出字段路径；TOML 语法错误与反序列化错误（未知字段、缺失必填字段、类型不符）按首个错误报告，附行号。

## 七、测试分层

| 层 | 方式 |
|---|---|
| `sse.rs` / `config` / `tools` 参数校验与截断 / `expand` | 纯单元测试，录制夹具 |
| `render` 格式 / `ui::prompt_rows`（回收时上移的行数）/ `ui::should_erase_hint` / `repl::within_window`（双按窗口） | 纯单元测试，无 TTY |
| `commands` 补全候选与命令文案 / `ui::helper` 的 Pair 适配 | 纯单元测试，无 TTY；`tests/repl_flow.rs` 覆盖模型切换后的候选刷新 |
| `provider::openai` / `provider::openai_responses` 解析 | 单元测试：构造 JSON chunk/事件 → `ChunkOutcome` / `TurnAcc` |
| `provider::dns` 报文与回退 | 单元 + 本地 mock：假 DoH 服务（std TcpListener 手写 HTTP），覆盖 A/AAAA、压缩指针、NXDOMAIN、端点失败转移、TTL 缓存；报文编解码与 `config::parse_dns_endpoint` 为纯单元测试 |
| agent loop + tools | 集成：`Session.launcher` 与 `Session.tools` 注入边界 + `ScriptedUi`，无网络驱动分支（取消/退出各粒度、占位补齐、非法参数/未知工具、配对不变式；时序用 channel 握手控制）；`BashTool::invoke` 与 `read` / `write` / `edit` 单元测试覆盖参数、截断、CRLF、相似行和文件边界 |
| provider + HTTP 层 | 集成：本地 mock SSE 服务（std TcpListener 手写），测协议映射与流读取 |
| 端到端工具循环 | 集成：mock SSE 服务驱动多轮真实 HTTP 请求，验证占位 Tool 消息的配对/顺序/第二次请求序列化（含 echo_reasoning） |
| 取消/退出 | 集成：`ScriptedUi` 注入 `Line("/exit")`、`Eof` 验证退出路径；注入 `Interrupted{line_empty}` 序列验证 2 秒双按窗口、编辑中撤销、提交行清除武装（并断言 `set_ctrl_c_hint` 序列）；直接注入信号记录（第 1 次）验证流式取消路径——quit 路径（第 2 次）的计数判定由 `term.rs` 单元测试覆盖，端到端行为由 PTY 测试覆盖 |
| 终端交互 / 真实信号 | PTY 集成（`tests/pty.rs`）：openpty 起真实 rho 二进制 + vt100 屏幕网格断言，覆盖信号到记录链路（on_sigint → self-pipe → 信号线程 → Cancel：流式取消/双按 quit）、灰提示出现与到期擦除、行回收、Tab 补全、bash 超时表现。窄终端提示换行、宽字符/tab 边界等布局细节仍手工抽查。已知竞态：行回收后新提示符出现的毫秒级窗口内写入的首字节可能丢失（macOS 实测，定位在内核 PTY 层 slave 端 `tcsetattr(TCSADRAIN)` 与 master 写入并发；人工敲键撞不上），PTY 测试在提示符后稳定 50ms（`SETTLE`）再发输入 |
