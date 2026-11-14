# 供应商协议对照

rho 通过 `protocol` 声明协议族：`openai_chat`（Chat Completions，`/chat/completions`）与
`openai_responses`（Responses，`/responses`）。请求体按 OpenAI 的标准形状组装，与标准不一致的
上游差异由两处表达：

- `[providers.<id>.compat]`：非标准的线上字段差异，逐项开关（见下文）。
- `[models."<provider>/<model>".thinking]`：思考档位片段，请求时顶层合并进 body，形状由供应商决定。

内置配置面向 DeepSeek，两个文件同时是 DeepSeek 的写法样例：`src/config/default.toml`（内置源）
与 `rho --init` 写出的配置文件内容一致。

## 上游文档

| 内容 | 地址 |
|---|---|
| Chat Completions 接口（请求/响应全字段） | https://api-docs.deepseek.com/zh-cn/api/create-chat-completion |
| 思考模式（`thinking`、`reasoning_effort`、`reasoning_content` 的回传规则） | https://api-docs.deepseek.com/zh-cn/guides/thinking_mode |
| 多轮对话拼接 | https://api-docs.deepseek.com/zh-cn/guides/multi_round_chat |
| 上下文硬盘缓存（缓存命中字段） | https://api-docs.deepseek.com/zh-cn/guides/kv_cache |
| Responses API 兼容 | https://api-docs.deepseek.com/zh-cn/guides/responses_api |

## Chat Completions：rho 发出的字段

| 字段 | 取值 |
|---|---|
| `model` | `Model.api_model`，未设置时取模型 id 中第一个 `/` 之后的部分 |
| `messages` | 历史原样映射：`system` / `user` / `assistant` / `tool`；assistant 的 `content` 在纯工具调用时为 `null`，`tool_calls` 用 `id` + `type: "function"` |
| `stream` | 恒为 `true` |
| `tools` | 内置四个工具的 `ToolSpec` 映射为 `function` 数组，每个请求都带 |
| `max_completion_tokens` | `Model.max_output`；`compat.legacy_max_tokens = true` 时改用 `max_tokens` |
| `stream_options.include_usage` | `compat.stream_usage = true`（默认）时发送 |
| thinking 片段 | 当前档位对应的 JSON 对象浅合并到顶层（如 `thinking` / `reasoning_effort`） |
| `reasoning_content`（assistant 消息内） | `compat.echo_reasoning = true` 且该轮有思考内容时写入 |

## 与 OpenAI 标准的差异

| 项目 | OpenAI 标准 | DeepSeek | rho 的处理 |
|---|---|---|---|
| 输出上限字段 | `max_completion_tokens` | `max_tokens`（范围 1–384K；不设置时非思考模式默认 8K、思考模式默认 64K） | 默认发 `max_completion_tokens`；`compat.legacy_max_tokens = true` 发 `max_tokens`，内置 DeepSeek 打开该开关 |
| 思考开关 | `reasoning_effort` 控制强度 | 顶层 `thinking: { type: "enabled" \| "disabled" }`，`reasoning_effort` 控制强度；`thinking.type` 优先于 `reasoning_effort` | 两者都写成 thinking 档位片段，由配置声明 |
| `reasoning_effort` 取值 | `minimal` / `low` / `medium` / `high`（随模型不同） | `minimal` / `low` / `medium` / `high` / `xhigh` / `max` / `ultra`，映射到 low / high / max 三档；默认 `high` | 内置档位提供 `low` / `high` / `max`，档位名可自行增删 |
| 思维链返回 | 推理模型不返回思维链文本，只在 usage 里给 `completion_tokens_details.reasoning_tokens` | 返回 `reasoning_content`，与 `content` 同级 | 流式累积并渲染；Chat Completions 同时接受 `reasoning_content` 与 `reasoning` 两个字段名 |
| 思维链回传 | 无该字段 | 请求带 `tools` 时，历史轮次的 `reasoning_content` 随请求回传并被拼进上下文 | `compat.echo_reasoning = true` 时序列化进 assistant 消息，内置 DeepSeek 打开 |
| 缓存命中统计 | `usage.prompt_tokens_details.cached_tokens` | `usage.prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` | 先读 `prompt_tokens_details.cached_tokens`，回退 `prompt_cache_hit_tokens`；状态栏的命中率取 `命中 / 输入` |
| 流式用量 | `stream_options.include_usage`，usage 在 `choices` 为空的末尾 chunk | 同标准字段；打开后每个块都带 `usage` 字段，除最后一块外均为 `null` | 解析前先判 `choices`，逐块读取非空 usage |
| 采样参数 | `temperature` / `top_p` / `presence_penalty` / `frequency_penalty` 生效 | 思考模式下 `temperature`、`presence_penalty`、`frequency_penalty` 接受但不生效；`top_p` 在 `deepseek-flash` 只在思考模式下生效（取值 0.95–1.0），在 `deepseek-v4-pro` 的思考模式下取固定值（启用工具调用时 0.95） | 请求体不包含这些字段，实际行为由供应商默认值决定 |
| 结束标记 | `data: [DONE]` | 同标准 | `[DONE]` 作为流结束；`finish_reason` 的提交资格在 worker 内判定 |

