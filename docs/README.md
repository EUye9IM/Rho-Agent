# 文档索引

`docs/` 存放 rho 的**现行**文档（设计、架构、功能范围、流程），不是历史归档。

| 文档 | 内容 | 维护时机 |
|---|---|---|
| [design.md](design.md) | 定位、运行时、CLI/REPL、配置模型、供应商、工具、取消语义、取舍 | 产品行为、配置语义、用户可见交互变化时 |
| [architecture.md](architecture.md) | 模块布局、关键类型、工具循环、线程/取消、错误与退出码、测试分层 | 模块边界、线程模型、测试策略变化时 |
| [features.md](features.md) | 已排期、开发中、待发布、已发布和明确不做的范围 | 功能进入版本、发布或范围变化时 |
| [workflow.md](workflow.md) | 从 issue 到开发、验证、发布的流程 | 流程变化时 |

根目录文档：

- [README.md](../README.md) / [README.zh-CN.md](../README.zh-CN.md)：用户安装、构建、使用说明
- [AGENTS.md](../AGENTS.md)：在本仓库工作的 agent 约定

维护规则：

- `design.md` 和 `architecture.md` 是现行规范，不是“任务完成报告”；不要因为 M1–M4 已完成就把整篇文档归档。
- 功能范围查 `features.md`；具体任务用 issue，版本范围用 milestone，不要把所有愿望堆进设计文档。
- 从愿望到发布的完整步骤见 `workflow.md`。
- 行为或接口变化时，同一提交内同步更新设计文档、架构文档和 README。
- 已完成的里程碑与发布记录只作为历史对照保留在文末，不继续往设计文档里追加流水账。
- 如需新增历史/发布记录，另建 `docs/history.md`，不要把 `design.md` 改成时间线。
