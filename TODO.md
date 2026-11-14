# TODO: 工具中间层与内置工具

状态：工具中间层与 `bash` / `read` / `write` / `edit` 已实现；动态提示词为后续。

## 已实现的设计

### 工具职责

每个工具自己负责：

- 工具名、描述和 JSON Schema。
- 参数字结构体与反序列化。
- 参数语义校验。
- 工具自己的配置（例如 bash 的超时和输出上限）。
- 执行并返回统一结果。

公共层只保留 `ToolSpec` / `ToolResult` / `ToolStatus` 等通用类型。

```rust
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn invoke(&self, args: Value, ctx: &ToolContext<'_>) -> ToolResult;
    // 实现中还提供调用行摘要的默认方法，具体工具可覆盖。
}
```

### ToolResult

```rust
pub struct ToolResult {
    /// 给模型的结果
    pub output: String,
    /// 给用户看的结果；None 表示不展示结果体
    pub display: Option<String>,
    /// UI 样式 + agent 控制流
    pub status: ToolStatus,
}

pub enum ToolStatus {
    Success,
    Failure,
    Interrupted,
}
```

状态语义：

| status | 含义 | agent 行为 | UI |
|---|---|---|---|
| `Success` | 工具完成，结果有效 | 继续 | 成功样式 |
| `Failure` | 参数错误、执行失败、超时、非零退出等 | 继续，让模型自行纠正 | 失败样式 |
| `Interrupted` | 用户 Ctrl-C 打断 | 记录当前结果，剩余调用补占位，停止本轮 | 打断样式 |

具体规则：

- [x] `output` 只给模型，详细错误必须写在这里。
- [x] `display` 只给用户；`None` 表示不展示结果体。
- [x] `display` 不设公共长度限制，由工具自己决定内容与长度。
- [x] bash 初期直接让 `display = Some(output)`，暂不做展示裁剪和字段区分。
- [x] `Interrupted` 承担原先 `stop` 的作用，不额外增加 bool。
- [x] 超时统一映射为 `Failure`，但 bash 的 `output` 中写明 `timed_out`。
- [x] 非零退出统一为 `Failure`，退出码保留在 bash 的 `output` 中。

### Agent 职责

- [x] 解析 LLM 返回的 `arguments` 原始字符串，必须是 JSON object。
- [x] 按工具名从注册表查找工具。
- [x] 未知工具、非法 JSON、非 object 参数由 agent 直接构造 `Failure` 结果。
- [x] 工具参数字段缺失、类型错误、多余字段由工具自己的反序列化校验处理。
- [x] 所有失败都作为该 `tool_call_id` 的 Tool 结果写回，不终止整个回合。
- [x] 所有模型发起的调用都计数，包括非法调用。
- [x] 保证每个 `tool_call_id` 都有对应 Tool 消息。
- [x] `Interrupted` 时补齐剩余调用占位并停止本轮。

### 工具注册与协议

- [x] 引入工具注册表，`Session` 持有注册表。
- [x] provider 的 `build_body` 接收全部 `ToolSpec`，不再硬编码 `tool_spec::bash()`。
- [x] Chat Completions 与 Responses 都从注册表生成 tools 数组。
- [x] `RenderEvent::ToolCall` 和 `RenderEvent::ToolResult` 带工具名。
- [x] UI 根据 `ToolStatus` 决定成功/失败/打断样式，使用 `display` 展示结果体。
- [x] 工具配置由工具实例持有，agent 不再直接向工具执行层传 `BashToolConfig`。

### 动态工具描述与系统提示词（后续）

