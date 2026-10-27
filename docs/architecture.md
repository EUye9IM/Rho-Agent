# rho 实现架构

本文描述 rho 当前的实现结构：单 crate、单二进制、模块边界少而清晰，阻塞 I/O + 线程。

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
- **信号线程 / 提示期限线程**：`rho-sigint` 消费 self-pipe 记录 Ctrl-C；`rho-hint` 在双按窗口到期后撤掉提示符行尾的灰提示（见三）。

## 二、Crate 与模块布局

单 crate `rho`，扁平模块：

```
src/
  lib.rs         库入口（集成测试经此访问 crate；bin 只是壳）
  main.rs        参数解析（手写）、退出码、组装启动
  config/        配置加载与严格校验
    mod.rs       结构定义、内置默认配置（include_str! + 解析）
    validate.rs  悬空引用 / 前缀对应 / 必填字段校验，一次收集全部错误并报告配置路径
    expand.rs    header 值 ${VAR} 展开；明文密钥告警
  chat.rs        内部消息模型 Msg、会话历史（Vec<Msg>）、工具循环
  provider/
    mod.rs       ProviderAdapter trait（Send+Sync，Arc 共享）、按 Provider 构造（注入 compat）、
                 流式请求工作线程（取消/超时/字节上限、首字节看门狗）
    http.rs      TLS 根配置（SSL_CERT_FILE 覆盖）、代理环境变量分流（NO_PROXY）、
                 按 base_url 缓存 ureq::Agent（TLS 根与连接池复用）、错误体截断
    openai.rs    openai_chat：请求体组装（含 max_tokens）、thinking 片段顶层合并、
                 reasoning_content 处理（echo_reasoning 构造时注入）
  sse.rs         SSE 行解析器（纯函数，可夹具测试；未终结单行有字节上限）
  tools.rs       bash 执行：进程组 spawn、超时升级、双 drain 管道读取、
                 输出截断、allowlist + 元字符判定、确认
  render.rs      纯格式化函数：思考暗淡色、正文原样、工具行、带左边栏的工具结果、截断标注
                 （颜色固定使用 ANSI 转义，暗色终端下仍可用）
  repl.rs        斜杠命令、Ctrl-C 语义；循环本体依赖 Ui trait
  term.rs        终端信号处理（SIGINT、进程组信号）
  ui/
    mod.rs       Ui trait + 生产实现 TermUi（rustyline + 终端 y/N）；负责终端输出
    hint.rs      Ctrl-C 双按灰提示：Hinter/高亮、期限线程与到期擦除
    scripted.rs  测试实现 ScriptedUi（预排输入与确认、收集渲染事件）
```

依赖方向为 `repl → chat → provider → sse`。`Msg`/`ToolCall` 等共享消息类型放在 `chat.rs`，provider 通过 `StreamEvent::Final(Box<Msg>)` 使用它们；这是唯一的跨层类型引用。`config` 被各层读取，处于依赖关系底层。`render` 提供纯格式化函数，由 `repl`/`chat`/`ui` 调用；`ui` 把格式化结果写入终端。

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
    Delta { text: Option<String>, reasoning: Option<String> }, // 渲染用增量（正文/思考文本）
    Done,                                      // [DONE] 触发；worker 已校验 finish_reason（合法才继续发 Final）
    Final(Box<Msg>),                           // worker 组装完成的完整 Assistant 消息，随 Done 后发送
    Error(String),                             // HTTP 错误 / 断流 / 非法 JSON / finish_reason=length|content_filter 等截断；异常 EOF 按错误处理
}
trait ProviderAdapter: Send + Sync {
    fn endpoint(&self, p: &Provider) -> String;
    fn build_body(&self, hist: &[Msg], model: &Model, model_id: &str, thinking: Option<&Value>) -> Value;
    fn headers(&self, p: &Provider) -> Vec<(String, String)>;
    // 解析单个 chunk：累积进 acc；有 text/reasoning 增量时返回 Some(Delta)。
    // tool_calls 的组装只进 acc，不产生事件。
    fn parse_chunk(&self, chunk: &Value, acc: &mut TurnAcc) -> Option<StreamEvent>;
}
// 适配器按 Provider 构造（adapter_for(&Provider) -> Result<Arc<dyn ProviderAdapter>, String>）：
// compat 开关（echo_reasoning）构造时注入；Session 长期持有 Arc，每轮 clone 一份按值传给 worker
// （thread::spawn + detach 取消语义需 'static，不能借引用）；/provider 切换时重建。其他协议族返回可展示的 Err。
```

- **`TurnAcc` 归 worker 线程所有**：chunk 解析与累加都在工作线程内完成；channel 传渲染增量（Delta）、终态（Done/Error）与组装完成的完整 Assistant 消息（Final）。主线程只消费 Final，提交历史/执行工具都依赖它；取消时 drop Receiver，累加器随 worker 结束丢弃。

```rust
// ui — 用户交互抽象，repl / chat / tools 通过此接口访问终端
// 退出语义由 REPL 层负责：/exit、Eof、空行 Ctrl-C 双按映射为退出；
// 活动操作中信号路径的 quit 由 agent loop 检测（见五）

