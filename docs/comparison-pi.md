# pi 与 agora-agent-os 会话能力对比

> 调研日期：2026-10-05 · pi 版本 1.0.2（本机 @earendil-works/pi-coding-agent）· agora-agent-os 提交 8fd9378
>
> **方法**：pi 一侧来自本机安装的 43 篇打包文档、270 个未打包自研模块、真实会话数据（~/.pi/agent/sessions 下 126 个 .jsonl）、以及 pi --help / pi config|mcp|auth|list|install --help 的实跑输出；agora-agent-os 一侧来自仓库代码（行号见正文）。所有"有/无"结论都有出处，无法确认的一律标注。

---

## 0.0 勘误（本报告自身的错误）

- **初版误判"记忆从未接线"**：初版用单行 grep（`memory\.(write|recall)`）检索，而调用是跨行书写的（`self.deps\n    .memory\n    .write(...)`），因此漏掉了两个写入点。**实际情况是"只写不读"**：写入已接线（session.rs:284、agent_loop.rs:323），召回从未被调用。§2 与 §7 的对应行已更正。教训：跨行调用的检索不能只用单行匹配。

## 0.1 证据的边界（先说不可靠的地方）

- **"文档里有"不等于"生产在用"**：pi 的自动压缩与分支摘要在文档与代码中确凿存在，但本机 126 个会话的条目直方图是 message 35706 / custom 162 / thinking_level_change 140 / model_change 136 / session 126 / custom_message 87 / **context_edit 13** / session_info 2，而 **compaction 0、branch_summary 0**——只有溢出恢复留下的 13 条 context_edit 痕迹。也就是说：这两套机制在该用户的真实负载下从未触发过。**这直接影响下面的优先级判定：压缩（A2）的紧迫性应低于"历史进请求"（A1）与"预算兜底"。**
- 对比基于**本机安装的 pi 1.0.2**；未做版本考古（CHANGELOG 612KB 未读），未覆盖 dist/bundle 内的第三方 chunk。
- pi 一侧的会话能力**全部是本地单文件形态**：无服务端、无会话锁、无跨进程编排。因此"分布式会话语义"这一栏 pi 没有可比对象，不能据此说我们领先或落后。
- agora-agent-os 一侧的结论均可在仓库代码中复核，关键处已标注文件与行号。

## 0. 先说定位差异，否则对比会失真

| | pi | agora-agent-os |
|---|---|---|
| 是什么 | **本地编码智能体产品**（终端 TUI，单机单进程一会话） | **分布式 Agent 运行时**（面向多节点、多会话、可迁移的 Actor 基础设施） |
| 会话是什么 | 一个 JSONL 树文件 = 一次人机对话 | 一个 **Actor**，有状态机、邮箱、检查点、可迁移 |
| 谁提供 UI | pi 自己（TUI + 快捷键 + 主题） | Web / Tauri 控制台（消费运行时的事件流） |
| 安全模型 | **明确不做沙箱与逐次审批**（docs/security.md:3、:99） | wasm 沙箱 + 工作区 jail + 能力策略引擎 |

结论先行：**pi 在"对话本身"上成熟得多；agora-agent-os 在"运行时的骨架"上领先**。缺的东西按"是否属于运行时职责"分三类看，才不会把产品功能误当成架构缺陷。

---

## 1. 会话生命周期

| 能力 | pi | agora-agent-os | 差距性质 |
|---|---|---|---|
| 自动持久化 | 有（docs/sessions.md:7，本机 126 个会话） | 有（检查点 + 事件日志） | 平 |
| 会话内严格有序 / 会话间并发 | pi：一会话一进程，无编排器；多进程写同一 .jsonl **无锁、行为未定义** | **运行时级**：Actor 邮箱保证会话内有序，会话间真并发 | 我们领先 |
| 恢复 | -c 续、-r 选、--session/--session-id 直开、/resume | 检查点恢复 + 事件回放（session_manager.rs:305-330） | 我们更重，pi 更好用 |
| 分支 | **树模型**：id/parentId，/tree /fork /clone /import | snapshot / restore（restore 会新建 Actor ≈ 粗粒度 fork）、migrate | **pi 领先**（数据模型层） |
| 命名 | /name、-n、选择器 rename | 仅创建时 title | **pi 领先**（客户端功能） |
| 删除 | 选择器 Ctrl+D（可走 trash）、删文件 | close（终态，不删数据） | pi 领先（语义不同） |
| 导出/分享 | /export（HTML/JSONL）、/share（私有 gist） | 无（仅有快照 JSON） | **pi 领先** |
| 临时会话 | --no-session | 无（总是持久化） | pi 领先（小） |
| 回滚/undo/checkpoint | **不存在**（全库扫描：rewind/checkpoint 命中皆为无关代码） | 有检查点快照与恢复 | **我们领先** |