- [x] 工具名、描述和 schema 由工具自己的 `ToolSpec` 提供，不再维护全局 bash 文案。
- [ ] provider 的 tools 数组与系统提示词来自同一份工具注册表快照，避免描述与实际可用工具不一致。
- [ ] 系统提示词每轮由统一的 prompt builder 动态拼装，不把拼装结果持久化进对话历史。
- [ ] 基础提示词仍来自配置；工具只提供 schema 无法表达的跨工具使用约束，不重复 schema description。
- [ ] skills / MCP 通过独立的 `PromptContributor` 提供提示词片段，不强行塞进 `Tool` trait。
- [ ] 片段按稳定顺序拼接，保证同一注册表快照生成相同提示词。
- [ ] 系统提示词有总长度预算；超限时按优先级省略或截断可省略片段。
- [ ] MCP 返回的描述和提示词片段视为不可信输入，限制长度，避免提示注入。
- [ ] skills 的完整内容按需加载，系统提示词只放索引和触发说明。
- [ ] `/clear` 只清对话历史；系统提示词在每次请求前按当前注册表重新生成。
- [ ] 如果采用动态提示词，需要同步修改 `Msg::System` 的播种方式和 `/clear` 的文档说明。

### 参数校验

- [x] 每个 Args 结构体使用 `#[serde(deny_unknown_fields)]`。
- [x] 每个工具的 JSON Schema 写明 `additionalProperties: false`。
- [x] Schema 负责提示模型，serde 负责服务端实际校验。
- [x] 测试覆盖：非法 JSON、非 object、未知工具、缺字段、类型错误、多余字段、语义校验失败。

## bash 工具需求

### 参数

- [x] `command`：必选，string，要执行的完整 bash 命令。
- [x] `timeout`：可选，integer，单位秒。
  - 默认 `120` 秒。
  - 最大 `300` 秒。
- [x] `cwd`：可选，string，命令启动时的工作目录。

### 工作目录

- [x] `cwd` 省略时继承 rho 进程启动目录。
- [x] 允许绝对路径和相对路径。
- [x] 相对路径按 rho 进程启动目录解析。
- [x] 不展开开头的 `~`。
- [x] 空字符串按参数错误处理，不启动 bash。
- [x] 执行前检查目录存在且确实是目录。
- [x] 目录无效时返回 `Failure`，不启动 bash，`output` 写明无法进入该目录。
- [x] `cwd` 只决定命令的初始目录；命令内部仍可自行 `cd` 到其他位置。
- [x] 不限制目录范围，仍以运行 rho 的用户权限为边界。

待定：

- [x] 不做 canonicalize，保留用户传入形式，仅做 `is_dir` 校验。
- [x] 校验通过后到 spawn 之间目录被删除，按 spawn 失败处理。

超时待定：

- [x] `timeout > 300` 报参数错误。
- [x] 非整数、`0`、负数的错误文案。
- [x] 删除全局 `[tools.bash]` 配置；超时由每次调用参数决定，输出上限固定为末尾 50,000 字符。

### 合并 stdout/stderr

已确认使用一个 pipe，不区分 stdout 和 stderr：

- [x] 子进程的 stdout 和 stderr 指向同一个 pipe，只保留一个读取线程。
- [x] 输出顺序是数据实际写入 pipe 的到达顺序。
- [x] 不再输出 `[stderr]` 段标记。
- [x] 注意：这只能保证内核写入顺序。stdout 在管道下通常是块缓冲，stderr 通常无缓冲，因此逻辑上的先后顺序不一定与 pipe 到达顺序一致。
- [x] 超时/取消后保留 pipe 中已经写入的数据，继续有界读取直到 EOF 或收尾期限。

### 输出格式

`output` 使用固定文本头，`display` 初期与 `output` 相同。

```text
[status] completed
[exit_code] 0
[truncated] false
[output]
<命令输出>
```

规则：

- [x] `[status]`：`completed` / `timed_out` / `interrupted` / `failed`。
- [x] `[exit_code]`：正常退出时为实际退出码；无法取得时用占位符 `-`。
- [x] `[truncated]`：`true` / `false`。
- [x] `[output]` 之后为保留的输出内容。
- [x] 发生截断时增加 `[omitted_chars]`，记录丢弃的字符数。
- [x] 输出上限为末尾 50,000 个字符，不是字节，也不是 50 KiB。
- [x] 截断按 Unicode 字符边界处理，不产生半个字符。
- [x] `display = Some(output.clone())`，暂不单独设计展示格式。
- [x] `interrupted` / `timed_out` / `failed` 无法取得退出码时用 `-`；正常退出保留实际值。
- [x] spawn / waitpid 失败统一 `[status] failed`，`[exit_code] -`，原因写在 `[output]`。