## compat 开关

声明在供应商下，语义是"该供应商与 OpenAI 标准的差异"。缺省整段即 OpenAI 标准行为；
外部配置完全替换内置配置，不合并。

```toml
[providers.deepseek.compat]
legacy_max_tokens = true   # 发 max_tokens 而不是 max_completion_tokens
echo_reasoning    = true   # 带 tools 的请求回传 reasoning_content
stream_usage      = true   # 发 stream_options.include_usage（默认即 true）
```

| 开关 | 默认 | 打开后的效果 | 生效位置 |
|---|---|---|---|
| `legacy_max_tokens` | `false` | 输出上限用 `max_tokens` | `provider::openai` 的 `build_body` |
| `echo_reasoning` | `false` | assistant 消息带 `reasoning_content` | 同上（消息序列化） |
| `stream_usage` | `true` | 请求带 `stream_options.include_usage`，状态栏才有用量与命中率 | 同上（请求体尾部） |

三项开关只作用于 `openai_chat`：`openai_responses` 的适配器用 `max_output_tokens` 表达输出上限，用量来自 `response.completed` 的 `usage`。

## 思考档位片段

内置 DeepSeek 配置的四个档位（`off` / `low` / `high` / `max`）对应 `thinking.type` 开关与
`reasoning_effort` 强度：

```toml
[models."deepseek/deepseek-v4-pro".thinking]
off  = { thinking = { type = "disabled" } }
low  = { thinking = { type = "enabled" }, reasoning_effort = "low" }
high = { thinking = { type = "enabled" }, reasoning_effort = "high" }
max  = { thinking = { type = "enabled" }, reasoning_effort = "max" }
```

片段与协议强相关：上面的形状是 Chat Completions 的写法，换成 `openai_responses` 时要按该协议的
参数形状重写档位表；给 OpenAI 推理模型使用时只保留 `reasoning_effort`（`thinking` 是 DeepSeek
的私有字段，OpenAI 会按未知参数拒绝）。

## 状态栏用量的来源

用量只出现在流式响应的 `usage` 字段里；`compat.stream_usage` 打开时状态栏显示用量与命中率段。

- Chat Completions：`stream_options.include_usage` 打开后，最后一个 `choices: []` 的 chunk 带
  `usage`；状态栏取该请求的 `total_tokens` 与 `context_window` 的比值，命中率取
  `prompt_cache_hit_tokens（或 cached_tokens）/ prompt_tokens`。
- Responses：`response.completed` 的 `response.usage` 提供 `input_tokens` / `output_tokens` /
  `total_tokens`，缓存命中在 `input_tokens_details.cached_tokens`。

## Responses 协议

`openai_responses` 把历史映射为 `input` items：用户/助手文本为 message item，工具调用为
`function_call`（`call_id` 配对 `function_call_output`），系统提示写入顶层 `instructions`，
输出上限用 `max_output_tokens`。流式事件以 `response.completed` / `response.incomplete` /
`response.failed` 结束，不发送 `data: [DONE]`；rho 在适配器里按事件类型判定终态，用量取
`response.completed` 里的 `usage`。