## 2. 上下文管理（差距最大的维度）

| 能力 | pi | agora-agent-os | 差距性质 |
|---|---|---|---|
| 历史进模型请求 | 有：按 token 预算装配整棵会话树 | **无**：agent_loop.rs:270/402-412 只发 system + 当前 goal（外加本次 run 的 tool 消息） | **核心缺口** |
| 上下文窗口感知 | 有：footer 实时占用、getContextUsage、tokensBefore | 无 | 核心缺口 |
| 自动压缩 | 有：contextTokens > window - reserve（默认 reserve 16384 / keep 20000），compaction 条目落盘 | 无 | 核心缺口 |
| 溢出恢复 | 有：provider 溢出或截断后压缩重试；context_edit 追加式改写历史（本机 13 条） | 无 | 核心缺口 |
| 摘要式压缩 | 有（/compact 可带保留指令，可被扩展拦截/自定义） | 无 | 核心缺口 |
| 长期记忆 | **明确没有**（memory 34 处全是 in-memory；无跨会话记忆、无向量检索） | **只写不读**：每次运行写两条（session.rs:284 会话级 episode、agent_loop.rs:323 答案级 semantic），但 recall/recent 仅存在于 trait/实现/测试，运行时从不调用 | 双方都缺；我们是"写了没人看" |
| 上下文文件 | AGENTS.md / CLAUDE.md 自动加载、SYSTEM.md 替换、APPEND_SYSTEM.md 追加 | 无（AgentSpec.system_prompt 是静态字符串，core/src/model/agent.rs:11） | **pi 领先且便宜可追** |
| 技能/提示模板 | skills 渐进披露、prompts 变 slash 命令 | 无 | pi 领先 |
| 提示缓存计量 | 有：5 分钟 TTL、1024 token 噪声阈值、缓存浪费统计 | 无 | pi 领先（成本优化） |

## 3. 工具与扩展

| 能力 | pi | agora-agent-os |
|---|---|---|
| 内置工具 | read / bash / powershell / edit / write / grep / find / ls（默认 read,bash,edit,write） | echo / calculator / clock / filesystem-list / -read / -write（整文件）+ remote 接缝 |
| 文件编辑 | 有 edit（补丁式） | **无补丁式编辑**（只能整文件写） |
| 代码检索 | 有 grep / find | **无** |
| 执行命令 | 有 bash / powershell（**无沙箱**，security.md:99） | **无 shell 能力**；但策略默认拒绝 process_exec，且已有 wasm 沙箱与工作区 jail |
| 工具白名单 | --tools / --exclude-tools / defaultTools 增量语法、只读模式 | 能力策略引擎（allow/deny 列表） |
| MCP | 完整客户端：mcp.json、pi mcp add/list/login（OAuth）、资源、注解透传给权限判断 | 无（自有能力协议：gRPC 远程能力 + wasm + 内置） |
| 扩展体系 | 27 类生命周期钩子、可改写消息/阻断工具、自定义工具与命令、npm/git 包 | 事件总线（只读观测）、策略引擎（静态）、能力注册表（工具扩展入口） |
| 子代理 | **不存在**（README 明说不做） | 任务图并行（会话内）；无"派生带独立上下文的子代理" |
| plan mode | **不存在**（由扩展提供） | **我们内建**：Goal → Plan(JSON) → 任务图 → 观察 → 总结 |

## 4. 安全与权限

| | pi | agora-agent-os |
|---|---|---|
| 逐次审批 | **没有**（security.md:3 原文：does not ask for approval before every tool call；由扩展实现范式） | 没有（静态策略 + 沙箱） |
| 沙箱 | **没有内置沙箱**（security.md:99 明确划出安全边界；替代方案是容器/VM） | wasm 沙箱 + epoch 超时 + 工作区 jail + 能力策略 |
| 信任模型 | 项目信任（.pi 下的设置/扩展/技能需信任；承认 sessionDir 在信任解析前被读是个缺口） | 不加载项目内代码，故无同类风险；但外部能力来源（MCP/远程 worker）尚无信任分级 |
| 凭据 | auth.json 明文存储（proper-lockfile 保护） | 配置只存**环境变量名**，密钥不入库（core/src/config.rs 注释与测试） |

## 5. 成本与可观测