### 超时行为

- [x] 超时后终止整个进程组，沿用现有实现：`SIGTERM` → 等待 2 秒 → `SIGKILL`。
- [x] 主线程继续作为唯一 reaper。
- [x] 回收后读取 pipe 中已有数据，返回“目前已有的输出”。
- [x] `output` 头部写 `[status] timed_out`，`ToolStatus` 为 `Failure`。
- [x] 已写入 pipe 的数据可以返回；进程用户态缓冲中尚未写入 pipe 的数据会丢失。
- [x] 超时后继续执行剩余工具调用，不终止回合。
- [x] 后台进程持续占用 pipe 时保持有界收尾，不能无限阻塞工具返回。

### 截断实现注意

- [x] 只保留末尾 50,000 个字符，不保留头部。
- [x] 实现必须有界内存，不能先缓存全部输出再截断。
- [x] 流式读取时处理 UTF-8 跨 chunk 边界。
- [x] 非法 UTF-8 的计数规则需要明确（建议按 lossy 后的替换字符计数）。
- [x] 记录被丢弃的字符总数，用于 `[omitted_chars]`。

## read 工具（设计中）

### 参数

- [x] `path`：必选，string，允许绝对路径和相对 rho 启动目录的路径。
- [x] `offset`：可选，integer，0-based 行偏移，默认 `0`；`0` 表示第一行。
- [x] `limit`：可选，integer，最多返回行数，默认 `2000`，最大 `2000`。
- [x] `limit > 2000` 按参数错误处理。
- [x] `offset < 0`、`limit <= 0` 按参数错误处理。

### 读取语义

- [x] 路径必须是普通文件；不存在、目录、权限错误都返回 `Failure`。
- [x] 文本检测覆盖整个文件：全文必须是有效 UTF-8 且不含 NUL 字节；任意位置不满足都返回 `Failure`。
- [x] 全文扫描用于文本检测；截断只作用于本次返回范围。
- [x] 按行读取，`offset` 是 0-based 行索引，输出行号使用 1-based 真实行号。
- [x] 同时受两个上限约束，先到者生效：
  - 文件正文最多 50,000 个字符，不含行号和头部。
  - 最多 2,000 行。
- [x] 单行超过 50,000 个字符时返回 `Failure`，错误信息建议改用 bash 的 `sed` / `cut` / `fold` 等命令处理。
- [x] 行号使用文件中的真实行号。
- [x] 保留空行和行内空白。
- [x] 统一处理 `\n` 与 `\r\n` 行尾。
- [x] 读取失败时不返回部分文件内容。
- [x] 非空文件 `offset` 超出末尾返回 `Failure`；空文件 `offset = 0` 返回成功空结果。
- [x] 空文件输出 `[lines] 0-0`，`[truncated] false`，正文为空。

### 输出格式（建议）

```text
[status] success
[path] /abs/path
[offset] 0
[lines] 1-200
[truncated] false
[output]
     1 | 第一行内容
     2 | 第二行内容
```

- [x] 行号右对齐，使用 `行号 | 内容`。
- [x] `[lines]` 使用 1-based 行号；`[offset]` 和 `[next_offset]` 使用 0-based，与参数一致。
- [x] 超过上限时 `[truncated] true`，并增加 `[next_offset]` 指向下一段起始偏移。
- [x] 50k 仅限制文件正文，不含行号和头部。
- [x] 单行自身超过 50k 时返回 `Failure`，不建议截断该行。
- [x] `offset` 超出文件末尾时返回 `Failure`（空文件 `offset = 0` 除外）。
- [x] 空文件输出 `[lines] 0-0`，`[truncated] false`，正文为空。

### 展示

- [x] 成功时 `display = None`，用户不需要看文件正文。
- [x] 失败时 `display = Some(简短错误)`，让用户看到失败原因。

## write 工具（设计中）

### 参数

- [x] `path`：必选，string，允许绝对路径和相对 rho 启动目录的路径。
- [x] `content`：必选，string，UTF-8 文本内容。

### 写入语义