trait Ui {
    fn read_line(&mut self) -> ReadOutcome;                       // REPL 输入 / 斜杠命令
    fn confirm(&mut self, cwd: &str, cmd: &str) -> ConfirmOutcome; // bash 确认
    fn print(&mut self, ev: &RenderEvent);                        // 渲染输出
    fn set_ctrl_c_hint(&mut self, armed: bool) {}                 // 双按灰提示开关
}
enum ReadOutcome {
    Line(String),                     // 普通输入或斜杠命令
    Interrupted { line_empty: bool }, // Ctrl-C：编辑中（false）清空输入；空行（true）进入双按窗口
    Eof,                              // Ctrl-D
}
enum ConfirmOutcome {
    Yes,        // 执行
    No,         // 拒绝，喂回模型
    Cancelled,  // Ctrl-C：取消当前回合，剩余调用按占位规则收尾，回 REPL
}
```

- 退出语义：`read_line` 只返回 `Line`/`Interrupted`/`Eof`；`/exit` 作为普通 `Line` 由 REPL 层解析为退出（斜杠命令由 repl.rs 解析）。空行 Ctrl-C 返回 `Interrupted`；REPL 层按 2 秒双按窗口判定——窗口内第二次才退出，第一次只武装并在提示符行尾显示灰提示，编辑中（`line_empty: false`）按 Ctrl-C 清行并撤销武装，任何提交的行（含空行）都清掉武装。`ConfirmOutcome::Cancelled` 是确认界面的 Ctrl-C，取消整轮（若为当前轮第二次则收尾后退出，见信号计数）。
- **Ctrl-C 的唯一记录生产者是专用信号线程**（confirm 场景）：终端保持 ISIG，Ctrl-C 产生 SIGINT，经 self-pipe/signalfd 由信号线程在锁内追加记录；`confirm` 的可中断读取由 SIGINT 唤醒返回 `Cancelled`；记录只由信号线程追加，计数只有一份。
- **rustyline 输入路径**：rustyline 原始模式会接管 SIGINT，Ctrl-C 由 rustyline 返回 `Interrupted`，REPL 层直接处理（清行/进入双按窗口）。这条路径属于按键输入；信号线程只处理活动操作（流式/工具/确认）的 SIGINT。两套路径在时间上互斥，计数保持唯一；`readline` 返回后到下一次关掉 ISIG 之间有一段 cooked 模式缝隙，落在缝隙里的 Ctrl-C 会被记成当前轮的 cancel，切轮时按旧轮单条记录作废（见五）。
- **Ui trait 只负责输入、确认和输出**：退出有两条路径——① REPL 层判定（`/exit`、`Eof`、空行 Ctrl-C 双按），以 `Result<T, Quit>` 内部错误通道传给 agent loop，agent loop 按占位规则收尾后向上返回退出；② 活动操作中信号路径的 quit（当前轮第二次 Ctrl-C）由 agent loop/工具等待循环直接检测，同样按占位规则收尾、完成工具进程清理后退出。两条路径最终都会先完成剩余调用的占位补齐与进程组清理，再退出。

- 生产实现 `TermUi`：rustyline 后端；`confirm` 用可中断读取（保持 ISIG，Ctrl-C 产生 SIGINT 唤醒读取并返回 `Cancelled`；记录由信号线程统一维护）。stdin 是否为 TTY 的检查在 `TermUi::new` 里做，`main` 只接收检查结果。
- 双按提示：`TermUi` 用 `Helper` 实现 `Hinter`/`Highlighter`，武装时在空行上返回暗淡色提示（`render::dim`），由下一次 `readline` 的重绘显示；`CTRL_C_WINDOW` 常量由 `ui` 模块提供，`repl` 用同一常量判定退出。到期由 `rho-hint` 线程撤掉提示：它只向 stdout 写一个“擦到屏幕末尾”的序列（`\x1b[J`），不移动光标，因此 rustyline 的位置模型（提示符末尾、输入行空）保持不变，也不会把提示符顶到下一行；用 `\x1b[J` 而不是 `\x1b[K` 是因为窄终端下提示会换到下一行，只擦当前行会留下尾巴。动手前提由 `should_erase_hint` 判定——处于 `readline` 期间、提示行当前为空（`Hinter` 每次重绘时更新）、stdout 是 TTY，否则只撤销武装不碰终端。判定与写入之间主线程可能已经开始重绘，因此这是尽力而为的擦除，两个方向（没擦干净 / 擦掉了刚画出的新提示）都可能发生；下一次按键重绘会自愈。
- 被 Ctrl-C 中断的输入不提交，而 rustyline 每次 `readline` 结束都会补一个换行（`Editor::readline_with` 的 `term.writeln()`），于是会在回滚区留下提示符 + 已输入内容（以及布局里的灰提示）组成的块。`CtrlCHandler` 在中断时记下输入行的显示宽度（`unicode-width`），块宽再加上 `Hinter` 记下的“最后一次布局里是否含灰提示”；`TermUi` 结合 `TIOCGWINSZ` 的列数算出回收时要上移的行数：`1 + 宽度 / 列数`——宽度刚好是列数整数倍时 rustyline 会自己补一个换行（`calculate_position` 的 `col == cols` 分支 → `refresh` 的 “generate our own newline on line wrap”），所以行数比内容高度多一，不能用向上取整。下一次 `read_line` 开始时用“`\x1b[{N}A\r\x1b[J`（上移 N 行、擦到屏幕末尾）”把这块收回重用；双按退出时在 `Drop` 里同样收回。这样打字后按 Ctrl-C 是原地清空，不多占行。`\x1b[J` 依赖“提示符块永远是最后一段输出”这个不变式：块高于屏幕时视口里全是块自己的行，`\x1b[{N}A` 被终端钳制到顶行，擦掉的仍只是块内容。stdout 不是 TTY 时不动光标。宽字符或制表符正好落在换列边界时行数会少算一行（`unicode-width` 不算 tab 的制表位展开），宁可留残影也不多上移（多上移会擦掉历史输出）。
- 到期擦除只动屏幕、不改 rustyline 缓存的 layout，所以“布局里含不含灰提示”的标志不能在到期时清零：下次中断的 `move_cursor_to_end` 仍会把光标移到含提示的 `layout.end`，回收行数要跟着布局走，否则会少上移一行。
- 测试实现 `ScriptedUi`：预排输入队列 + 预排确认结果 + 收集渲染事件，驱动 `repl → agent loop → tools` 全链路。

- `TurnAcc`（worker 线程的每流累加器）：按 `index` 组装多段 `tool_calls`、累积 text/reasoning；流正常结束时组装为完整 Assistant 消息经 `Final` 交付主线程。取消时主线程 drop Receiver，累加器随 worker 结束一起丢弃。
- 响应 / Anthropic 协议：当前实现保留 trait 分发；`protocol` 指向其他协议时返回可展示的错误。

## 四、Agent 工具循环（chat.rs）

```
用户输入 → push User msg
loop {
    body = adapter.build_body(history, model, model_id, thinking)
    worker 线程发请求并持有 TurnAcc，主线程 recv_timeout 短轮询：
        每次成功收到事件后先查标志再处理（持续有事件也逐块可取消）
        Delta     → 渲染
        Done      → 无载荷；worker 已校验 finish_reason，接收继续
        Final     → 收到完整 Assistant 消息才退出接收、进入流后处理；
                    Done 后通道断开或缺失 Final 按 Error 收尾
        Error     → 展示错误，回合结束（保留已完成历史）
        Disconnected → worker 契约要求任何结束路径必以 Final 或 Error 收尾后
                    才退出；收到 Disconnected 即防御性按 Error 收尾
        超时无事件 → 查取消标志，未取消则继续轮询
    流式期间 Ctrl-C → drop Receiver，回合终止，回 REPL（若 quit 则退出）
    流结束后：
        tool_calls 为空 → push Assistant msg，回合结束
        提交前校验：tool_call_id 唯一且非空；非法则整条按 Error 处理
        有 tool_calls → push Assistant msg；逐个（每个执行前先查 steps < max_tool_steps
            与取消标志；发现取消则当前及剩余补“用户取消执行”，回 REPL/退出）：
            超限 → 当前及剩余调用补“超限未执行”Tool 结果，回合结束
            参数解析失败/未知工具名/spawn 失败 →
                当前调用产出真实错误文本的 Tool 结果，steps += 1，
                剩余调用继续逐个判定（单调用失败后继续处理后续调用，除非同时超限/取消）
            判定(allowlist/元字符/确认/--yes)：
                ConfirmOutcome::Yes → 执行 bash（前台，超时升级，Ctrl-C 升级）
                No → “用户拒绝执行”Tool 结果，steps += 1
                Cancelled → 已执行的保留真实结果，当前及剩余补
                    “用户取消执行”Tool 结果，回 REPL
                    （若本次 Ctrl-C 是当前轮第二次，信号计数 quit 生效则收尾后退出）
            执行中超时（未被取消）→ “已部分执行（超时）”Tool 结果，
                steps += 1，继续剩余调用
            执行中被取消 → 当前调用补“已部分执行（被取消，副作用已发生）”
                Tool 结果，剩余补“用户取消执行”，
                清理进程组后回 REPL（若 quit 则退出）
            正常完成 → 真实结果 Tool 结果，steps += 1
        全部执行完 → continue（带上 tool 结果再请求）
}
```

关键点：
- **历史始终配对完整**：Assistant 的全部 tool_calls 一经提交，每个调用必有对应 Tool 消息——真实结果、真实错误文本、或“拒绝/取消/超限/部分执行”占位结果。任何单调用失败，后续调用仍逐个判定；取消/退出在任何粒度发生，剩余调用必补占位后收尾。
- **提交资格（worker 侧）**：`finish_reason` 为 `stop`/`tool_calls`/空才发 `Done`+`Final`；`length`/`content_filter` 等截断原因在 worker 内直接发 `Error`，被截断的 tool_calls 丢弃。主线程不重复判定：收到 `Final` 才提交，收到 `Error`（含通道断开）就按错误收尾。
- **取消在两个粒度检查**：接收循环逐事件检查（含持续有事件时）；工具序列逐个调用前检查。任一处发现取消即按占位规则收尾。
- **限额逐个前置检查**：每个调用执行前先检查限额；**所有模型发起的调用都计数**（含参数解析失败/未知工具名/spawn 失败的调用），防止错误调用无限循环。
- **历史即上下文**：历史（`Vec<Msg>`）原样序列化进请求；窗口超限时展示错误并提示 `/clear`。
- **reasoning 默认留在本地**：`echo_reasoning = true` 时随历史回传，其余情况序列化时丢弃（适配层职责）。
- **两条退出路径最终汇合**：REPL 层将 `/exit` 命令、空行 Ctrl-C 双按、`Eof`（Ctrl-D）映射为退出（经内部通道传 agent loop）；活动操作中信号路径的 quit（当前轮第二次 Ctrl-C）由 agent loop/工具等待循环直接检测。两路径均先完成剩余调用占位补齐与工具进程清理再退出。

## 五、线程与取消（term.rs / repl.rs）

```rust
struct Cancel {
    inner: Mutex<State>, // epoch、记录追加、切轮共用同一把锁
}
struct State { epoch: u64, records: Vec<u32 /*第几次 Ctrl-C*/>, prev_quit: bool }
```

- **异步信号安全**：信号处理器只做最小动作（向 socketpair 写一个字节，或写 signalfd）；取锁、读取 epoch 和追加记录都由**专用信号线程**在普通线程上下文完成。活动操作（流式/工具/确认）期间，Ctrl-C 记录只由这个线程产生；REPL 输入态的 Ctrl-C 走 rustyline 按键路径（见 ui 模块一节）。专用线程与主线程切轮在同一锁内串行化——信号线程先拿锁则记入旧轮并被结转，切轮先拿锁则记入新轮，两种顺序新轮都能看到，旧轮尚未消费的 quit 会结转，下一轮仍触发退出。
- 主线程查询：锁内看 `prev_quit`（则退出）或当前轮记录——第 1 次为 cancel，第 2 次及以上为 quit；旧轮非 quit 记录作废。
- 消费点：渲染/接收循环（`recv_timeout` 短轮询，超时即查；**成功收到事件也先查再处理**）、工具等待循环（触发进程组升级终止；发现 quit 只终止进程组并**返回“quit 已请求”给 agent loop**，由 agent loop 收尾）、agent loop（按占位规则补齐当前及剩余调用后回 REPL 或统一退出程序）。工具执行/清理期间由当前轮信号记录承接第二次 Ctrl-C。
- 流线程取消：主线程 drop Receiver 并置位取消标志（`Arc<AtomicBool>`）→ worker 在读取循环中每次 `read` 返回后、以及每次 `send` 前轮询它，立即退出并静默返回（主线程已放弃接收）。读超时是 socket 级的 `timeout_read`：ureq 只有一个旋钮且同样作用于读取响应头，所以取空闲上限 120s，不用短超时轮询——上游“思考”数秒才回头是常态。取消后若连接完全无数据，worker 最多滞留到该读超时；因此取消的及时性以“下一块数据到达”为界。另有一条整轮响应体累计上限 32MiB（超限按流错误收尾），channel 为无界 `mpsc`：无界使发送永不阻塞，配合 drop-Receiver 早退语义最简单。
- 工具执行与进程组终止：`setsid` 建进程组；stdout/stderr 各起一个 drain 线程并行读取。截断缓冲为两段结构：头部前缀（上限前半）+ 滑动尾部窗口（上限后半），中间字节计数丢弃，按头尾保留语义处理。**主线程是唯一 reaper**：`waitpid` 只由主线程执行，退出状态存入共享标志；drain 线程只轮询共享退出标志，观察到退出后进入收尾阶段（短超时上限内继续读取管道中已缓冲的剩余数据，防止后台进程持续写入），超时或 EOF 即退出线程并关闭自己持有的读端；主线程只负责 `waitpid`，读端由 drain 线程持有。终止升级以进程组为准：SIGTERM 发 pgid → 2s → 对 pgid 发 SIGKILL → 主线程回收 bash（直接子进程回收条件与进程组清理条件分离）。
- **Ctrl-C 的两套记录互不复用**：活动操作（流式/工具/确认）的计数归 `Cancel`（信号线程写入，只看次数和 epoch，没有时间概念）；提示符态的双按武装归 `repl` 的一个 `Option<Instant>`，窗口常量 `CTRL_C_WINDOW` 与 `ui` 模块共用，从不写入 `Cancel`。rustyline 在 `readline` 期间清掉 ISIG，两套记录在时间上互斥；唯一的交叠是 `readline` 返回到下一次关 ISIG 之间的 cooked 模式缝隙，那里收到的 Ctrl-C 会被记成当前轮的一次 cancel，而切轮会丢弃旧轮的单条 cancel，用户可见行为不变（只有凑巧与下一次请求竞争的 cancel 才会取消那一轮）。
- quit（当前轮第二次 Ctrl-C）被消费即走退出路径；切轮时旧轮未消费的 quit 结转为 `prev_quit`，下一轮查询时仍触发退出。

## 六、错误与退出码

```
exit 2：参数用法错误（含 stdin 非 TTY）、配置解析/校验失败、显式指定的供应商 key 不齐全、隐式选择时无任何可用供应商（含缺失变量列表）
exit 1：REPL 无法继续的内部运行时错误（渲染 I/O 错误等）；panic=abort 下 panic 直接 abort，exit 1 用于其他 REPL 内部错误
exit 0：/exit、空闲态 Ctrl-C 双按、Ctrl-D（均正常退出）
```

- 轮内错误（断流、上游 4xx/5xx）**展示后回到 REPL**，保留已完成历史；exit 1 用于 REPL 本身无法继续的内部错误。
- 所有配置错误统一走 `config::validate` 收集，一次报全部（语义错误给字段路径，TOML 语法错误给行号）。

## 七、测试分层

| 层 | 方式 |
|---|---|
| `sse.rs` / `config` / `tools` 判定与截断 / `expand` | 纯单元测试，录制夹具 |
| `render` 格式 / `ui::prompt_rows`（回收时上移的行数）/ `ui::should_erase_hint` / `repl::within_window`（双按窗口） | 纯单元测试，无 TTY |
| `provider::openai` 解析 | 单元测试：构造 JSON chunk → StreamEvent |
| agent loop + tools | 集成：fake adapter 吐预编排 `StreamEvent` 序列 + `ScriptedUi` 驱动输入/确认 |
| provider + HTTP 层 | 集成：本地 mock SSE 服务（std TcpListener 手写），测协议映射与流读取 |
| 端到端工具循环 | 集成：mock SSE 服务驱动多轮真实 HTTP 请求，验证占位 Tool 消息的配对/顺序/第二次请求序列化（含 echo_reasoning） |
| 取消/退出 | 集成：`ScriptedUi` 注入 `Line("/exit")`、`Eof` 验证退出路径；注入 `Interrupted{line_empty}` 序列验证 2 秒双按窗口、编辑中撤销、提交行清除武装（并断言 `set_ctrl_c_hint` 序列）；直接注入信号记录（第 1 次/第 2 次）验证取消与退出路径；信号到记录、灰提示到期擦除与屏幕行回收（含到期后重新武装、窄终端提示换行、宽字符/tab 边界）这一段（`term.rs` / `ui` 模块）手工验收 |
| 终端交互 / 真实信号 | 手工验收 |