| 能力 | pi | agora-agent-os |
|---|---|---|
| token 计数 | 每条消息带 usage（本机 usage 条目落盘） | 仅 provider 聚合（router.rs:48-56/259）+ Prometheus 计数器；**run / session 维度没有** |
| 费用统计 | 有：createUsageTotals{cost}、按 provider/model 分账、/session 汇总 | 无（无定价概念） |
| 缓存浪费 | 有 | 无 |
| 上下文占用显示 | footer 实时 | 无 |
| 诊断 | crash-log、diagnostics、/bug（含环境与 provider 配置，不含凭据） | tracing + /v1/metrics + 结构化事件流（seq、可过滤、可回放） |

## 6. 交互体验

| 能力 | pi | agora-agent-os |
|---|---|---|
| 流式输出 | 有（message_start/update/end 增量） | 无（请求/响应；进度靠事件：agent_step / task_* / tool_*） |
| 多模态 | 有：@path 附图、read 读图、剪贴板粘贴、工具结果回图 | 无（ContentPart 仅文本） |
| 输出模式 | text / json / rpc 三种 | HTTP + WebSocket（命令面：ping/goal/cancel/snapshot/health） |
| 转录内搜索 | 有 | 无（浏览器自带） |
| 消息队列 | follow-up + dequeue | 邮箱天然排队（会话内严格有序），**无撤回** |
| 会话内切模型/思考等级 | 有（落盘为条目） | run 级 model hint，无会话内切换 UI |
| 并行工具调用 | 有（同批并行 + 文件写串行化） | 任务图并行（DAG + 依赖） |
| 快捷键/主题/Mermaid | 终端产品级 | 控制台级（i18n 中英 + 亮暗 + React Flow 拓扑/任务图） |

---

## 7. 逐条判定：值不值得实现

**判定口径**：①是否属于运行时职责（否则归客户端）②是否为本项目目标的必要条件 ③与既有架构是否冲突 ④成本。

### 第一批：让"会话"名副其实（小而直接，强烈建议）

| 项 | 判定 | 理由 | 工作量 |
|---|---|---|---|
| **A1 历史进模型请求** | **值得，最高优先** | 现在每轮 goal 都从零开始（agent_loop.rs:402-412），"会话"实为任务容器而非对话。这是任何多轮 agent 的地基；不修，后面所有上下文能力都无处附着 | 小（装配 + 截断） |
| **A3 run/session 级 token 记账** | **值得，便宜** | provider 已返回 usage；把它记到 AgentStep/Run 并在会话汇总，是费用、配额、以及设计里"预留支付接口"的前提 | 小 |
| **A4 记忆召回接线** | **值得** | 写入已接线（每运行两条），但**召回从未被调用**——写进去的记忆没有任何读者，等于只写不读。补上"规划前按会话召回并注入"即可闭环；这也是我们对 pi 的**差异化**（pi 明确没有长期记忆） | 小-中 |
| **B4 上下文文件装配** | **值得** | pi 的 AGENTS.md/SYSTEM.md 机制是"让 agent 好用"的关键且成本低；我们有工作区 jail，天然有边界 | 小-中 |

### 第二批：上下文可持续 + 工具够用

| 项 | 判定 | 理由 | 工作量 |
|---|---|---|---|
| **A2 上下文预算与压缩** | **值得，A1 之后必做** | 有了历史就必须有窗口管理；建议分两步：先"预算 + 截断"（必做），再"摘要压缩"（需额外模型调用，可后置） | 中 |
| **B1 补 edit/grep 工具** | **值得** | 补丁式编辑与代码检索是编码类 agent 的刚需，且天然是能力插件 | 小-中 |
| **B1' shell 能力** | **谨慎值得** | 价值极高但风险最大。正确做法：作为显式能力 + 策略门 + 沙箱（与现有 process_exec 默认拒绝一致），**不要**学 pi 直接给 bash 无沙箱 | 中 |
| **A5 导出 / 恢复为新会话** | **值得（分档）** | restore 已能"从快照派生新 Actor"，把它暴露成"另存为分支"成本很小；导出 JSON/Markdown 也便宜。**会话树**是新的数据模型，取决于是否真要多分支探索 | 小→大 |
| **A6 会话 rename / 搜索** | **值得（低优先）** | 纯客户端，便宜 | 小 |

### 第三批：生态与体验（按需）