- [x] 路径必须不存在；已存在的普通文件、目录、符号链接或其他目录项一律返回 `Failure`。
- [x] 父目录不存在时由 write 工具自动 `create_dir_all`，不需要模型先调用 bash。
- [x] 使用 `create_new` 创建文件，避免检查后到创建之间的竞态导致覆盖。
- [x] 内容按传入字节原样写入，不自动追加换行，不转换行尾。
- [x] 拒绝包含 NUL 字节的 `content`；校验在任何磁盘副作用之前完成。
- [x] `content` 不设长度上限。
- [x] 直接写入，不使用临时文件 + rename。
- [x] 写入中途失败可能留下空文件或部分文件，不自动回滚；已创建的空目录也不删除。
- [x] 新文件按 umask 创建。
- [x] 写入失败返回 `Failure`，错误信息包含路径和原因。

### 输出格式

```text
[status] success
[path] /abs/path
[action] created
[bytes] 1234
[output]
已写入 /abs/path（新建，1234 字节）
```

- [x] `[action]` 固定为 `created`。
- [x] `display = Some("已写入 /abs/path（新建，N 字节）")`。
- [x] 失败时 `display = Some(简短错误)`。

其余边界：

- [x] 父路径存在但不是目录时，`create_dir_all` 失败并返回 `Failure`。
- [x] 路径已存在的判断使用 `symlink_metadata`，损坏的符号链接也算已存在。
- [x] 目录创建和文件创建/写入失败都返回 `Failure`。

## edit 工具（设计中）

### 参数

- [x] `path`：必选，string，允许绝对路径和相对 rho 启动目录的路径。
- [x] `edits`：必选，array，至少 1 项。
  - [x] `old_text`：必选，string，非空。
  - [x] `new_text`：必选，string，可以为空（删除）。
  - [x] `replace_all`：可选，bool，默认 `false`。

### 文件读取与换行

- [x] 文件必须已存在且是普通文件；不存在、目录、权限错误都返回 `Failure`。
- [x] 跟随符号链接到普通文件；链接目标无效或不是普通文件时报错。
- [x] 文件大小上限 `10 MiB`（`10 * 1024 * 1024` 字节）；超限直接返回 `Failure`，不读入内存。
- [x] 文本检测与 read 一致：全文必须是有效 UTF-8 且不含 NUL。
- [x] 允许绝对路径和相对 rho 启动目录的路径；`~` 不展开。
- [x] 先整体读入，再检测换行类型：统计 `\r\n` 与独立 `\n` 数量，CRLF 较多则判定为 CRLF，否则判定为 LF；没有换行时按 LF 处理。
- [x] 混合换行文件统一按多数类型处理，写回时变成统一换行。
- [x] 内存中把所有 `\r\n` 转为 `\n` 后再匹配和编辑。
- [x] `old_text` / `new_text` 中的 `\r\n` 也先转为 `\n`，避免模型提供 CRLF 时匹配失败。
- [x] 写回时若原文件判定为 CRLF，把所有 `\n` 转为 `\r\n`；LF 文件原样写回。
- [x] 保留文件开头的 BOM 作为内容，不做特殊处理。

### 匹配语义

- [x] 精确字符串匹配，不使用正则。
- [x] `replace_all = false` 时 `old_text` 必须恰好出现一次。
- [x] 出现 0 次：返回匹配失败。
- [x] 出现多次：返回失败，报告匹配行号，并建议增加上下文或设置 `replace_all = true`。
- [x] 匹配行号最多报告前 10 个，同时给出总匹配数。
- [x] `replace_all = true` 时替换全部出现；出现 0 次同样失败。
- [x] 多个 edit 按数组顺序依次应用；后一个 edit 看到前一个 edit 的结果。
- [x] `old_text` 为空字符串时按参数错误处理。
- [x] `old_text == new_text` 时按无操作处理，但仍要求匹配成功。

### 写入语义

- [x] 每个 edit 成功应用后立即按检测到的换行类型写回磁盘。
- [x] 后续 edit 失败时，之前已经成功写回的 edit 保留，不回退。
- [x] 写入采用直接写，文件写入失败可能留下部分内容。
- [x] 写入失败后不再尝试后续 edit。
- [x] `edits` 为空数组时按参数错误处理。

