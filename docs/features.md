# 功能清单

本文件回答三个问题：**现在有什么、接下来可能做什么、明确不做什么**。

它是产品范围索引，不是任务看板：

- **愿望先写 issue，不直接写进本文件。** 只有经过 triage、确认进入版本的功能才出现在下面。
- 行为细节以 [design.md](design.md) 和 [README](../README.md) 为准。
- 具体开发任务用 GitHub Issue 跟踪。
- 版本范围用 milestone 跟踪。
- 不要在本文件里维护“完成百分比”或没有承诺的日期。
- 完整流程见 [workflow.md](workflow.md)。

## 状态定义

| 状态 | 含义 |
|---|---|
| 💡 候选 | 讨论中，尚未承诺版本；只在 issue 里 |
| 📋 已排期 | 已确认进入某个 milestone，尚未开始或未开 PR |
| 🚧 开发中 | 已有 feature branch / PR |
| 🧪 待发布 | 已合并到 `dev` 并通过验收，但还没进入 release |
| ✅ 已发布 | 已进入某个 release，用户文档和测试齐全 |
| ⛔ 不做 | 明确排除，除非重新讨论设计 |

## 已发布

### v0.1.0

| 功能 | 说明 | 细节 |
|---|---|---|
| 单二进制 REPL | 单 crate、单二进制，Linux musl 静态构建；阻塞 I/O + 线程，无 async runtime | [architecture.md](architecture.md) |
| 流式对话 | 按 SSE 增量输出正文；正文不做 markdown 渲染 | [design.md](design.md) 十节 |
| 配置驱动供应商 | provider / model / thinking 全部由 TOML 声明；v1 只实现 OpenAI Chat Completions 协议族，无硬编码供应商分支 | [design.md](design.md) 五、六节 |
| 内置 DeepSeek | 内置 `deepseek/deepseek-flash` 与 `deepseek/deepseek-v4-pro`，含上下文窗口、输出上限和思考档位 | [design.md](design.md) 五节 |
| 思考档位与 reasoning | 档位名和请求片段由配置定义；思考内容暗淡色流式显示；默认不回传，`compat.echo_reasoning` 可开 | [design.md](design.md) 六、十节 |
| REPL 命令 | `/model`、`/provider`、`/thinking`、`/clear`、`/help`、`/exit`，切换只影响后续请求 | [design.md](design.md) 四节 |
| `bash` 工具 | 前台 `bash -c`，每次新 shell；allowlist + 元字符检查 + 人工确认；`--yes` 跳过确认 | [design.md](design.md) 七节 |
| 工具执行限制 | 超时、进程组 SIGTERM → 2s → SIGKILL、stdout/stderr 独立截断、`max_tool_steps` 上限 | [design.md](design.md) 七节 |
| Ctrl-C 取消 | 流式/工具/确认期间第一次取消，第二次退出；已完成的协议消息保留，历史保持合法 | [design.md](design.md) 三节 |
| 严格配置校验 | 未知字段、悬空引用、缺失必填字段硬报错；`${VAR}` header 展开；疑似明文密钥告警；退出码 2 | [design.md](design.md) 五节 |
| HTTP / TLS 兼容 | 读取代理环境变量；`SSL_CERT_FILE` 覆盖内置根证书；HTTP 不重试 | [design.md](design.md) 九节 |
| 可预测退出码 | 0 正常退出，1 REPL 内部错误，2 用法/配置错误；stdin 非 TTY 时拒绝启动 | [design.md](design.md) 四节 |

## 候选与排期

当前没有承诺 `v0.2.0` 的具体功能。

流程：

1. 新想法先开 issue（Feature request 模板），描述**用户问题和场景**，不要直接写进本表。
2. 确认进入某个版本时，再更新本表：状态写 📋，挂上对应 milestone 和 issue。
3. 开了 feature branch / PR 后改成 🚧；合并到 `dev` 并验收后改成 🧪。
4. 功能随版本发布后才改成 ✅，并写清版本号。
5. 取消或降级时保留一行简短记录，不要悄悄删除，便于以后回溯。

功能进入版本前，至少要想清楚：

- 是否仍然符合“轻量、单二进制、配置驱动”的定位
- 是否必须新增依赖或第二个工具
- 对体积、RSS、启动时间的影响
- 用户可见行为是否需要先改 `design.md`

## 明确不做（v1）

- 一次性执行模式、stdin 管道、会话持久化
- 文件读写/编辑工具、图像、MCP、子代理
- 上下文预算管理、摘要压缩、tokenizer
- HTTP 重试、async 运行时
- OpenAI Chat Completions 之外的协议族实现
- Windows / macOS
- `--no-color` / `NO_COLOR` 颜色开关
- `config validate` 子命令

这些不是“以后一定做”的路线图，而是当前明确排除的范围。要改结论，先改 [design.md](design.md) 的“明确不做”一节。

## 与其他文档的分工

| 位置 | 负责什么 |
|---|---|
| GitHub Issues | 愿望、讨论、验收条件 |
| `docs/features.md` | 已接受的产品范围：已排期、开发中、待发布、已发布、不做 |
| GitHub Milestones | 某个版本要装哪些 issue |
| `docs/workflow.md` | 从 issue 到开发、验证、发布的流程 |
| `docs/design.md` | 行为语义与取舍 |
| `docs/architecture.md` | 模块、线程、错误处理和测试策略 |
| `README.md` / `README.zh-CN.md` | 用户安装与使用说明 |

本文件只在**范围变化**或**发布**时更新；普通 bugfix、重构、测试补充不需要动它。