| 项 | 判定 | 理由 | 工作量 |
|---|---|---|---|
| **B2 MCP 适配器** | **值得** | Capability trait 让"MCP server → 能力"是干净接线，能直接吃生态工具。但必须定信任分级（外部进程） | 中 |
| **E1 流式输出** | **中等** | 改善首字延迟体验；对事件总线是自然扩展（增量事件），但牵动 provider 适配层与前端渲染。可延后 | 中 |
| **E2 多模态** | **中等偏低** | 类型层（ContentPart::Image）便宜，端到端（provider/UI/存储）中等；取决于是否要做视觉 | 中 |
| **D2 诊断包** | **中低** | 分布式系统里"一键导出 health + 事件 + 脱敏配置"很实用，成本小 | 小 |
| **C1 审批回路** | **低** | pi **也没有**逐次审批，不构成对标缺口。若要做，需要"待审批"状态机 + UI 回路，属于策略面增强 | 中 |
| **A5' 消息撤回** | **低** | 邮箱已保证排队，撤回是锦上添花 | 小 |

### 明确不建议做

| 项 | 理由 |
|---|---|
| **B3 照搬 pi 的进程内扩展钩子** | 会在运行时里执行用户/项目提供的 JS，破坏我们的沙箱与"不可信输入"原则。正确形态是**沙箱化的能力/策略插件**（已有架构的自然延伸） |
| **B5 codemode / tool_search** | pi 的产品特有交互，与运行时职责无关 |
| **C3 照搬项目信任模型** | pi 需要它是因为会加载项目内扩展/技能；我们不加载项目内代码，风险面不同。真正需要的是"外部能力来源的信任分级" |
| **D1 缓存浪费计量** | 现阶段无真实成本压力；需要 provider 透传 cache 字段，收益要等用量上来 |
| **E3 TUI 细节（快捷键/终端渲染）** | 形态不同；我们的等价物是控制台交互，已具备 i18n + 主题 + 图视图 |

---

## 8. 我们有而 pi 没有的（避免单向视角）

- **分布式**：节点发现（同机零配置 + 可换 libp2p）、放置服务、worker 租约与心跳回收、gRPC 远程能力调用、**Actor 迁移**（检查点→快照→传输→恢复→回放→续跑）。
- **沙箱与策略**：wasm 能力沙箱（epoch 超时）、工作区越界防护、能力策略引擎、JSON-Schema 校验、超时/重试。
- **结构化事件总线**：seq、过滤、可回放，加 Prometheus 指标——pi 只有 JSONL 转录。
- **运行时级多会话并发**：pi 无编排器（多进程写同一会话文件无锁）。
- **类型化 Actor**：邮箱有序 + 独立取消令牌（取消绕过邮箱，不会被正在跑的运行阻塞）。
- **控制台**：拓扑、任务图 DAG、能力、事件视图；中英双语 + 亮暗主题。

---

## 9. 建议路线（三批）

1. **第一批（会话语义奠基）**：A1 历史进请求 → A3 token 记账 → A4 记忆召回 → B4 上下文文件。
2. **第二批（可持续 + 够用）**：A2 预算/压缩 → B1 edit+grep（+ shell 视策略）→ A5 导出/派分支 + A6 rename/搜索。
3. **第三批（生态与体验）**：B2 MCP → E1 流式 → E2 多模态 → D2 诊断包 → C1 审批回路。

---

## 10. 实施进度

> 每完成一项在这里记一行，附带可复核的证据（测试名 / 命令 / 提交）。未完成的不写。