### 失败提示

失败结果需要包含：

- [x] 第几个 edit 失败（1-based）和总 edit 数。
- [x] 失败原因：未找到、匹配多次、写入失败等。
- [x] 已成功应用的 edit 数量。
- [x] 未找到时，在当前文件内容中寻找最相似文本的行号（1-based）。
- [x] 相似度算法：取 `old_text` 中最长的非空行，trim 后作为探针；文件逐行 trim 后比较。
- [x] 相似度用最长公共前缀 + 最长公共后缀计算，避免大文件失败路径上跑全量 Levenshtein。
- [x] 相似度阈值为 `0.6`；低于阈值时不报告最相似行。
- [x] 达到阈值时返回行号和该行内容片段；片段最多 200 个字符，超出加省略号。
- [x] 相似行只针对当前 edit 失败前、已经应用前序 edit 的内存内容。

### 输出格式（建议）

成功：

```text
[status] success
[path] /abs/path
[edits] 2
[replacements] 3
[bytes] 1234
[output]
已编辑 /abs/path（2 个 edit，3 处替换，1234 字节）
```

失败：

```text
[status] failed
[path] /abs/path
[edits_applied] 1
[edits_total] 2
[error] 第 2 个 edit 未找到 old_text
[closest_line] 42
[closest_text]     fn foo() {
[output]
已应用 1/2 个 edit；第 2 个失败：未找到 old_text
```

- [x] `replacements` 统计实际替换次数（`replace_all` 可能大于 1）。
- [x] `display`：成功为一行摘要；失败为“已应用 N/M + 错误 + 可选的最相似行号和片段”。
- [x] 相似度低于阈值时，`output` 和 `display` 都不包含 `closest_line` / `closest_text`。

## 实现与同步修改结果

- [x] 新增通用工具层：`Tool` / `ToolSpec` / `ToolResult` / `ToolStatus` / `ToolContext` / 注册表 / 参数解析错误辅助。
- [x] 拆分 bash 实现：schema、Args、配置、执行、结果格式化归 bash 模块。
- [x] 删除对 `tool_spec::bash()`、`ToolRunner`、`BashToolConfig` 的全局硬编码。
- [x] `src/chat.rs`：只保留通用调度、错误结果、配对、计数和占位逻辑。
- [x] `src/tools.rs`：单 pipe、末尾字符截断、超时终止、`cwd` 设置与校验、固定文本输出。
- [x] 新增 read 工具：全文文本检测、行偏移、行号、正文 50k 字符与 2000 行上限、单行超长错误。
- [x] 新增 write 工具：仅新建、自动创建父目录、拒绝 NUL、直接写入、created 输出。
- [x] 新增 edit 工具：顺序 edits、精确匹配、replace_all、失败相似行、成功后立即写回、不回退。
- [x] `src/provider/openai.rs` 与 `src/provider/openai_responses.rs`：从注册表映射工具定义。
- [x] `src/render.rs`：工具名、display、状态样式。
- [x] `src/config/default.toml`：system prompt、bash 配置。
- [x] `src/config/mod.rs` / `src/config/validate.rs`：配置字段与校验。
- [x] 测试：schema、参数校验（含 `cwd` 相对/绝对路径）、超时上限、单 pipe 顺序、末尾字符截断、固定文本、三态结果、协议序列化、占位配对。
- [x] read 测试：缺失/目录/二进制文件、全文 UTF-8/NUL 检测、offset/limit、行号、50k 正文字符与 2000 行截断、单行超长、next_offset。
- [x] write 测试：已存在路径、父目录自动创建、目录/NUL 拒绝、空内容、相对路径、直接写入失败后的残留行为。
- [x] edit 测试：缺失/二进制文件、文件大小上限、符号链接、唯一匹配、多次匹配、replace_all、顺序应用、中途失败保留前序结果、CRLF 检测与写回、相似行阈值与片段、old_text 为空。
- [ ] 如果实现动态提示词：测试稳定顺序、长度预算、skills/MCP 片段拼接与不可信内容处理。
- [x] 文档：`README.md`、`README.zh-CN.md`、`docs/architecture.md`、`AGENTS.md`。
