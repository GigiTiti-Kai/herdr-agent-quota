# Issue #22：活动模型显示名可观测性调研

> Historical research, valid as of the date below. For current behavior and upgrade instructions, see the [README](../../README.md).

研究日期：2026-08-26（Asia/Shanghai）  
范围：GitHub issue #22、Claude Code statusLine、Google Antigravity/Agy statusLine、Codex app-server v2 `thread/list`，以及 Grok Build 的官方 status-line 合同。  
来源约束：优先使用供应商官方文档和官方源码；Codex 源码固定到 2026-08-26 访问时的 `main` commit `2764e83626efe55f64e04d153fc99a157327f3c2`，Grok Build 固定到 `77cd7eb675ba911c225c3aaeeece3a20cbccc426`。移动中的文档页只代表访问日合同，不代表本地 CLI 永远不会变。

## 结论先行

| Provider | 当前一手本地来源 | 活动模型显示名字段 | 对本插件的结论 |
| --- | --- | --- | --- |
| Claude Code | `statusLine` 命令的 stdin JSON | `model.display_name`（同时有 `model.id`） | `confirmed`：直接解析显示名；缺失时隐藏，不从 id 猜人类名称。 [官方字段表](https://code.claude.com/docs/en/statusline#available-data) |
| Agy / Antigravity CLI | `statusLine` 命令的 stdin JSON | 顶层 `model.display_name`（同时有 `model.id`） | `confirmed`：与 Claude 使用同一层级的可读字段；按 `session_id`/`conversation_id` 保存。 [官方字段表](https://antigravity.google/docs/cli/statusline/#available-json-fields) |
| Codex app-server v2 | `thread/list` 返回的 `Thread` 摘要 | 只有 `modelProvider`，没有 thread-level `model` | `unsupported`（对当前 `thread/list` 调用）：不能把 provider 当模型。`thread/start`/`turn/start` 的 `model` 是请求覆盖，不会令 `thread/list` 返回活动模型。 [官方 Thread 定义](https://github.com/openai/codex/blob/2764e83626efe55f64e04d153fc99a157327f3c2/codex-rs/app-server-protocol/src/protocol/v2/thread_data.rs#L199-L243) |
| Grok Build | 可选的 Grok `status_line` command/builtin | `model.id`、`model.display_name`，无法读取时省略 | `confirmed`（若接入 Grok status-line）；但本插件当前 Grok adapter 只读 billing endpoint，因此现有 quota 快照没有模型字段。 [官方可用字段](https://github.com/xai-org/grok-build/blob/77cd7eb675ba911c225c3aaeeece3a20cbccc426/crates/codegen/xai-grok-pager/docs/user-guide/25-status-line.md#available-data) |

这意味着 issue #22 的 `$quota_model` 可以可靠覆盖 Claude 与 Agy；Grok 需要另有 status-line 输入才能覆盖，Codex 不能从现有独立 `thread/list` 轮询可靠得到活动模型。Issue 本身只把 Claude 的 `model.display_name` 作为已确认来源，并明确要求另外核查 Codex/Grok/Agy。[Issue #22](https://github.com/levi-qiao/herdr-agent-usage/issues/22#issue-3420898044)

> **实现更新（2026-08-26）：** 上表刻意描述“仅凭 `thread/list`/billing”的边界；本插件随后增加了一个不改变该边界的本地补充层：Codex 只按返回的 thread id 读取对应 rollout 尾部的 `turn_context`，Grok 只读取本地 session 的 `signals.json`。因此现在 Codex/Grok 在本地文件提供字段时也会显示模型；context/cache 的字段来源和安全边界见 [`codex-grok-context-cache.md`](codex-grok-context-cache.md)。

默认侧栏把 provider 和模型合并为一个 `$quota_provider_model`（例如
`Claude/Sonnet`）；底层 `$quota_provider`/`$quota_model` 仍保留给自定义布局兼容使用。

## Issue #22 要解决什么

Issue 描述的是同一 provider 打开多个 pane 时，只显示 `Claude`/`Codex`/`Grok`/`Agy` 不足以区分 Sonnet、Opus 等实际模型；请求是增加 `$quota_model`，显示人类可读 display name，而不是完整模型 id。[Issue #22](https://github.com/levi-qiao/herdr-agent-usage/issues/22)

Issue 作者已确认 Claude Code 的官方 statusLine stdin JSON 包含 `model` 对象，但没有伪造 Codex/Grok/Agy 的样例；因此以下把“官方字段存在”和“本插件当前采集链路能拿到”分开记录。[Issue #22 的来源说明](https://github.com/levi-qiao/herdr-agent-usage/issues/22#issue-3420898044)

## 1. Claude Code：官方 statusLine 直接提供 display name

### 观察到的事实

- Claude Code 运行用户配置的 `statusLine` command，把 JSON session data 写到脚本 stdin；官方 available-data 表把 `model.id`、`model.display_name` 定义为当前模型的 identifier 和 display name。[Claude Code statusLine：Available data](https://code.claude.com/docs/en/statusline#available-data)
- 官方完整 JSON 示例同时展示 `"model": { "id": "claude-opus-5", "display_name": "Opus" }`；官方示例脚本也是读取 `.model.display_name`，不是从 id 做字符串转换。[Claude Code statusLine 完整 schema](https://code.claude.com/docs/en/statusline#full-json-schema)
- statusLine 在本地运行，不消耗 API token；它按 session/assistant 等事件驱动更新，适合在已有 hook 中读取而不是另起一个模型请求。[Claude Code statusLine 工作方式](https://code.claude.com/docs/en/statusline#how-status-lines-work)

### 对实现的含义

1. 解析顺序应是 `model.display_name`；`model.id` 只作为原始诊断字段（若未来确实需要），不应把 `claude-sonnet-*` 等 id 猜成 UI 名称。[字段定义](https://code.claude.com/docs/en/statusline#available-data)
2. `$quota_model` 应是可选值。首个 payload 没有模型、字段为 `null` 或解析失败时，不写入空字符串覆盖已有值；同一 `session_id` 的上次有效 display name 可以保留，避免 sidebar 闪烁和 metadata repaint。[完整 schema 的缺失/null 说明](https://code.claude.com/docs/en/statusline#full-json-schema)
3. 模型属于 session/pane 维度，不属于 provider 全局维度；状态缓存要按 `session_id`（必要时结合 transcript/session 边界）保存，不能让一个 Claude pane 的模型覆盖另一个 pane。[Claude statusLine 的 `session_id` 与 `model` 字段](https://code.claude.com/docs/en/statusline#full-json-schema)

## 2. Agy / Antigravity CLI：同样有顶层 `model` 对象

### 观察到的事实

- Google Antigravity 官方 status-line customization 文档说明：每次 agent state 改变时，TUI 执行配置的命令，把详细状态 JSON 通过 stdin 传给脚本。[Antigravity status-line 配置](https://antigravity.google/docs/cli/statusline/#configuration)
- 官方 available JSON fields 表明确列出顶层 `model` object，内容是 active model 的 `id` 和 `display_name`；它不是 quota bucket 名，也不是 `product` 字段。[Antigravity available JSON fields](https://antigravity.google/docs/cli/statusline/#available-json-fields)
- 官方 sanitized payload 示例同时包含 `"model": { "id": "Gemini 3.5 Flash (High)", "display_name": "Gemini 3.5 Flash (High)" }`，并将 `quota` 另列为独立对象。这证明模型和额度池是两个不同合同。[Antigravity payload example](https://antigravity.google/docs/cli/statusline/#json-payload-example)

### 对实现的含义

1. Agy adapter 可直接复用“只取 `model.display_name`”的 statusLine parser；不应从 `quota` 的 `gemini-*`/`3p-*` key 反推模型名。[字段表](https://antigravity.google/docs/cli/statusline/#available-json-fields)
2. 使用官方的 `session_id`（文档说明它是 `conversation_id` 的兼容别名）或 `conversation_id` 作为保存键；模型缺失时保留该 session 的上一条有效值，新的 session 则保持 unknown/隐藏。[字段表](https://antigravity.google/docs/cli/statusline/#available-json-fields)
3. 该来源是现有 statusLine hook 的本地 stdin，不需要访问 pane、启动 headless prompt 或重新登录；这样不会把“显示模型”变成额外模型调用。[Antigravity status-line 配置与 stdin 语义](https://antigravity.google/docs/cli/statusline/#configuration)

## 3. Codex app-server：`thread/list` 只有 provider，不提供活动模型

### 观察到的事实

- 截至上述 pinned commit，官方 v2 `Thread` 结构在 `model_provider`（JSON 为 `modelProvider`）上给出的语义是“Model provider used for this thread”；同一个结构从 `id`、`modelProvider`、时间、preview、status 到 turns 没有 `model` 字段。[Codex `Thread` 源码（pinned commit）](https://github.com/openai/codex/blob/2764e83626efe55f64e04d153fc99a157327f3c2/codex-rs/app-server-protocol/src/protocol/v2/thread_data.rs#L199-L243)
- 生成的官方 JSON schema 也只把 `modelProvider` 列为 `Thread` 的 required property；`thread/list` 的 `data` 项引用该 `Thread` 定义。 [Codex `ThreadListResponse` schema](https://github.com/openai/codex/blob/2764e83626efe55f64e04d153fc99a157327f3c2/codex-rs/app-server-protocol/schema/json/v2/ThreadListResponse.json#L889-L1080)
- 官方 API projection 从 stored thread 构造 `Thread` 时只映射 `model_provider`；即使内部 rollout/state metadata 可能保存 model，`thread/list` 的公开 projection 也会丢弃它。这是“内部有记录”与“API 可读”之间的边界。[Codex `thread_from_stored_thread` projection（pinned commit）](https://github.com/openai/codex/blob/2764e83626efe55f64e04d153fc99a157327f3c2/codex-rs/app-server/src/request_processors/thread_processor.rs#L5844-L5925)
- 官方 `thread/list` 示例返回 `id`、`preview`、`modelProvider`、时间和 `status`，没有 `model`；`status: active` 只表示运行状态，不增加模型名称。[Codex app-server `thread/list` 示例](https://github.com/openai/codex/blob/2764e83626efe55f64e04d153fc99a157327f3c2/codex-rs/app-server/README.md#L434-L481)
- `thread/start` 和 `turn/start` 的请求可以传 `model` 覆盖；这说明模型是 thread/turn 运行配置，但请求参数不等于 `thread/list` 返回字段。[Codex app-server 生命周期与 `turn/start` 覆盖](https://github.com/openai/codex/blob/2764e83626efe55f64e04d153fc99a157327f3c2/codex-rs/app-server/README.md#L75-L83)
- `thread/resume` 默认会使用 thread 中持久化的最新 `model`/`reasoningEffort`，但该内部持久化值仍没有进入 `Thread`/`thread/list` 摘要；调用 resume 也不是无副作用的只读查询。[Codex app-server resume 语义](https://github.com/openai/codex/blob/2764e83626efe55f64e04d153fc99a157327f3c2/codex-rs/app-server/README.md#L370-L384)
- 若客户端本来就持有活动 app-server 连接，官方事件流可告知 turn 级模型变化，例如 `model/rerouted` 带 `fromModel`/`toModel`；`thread/list` 本身不订阅或回放这些运行时事件。[Codex app-server turn/model events](https://github.com/openai/codex/blob/2764e83626efe55f64e04d153fc99a157327f3c2/codex-rs/app-server/README.md#L1678-L1692)

### 对本插件的结论

- 当前 quota fetch 启动的是独立 app-server，只做 `account/read`、`account/rateLimits/read` 和不扫描 rollout 的 `thread/list`；这条请求链可以得到 provider 和 preview，但没有活动 pane 的 model 事件。不要把 `modelProvider: "openai"` 变成 `GPT-5.x`，也不要从 thread id、preview、rate-limit bucket 或本地文件名猜模型。[本仓库 Codex fetch 的 `thread/list` 调用](../../src/providers/codex.rs#L153-L199)
- 若未来要支持 Codex `$quota_model`，需要把插件接入**活动会话已有的 app-server 事件/客户端**，按 `threadId` 跟踪 turn 请求和 `model/rerouted`；不能通过额外 `thread/resume`、模型请求或 rollout 扫描来“查一下”。[官方 turn/model events](https://github.com/openai/codex/blob/2764e83626efe55f64e04d153fc99a157327f3c2/codex-rs/app-server/README.md#L1678-L1692)
- 版本边界必须显式写出：官方说明 `codex app-server generate-ts/json-schema` 的产物只保证匹配**运行该命令的 Codex 版本**。因此实现前应对用户本地 binary 运行 `codex app-server generate-json-schema`（必要时加 `--experimental`），不能把 moving `main` 的字段当作所有版本的稳定合同。[官方 schema 生成说明](https://github.com/openai/codex/blob/2764e83626efe55f64e04d153fc99a157327f3c2/codex-rs/app-server/README.md#L245-L251)

## 4. Grok Build：官方 status-line 有模型，但现有 quota adapter 没有

### 观察到的事实

- xAI 官方 Grok Build status-line 文档的 builtin `model` item 明确显示 Model display name；command 模式把 JSON 通过 stdin 交给用户脚本。[Grok Build status-line 配置](https://github.com/xai-org/grok-build/blob/77cd7eb675ba911c225c3aaeeece3a20cbccc426/crates/codegen/xai-grok-pager/docs/user-guide/25-status-line.md#built-in)
- 官方 available-data 表列出 `model.id`、`model.display_name`，并明确“当 agent 无法读取 session model 时省略该字段”。脚本示例也使用 `.model.display_name // "?"`，不从其他字段推断。[Grok Build available data](https://github.com/xai-org/grok-build/blob/77cd7eb675ba911c225c3aaeeece3a20cbccc426/crates/codegen/xai-grok-pager/docs/user-guide/25-status-line.md#available-data)
- 本仓库的 Grok adapter 当前只调用 `https://cli-chat-proxy.grok.com/v1/billing?format=credits`，解析 `config.creditUsagePercent` 与 `currentPeriod`；没有消费 Grok status-line stdin，因此当前 Grok quota snapshot 没有模型可读来源。[本仓库 Grok adapter](../../src/providers/grok.rs#L10-L43)

### 对实现的含义

1. 如果后续给 Grok 加 statusLine wrapper，应只解析官方 `model.display_name`，并把字段省略当作 unknown；不要从 billing account、credits period、`grok` executable 名或默认模型版本猜显示名。[Grok Build available data](https://github.com/xai-org/grok-build/blob/77cd7eb675ba911c225c3aaeeece3a20cbccc426/crates/codegen/xai-grok-pager/docs/user-guide/25-status-line.md#available-data)
2. Grok command status-line 有 `refresh_interval`，但定时运行拿到的 payload 可能是最近一次 state change 的快照；若要接入，沿用 hook 输入并控制刷新，不要让插件为模型名启动额外网络/模型请求。[Grok Build refresh 语义](https://github.com/xai-org/grok-build/blob/77cd7eb675ba911c225c3aaeeece3a20cbccc426/crates/codegen/xai-grok-pager/docs/user-guide/25-status-line.md#refresh-runs)

## 落地设计依据

1. **统一内部字段但保持可选：** 在现有 snapshot/presentation 层使用 `Option<String>` 的 display label；解析器只接受 provider 官方 `model.display_name`（兼容 Agy/Grok 文档中的 camelCase 变体时要有明确来源），不把 id 变成人类名。[Claude 字段合同](https://code.claude.com/docs/en/statusline#available-data) · [Agy 字段合同](https://antigravity.google/docs/cli/statusline/#available-json-fields) · [Grok 字段合同](https://github.com/xai-org/grok-build/blob/77cd7eb675ba911c225c3aaeeece3a20cbccc426/crates/codegen/xai-grok-pager/docs/user-guide/25-status-line.md#available-data)
2. **按 pane/session 保存：** Claude/Agy/Grok 的 statusLine payload 都带 session/conversation 标识；缓存模型时使用该标识，状态缺失时保留同一 session 的最后有效值，避免每次刷新清空 token。[Claude schema](https://code.claude.com/docs/en/statusline#full-json-schema) · [Agy fields](https://antigravity.google/docs/cli/statusline/#available-json-fields) · [Grok fields](https://github.com/xai-org/grok-build/blob/77cd7eb675ba911c225c3aaeeece3a20cbccc426/crates/codegen/xai-grok-pager/docs/user-guide/25-status-line.md#available-data)
3. **展示策略：** `$quota_model` 只在 display name 已确认时显示；默认布局用
   `$quota_provider_model` 将它与 provider 紧凑合并，但不要为 unknown 输出“默认模型”或
   provider 名的伪模型。Claude/Agy 可由 statusLine hook 提供，Codex/Grok 在本地
   rollout/session 文件有证据时也可提供。[Issue #22 的 display-name 目标](https://github.com/levi-qiao/herdr-agent-usage/issues/22) · [Codex Thread schema](https://github.com/openai/codex/blob/2764e83626efe55f64e04d153fc99a157327f3c2/codex-rs/app-server-protocol/src/protocol/v2/thread_data.rs#L199-L243)
4. **刷新和成本：** 只在已有 statusLine 输入/活动 app-server 事件中更新模型；不要为查模型读取 Herdr pane、resume Codex thread、扫描所有 rollout、发送 warm-up prompt 或触发网络模型请求。这个边界与本仓库“pane 读取有可见 repaint 成本、事件只读指定 pane”的约束一致。[仓库 AGENTS.md](../../AGENTS.md) · [Codex app-server 生命周期](https://github.com/openai/codex/blob/2764e83626efe55f64e04d153fc99a157327f3c2/codex-rs/app-server/README.md#L75-L83)

## Codex / Grok 不猜测边界

- **不把 provider 当模型：** Codex `modelProvider` 只回答“由哪个 provider 服务”，不能回答“当前调用哪个模型”。[Codex `Thread` 定义](https://github.com/openai/codex/blob/2764e83626efe55f64e04d153fc99a157327f3c2/codex-rs/app-server-protocol/src/protocol/v2/thread_data.rs#L199-L243)
- **不把请求配置当实际结果：** Codex `turn/start.model` 是 override；后端还可能发出 `model/rerouted`，所以即使知道请求参数，也不能声称它是该 turn 最终实际模型。[Codex turn/model events](https://github.com/openai/codex/blob/2764e83626efe55f64e04d153fc99a157327f3c2/codex-rs/app-server/README.md#L1678-L1692)
- **不从 quota/billing 反推：** Grok 官方把 model 和 billing/context 作为不同 status-line 数据；当前 billing adapter 不包含 model，因此 credits period、账号 token、默认版本都不能作为显示名。[Grok available data](https://github.com/xai-org/grok-build/blob/77cd7eb675ba911c225c3aaeeece3a20cbccc426/crates/codegen/xai-grok-pager/docs/user-guide/25-status-line.md#available-data) · [本仓库 Grok adapter](../../src/providers/grok.rs#L152-L191)
- **未知就隐藏/保留旧值，不编造：** provider 明确省略字段时显示 unknown（或同一 session 保留上次已确认值）；不要输出 `GPT-5`、`Grok 4`、`Gemini` 等未经 payload 证实的默认名。[Grok 的“字段省略”说明](https://github.com/xai-org/grok-build/blob/77cd7eb675ba911c225c3aaeeece3a20cbccc426/crates/codegen/xai-grok-pager/docs/user-guide/25-status-line.md#available-data) · [Claude 的缺失/null 处理](https://code.claude.com/docs/en/statusline#full-json-schema)

## 验证清单

- Claude/Agy parser fixture：有 `model.display_name`、只有 id、model 为 null、整个 model 缺失；断言只有 display name 会进入 `$quota_model`，并检查多 session 不互相覆盖。[Claude schema](https://code.claude.com/docs/en/statusline#full-json-schema) · [Agy payload example](https://antigravity.google/docs/cli/statusline/#json-payload-example)
- Grok 若接入 statusLine：覆盖有模型、字段省略、`refresh_interval` payload 为旧快照三种情况；不把省略当错误或默认模型。[Grok available data](https://github.com/xai-org/grok-build/blob/77cd7eb675ba911c225c3aaeeece3a20cbccc426/crates/codegen/xai-grok-pager/docs/user-guide/25-status-line.md#available-data)
- Codex contract fixture：`thread/list` 项只有 `modelProvider` 时 `$quota_model` 必须为空；不要为了取得 model 调 `thread/resume`。本地版本验证使用 `codex app-server generate-json-schema`，而不是假定 pinned `main` 适用于所有安装版本。[Codex schema generation](https://github.com/openai/codex/blob/2764e83626efe55f64e04d153fc99a157327f3c2/codex-rs/app-server/README.md#L245-L251)