| 项 | 状态 | 证据 |
|---|---|---|
| **A1 历史进模型请求** | ✅ 完成 | `history_for_model`（agent_loop.rs）+ 5 个单测；配置 `policy.history_messages`(20)/`history_chars`(8000)；实测两轮对话：Plan 请求 messages 2 → 4，事件 `conversation history assembled` 显示 history_messages 0 → 2 |
| **A3 run/session token 记账** | ✅ 完成 | `TokenUsage`（core/model/agent.rs，3 个单测）；`UsageMeter` 汇总 plan/finalise/并行任务三类调用；`/v1/sessions/{id}` 暴露 `runtime.usage` 与 `runs[].usage`；契约测试断言 calls ≥ 2、tokens > 0、会话合计 = 单次运行；控制台显示每轮 tokens 与会话累计 |
| **A4 记忆召回接线** | ✅ 完成 | 写入统一为每轮一条可解析记录（`goal:`/`answer:`，session.rs），删除 agent_loop 里重复的 semantic 写入；`recall_context` 按 goal 与可见历史去重后注入（6 个单测）；配置 `policy.memory_recall_limit`(5)/`memory_recall_chars`(1200)；实测（历史窗口关闭）`memory_recalled` chars=282、Plan 请求 2 → 3 条 |
| **第一批整体验证** | ✅ 完成 | 真实浏览器（1440×900，两轮对话）：每轮 run 头显示「424 tokens，3 次模型调用」、会话徽章「本会话累计 848 tokens」、事件流出现 `conversation history assembled` 与 `context_loaded`；`context_loaded` 每轮 1 次、`memory_recalled` 为 0 **符合设计**（历史窗口已覆盖的轮次不重复注入，召回只在窗口之外起作用——已用历史关闭的对照实验单独验证）。截图 docs/screenshots/console-batch1-zh.png。全量 159 passed / 0 failed |
| ⚠️ 环境变量嵌套 bug | ✅ 已修 | `AGENTOS_HISTORY_*` / `AGENTOS_MEMORY_RECALL_*` 四个覆盖曾被嵌进 `AGENTOS_MAX_STEPS` 块内而静默失效；已移到顶层并加 `context_switches_apply_from_the_environment_alone` 测试钉死 |
| **A6 会话改名/搜索** | ✅ 完成 | 改名走 Actor（新 `SessionMessage::Rename` + `SessionRenamed` 事件），因此运行时状态与存储不会漂移——契约测试专门断言了这一点；列表支持 `?q=`（标题/用户子串，不分大小写）；控制台会话页加搜索框与行内改名。实测（真实浏览器）：输入 second 立即筛出 1 条、清空恢复 3 条、行内改名后标题与列表行同步更新，无 JS 异常。截图 docs/screenshots/console-sessions-search-zh.png |
| **第二批整体验证** | ✅ 完成 | A2 压缩、B1 edit/grep、A5 分支/导出、A6 改名/搜索全部落地并各自实测；全量 177 passed / 0 failed |
| **A5 会话导出/派分支** | ✅ 完成 | `SessionManager::branch`（快照 → 改写 actor/session id 与状态内所有 session 引用 → 恢复为新 Actor；刻意不复制 memory，因为记忆属于会话，派生分支应能干净地试错）+ `export`（新模块 export.rs 的 Markdown 渲染器，2 个单测；JSON 为结构化投影）。路由 `POST /v1/sessions/{id}/branch`、`GET /v1/sessions/{id}/export?format=json\|markdown`（未知格式 400，不静默回退）。2 个契约测试 + 实测：分支继承 2 轮后独立演进（原会话不变、分支 4 轮），Markdown 含前页/对话/Runs（每轮 provider 与 tokens），JSON 含会话总开销 848 tokens / 6 calls |
| **B1 edit/grep 能力** | ✅ 完成 | 新模块 `capability-runtime/src/file_tools.rs`：`filesystem-edit`（按唯一片段打补丁；重复片段拒绝并提示加长上下文或 `replace_all`；越界走 jail；同 write 一样需显式授权）与 `filesystem-search`（手写 glob `*`/`**`/`?` + 大小写不敏感子串，按结果数/文件数/文件大小设限，跳过二进制与隐藏目录）；**不引入 regex 依赖**（已在 capabilities/README.md 明确记录该限制）。8 个单测 + 网关实测：search 命中行号、edit 唯一替换成功、重复片段 400 且文件不变、越界 403、未授权 403 |
| **B4 工作区上下文文件装配** | ✅ 完成 | `load_workspace_context`（新模块 context.rs，5 个单测：缺失文件不报错、按序加载、预算截断、越界文件被拒、空/重复条目忽略）；配置 `policy.context_files`/context_files_chars + 两个 env；每轮发 `context_loaded` 事件；实测：`AGENTS.md` 进入 Plan 请求（messages 3，含 project 指令），配置的 `../secret.md` 被 jail 拒绝且内容从未进日志或提示 |
| **A2 上下文预算与压缩** | ✅ 完成 | 预算兜底由 A1 的 `history_messages/chars` 提供；压缩新增 `compaction_window`（4 个单测：窗口内无事、只压掉出窗口的部分、余量过小不花模型调用、水位线不回退）+ `maybe_compact()`（每轮开始前压缩 → 摘要在同一轮即可被召回；失败时水位线不动以便重试）；摘要以高 importance 的 memory 记录存储；水位线与摘要开销进 Actor 持久状态（重启/迁移不重复付费）；配置 `policy.compaction_enabled`/`compaction_min_messages` + 2 个 env；`session_compacted` 事件 + `runtime.compaction_usage`。实测：水位线 0→2→4 两次摘要各 72 tokens，召回 chars 634→1207，会话总计 1840 = 4×424 + 144 |
