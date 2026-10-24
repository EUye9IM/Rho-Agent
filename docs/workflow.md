# 开发、验证与发布流程

分支模型：

```text
feature branch ──PR──► dev ──PR──► master ──tag vX.Y.Z──► GitHub Release
```

原则：

- **愿望进 issue，范围进 features，行为进 design，任务进 PR。**
- `dev` 是集成分支；`master` 是稳定分支；发布只从 `master` 打 tag。
- 不把未排期的想法直接写进 `docs/features.md`。

## 1. 愿望：开 issue

在 GitHub 选择 **New issue → Feature request / 功能建议**。模板默认标题前缀 `[Feature] `，并自动加 `enhancement` 标签。模板会要求：

- 用户问题：现在哪里不方便，谁在什么场景下遇到
- 期望行为：用户可见结果，不先规定实现
- 验收条件：怎样算完成，尽量可勾选
- 边界与非目标：这次明确不做什么
- 影响范围、版本意向、设计/依赖/安全影响

写法建议：

- 标题写结果，不写实现：`[Feature] 启动时报告配置错误的行号`
- 一个 issue 只解决一个问题；相关但独立的想法拆开
- 先写“为什么”，再写“怎么做”
- 没有验收条件的 issue，不进入版本

## 2. Triage：决定是否排期

定期处理新 issue，只做三选一：

| 结论 | 动作 |
|---|---|
| 不做 | 说明原因后关闭；重要范围变化同步到 `features.md` 的“明确不做” |
| 候选 | 保持 open，不打 milestone；留在 issue 里继续讨论 |
| 接受 | 挂 `vX.Y.Z` milestone，更新 `features.md` 为 📋 已排期 |

只有在**接受并挂 milestone** 后，功能才进入 `docs/features.md`。候选 issue 不往清单里搬。

## 3. 设计先行

开始写代码前确认：

- 用户可见行为或配置语义变化 → 先改 `docs/design.md`
- 模块、线程、错误处理或测试策略变化 → 改 `docs/architecture.md`
- 产品范围变化 → 改 `docs/features.md`
- 需要新依赖、第二个工具、体积明显增加 → 在 issue 里明确确认

设计结论写回 issue，避免只存在于聊天记录里。

## 4. 开发：feature branch → dev

从最新 `dev` 开分支：

```sh
git switch dev
git pull --ff-only
git switch -c feat/123-config-error-line
```

开发时：

- 提交信息用英文祈使句，保持小而聚焦
- 行为改动补对应层测试
- 需要终端行为的改动，用 PTY 手工验收
- PR 目标分支是 `dev`，不是 `master`
- PR 描述里写 `Closes #123`，并贴验收条件和测试命令

PR 会触发 CI（测试 + clippy）；`build` job 只在 `master` 和 `v*` tag 上跑。

## 5. 验证

本地至少跑：

```sh
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
```

涉及 MSRV 时：

```sh
cargo +1.75.0 test --all-targets --locked
```

手工验收：

- 终端交互、Ctrl-C、确认提示：用真实 TTY 或 `script -qec` 跑
- 真实 DeepSeek 请求：手工验收，CI 不调用外部 API
- 发布构建相关：按 AGENTS.md 的 musl 基线命令构建，并用 `qemu-x86_64 -cpu qemu64` 做低指令集检查
- 目标机验证仅在明确需要时进行；不要默认 scp 或替换系统二进制

验收通过后合并 PR 到 `dev`。此时功能状态从 🚧 变为 🧪 待发布（如果已合并但还没发版）。

## 6. 发布：dev → master → tag

发布前：

1. 确认 milestone 里的 issue 都已完成，或明确移到下一个版本
2. 更新 `Cargo.toml` 版本，并让 `Cargo.lock` 同步
3. 更新 `docs/features.md` 的版本范围和状态
4. 需要时更新 README、design、architecture
5. 从 `dev` 向 `master` 开 PR，CI 通过后合并

然后打 annotated tag：

```sh
git switch master
git pull --ff-only
git tag -a v0.2.0 -m "rho v0.2.0"
git push origin v0.2.0
```

tag 会触发 CI 构建 musl 产物并创建 GitHub Release。

发布后验证：

- CI run 成功
- Release 资产存在：`rho-x86_64-unknown-linux-musl.tar.gz` + `.sha256`
- 下载资产校验 sha256，确认是 static-pie
- 必要时用 `qemu-x86_64 -cpu qemu64` 跑一次 HTTPS 路径
- 更新 `docs/features.md`：🧪 → ✅，写清版本号
- 关闭 milestone 和对应 issue
- 为下一个版本建立空 milestone（需要时）

## 7. 修 bug / hotfix

普通 bug：

- 从 `dev` 开 `fix/...`，PR 回 `dev`，随下一个版本发布

已发布版本的紧急问题：

- 从 `master` 开 `hotfix/...`，PR 到 `master`
- 合并后打 patch tag（例如 `v0.1.1`）
- 再把 `master` 合回 `dev`，避免修复只在稳定分支

不要移动已经发布的 tag；需要修正就发新 patch 版本。

## 状态流转

| 阶段 | `features.md` 状态 | GitHub |
|---|---|---|
| 只是愿望 | 不出现 | open issue，无 milestone |
| 接受排期 | 📋 已排期 | issue + milestone |
| 开发中 | 🚧 开发中 | feature branch / PR 到 dev |
| 已合并待发布 | 🧪 待发布 | 已合入 dev，等待 master/tag |
| 已发布 | ✅ 已发布 | release + 关闭 issue/milestone |
| 决定不做 | ⛔ 不做 | 关闭 issue 并说明原因 |
