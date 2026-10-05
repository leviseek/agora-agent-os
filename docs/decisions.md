# Engineering decisions

Every decision below was taken to keep the first version **simple, runnable and replaceable**.
Each records the alternative that was rejected and why.

## D1. A dedicated `crates/kernel` as the composition root

**Decision.** The wiring of concrete implementations lives in one crate (`agentos-kernel`) that
depends on every plane. `crates/api` is a pure gateway, `crates/cli` and `crates/server` are entry
points.
**Rejected.** Putting the wiring into `crates/api` (would make the gateway depend on every plane
*and* be depended on by them) or into `main.rs` (would make it untestable).
**Consequence.** No plane can reach a concrete backend: the only place that knows `FileStore`
exists is the kernel.

## D2. Actor state is JSON, not a process image

**Decision.** `ErasedActor::snapshot` returns `serde_json::Value`; migration is
checkpoint/restore/replay.
**Rejected.** Copying process memory or serializing a stack. Not portable, not versionable, not
cloneable.
**Consequence.** Checkpoints are readable, diffable and hashable. The cost is serialization
overhead, which v1 accepts.

## D3. One mailbox per actor, one actor per session

**Decision.** Ordering inside a session is guaranteed by a single `mpsc` queue per actor; different
sessions are different actors on different tasks.
**Rejected.** A global work queue with per-session sequence numbers: more machinery for the same
guarantee, and it puts a shared lock on the message path.
**Consequence.** A long-running goal occupies its session (by design); cancellation therefore goes
through a `CancellationToken` registry *outside* the mailbox.

## D4. Control plane off the hot path

**Decision.** `SessionManager` consults the in-process actor runtime first and the Actor Directory
only on a miss or after a failure. Both outcomes are counted as metrics.
**Rejected.** Looking up the directory on every message: it would make every message a
control-plane round trip and couple throughput to the directory.
**Consequence.** The directory may be slightly stale; the runtime treats a stale entry as a
recovery trigger rather than an error.

## D5. Protobuf carries JSON payloads

**Decision.** `proto/agentos/v1/*.proto` transports domain objects as opaque JSON strings; only
routing-relevant fields are structured.
**Rejected.** Mirroring the full domain model in Protobuf: every domain change would be a wire
change, and the runtime would carry two definitions of the same type.
**Consequence.** gRPC stays stable while the domain evolves; the cost is that field-level schema
validation does not happen at the wire boundary (it happens in the mesh, against the capability
schemas).

## D6. Wasm capabilities use a JSON-in/JSON-out ABI

**Decision.** Guests export `alloc` and `invoke`, exchange UTF-8 JSON, and may call a handful of
permission-gated host functions.
**Rejected.** WASI preview 1 with a filesystem preopen: it would hand guests a real filesystem
surface before the runtime has a sandbox policy for it, and it pulls in a large dependency.
**Consequence.** The example `echo.wat` is ~40 lines and the sandbox surface is exactly what the
policy grants. WASI can be added later behind the same `Capability` trait.

## D7. Timeouts use epoch interruption, not threads

**Decision.** `Config::epoch_interruption(true)` plus a watchdog task that ticks the engine; each
store gets a deadline computed from its timeout.
**Rejected.** Running each guest on a thread that gets killed: cannot interrupt wasm safely, and
threads are not free.
**Consequence.** Guests are interrupted at the next epoch check (bounded by `epoch_tick_ms`, 50ms
by default), and the runtime never leaks a spinning guest.

## D8. A mock model provider ships in the runtime

**Decision.** `MockProvider` is always registered. The router prefers configured providers and
falls back to the mock.
**Rejected.** Requiring an API key to run anything: it makes tests network-dependent and the
desktop demo unusable offline.
**Consequence.** The whole acceptance suite runs with no network. The mock implements the same
contract including tool calls, so it exercises the real code path.

## D9. Retry decisions live in the error taxonomy

**Decision.** `ErrorKind::retryable()` decides retries; individual errors may override.
**Rejected.** Per-call-site retry booleans: guaranteed drift.
**Consequence.** A capability can mark one failure as `retryable(false)` and the mesh, the
scheduler and the router all honour it without further code.

## D10. Task graphs are data, the scheduler is the only decision maker

**Decision.** `TaskGraphBuilder` validates the DAG (dangling deps, cycles) before anything runs;
`Scheduler` owns concurrency, retry and cancellation.
**Rejected.** Executing the plan directly inside the agent loop: it would duplicate scheduling logic
and make the loop untestable in isolation.
**Consequence.** The same scheduler serves the agent loop, the CLI and any future workflow engine.

## D11. Memory retrieval is a trait from day one

**Decision.** `MemoryStore` with a deterministic local implementation (tag/importance/recency).
**Rejected.** Building a vector index into the runtime before a provider exists.
**Consequence.** `MemoryRecord.embedding` is reserved; a vector store drops in behind the trait
without touching the agent loop.

## D12. The desktop shell reuses the web client

**Decision.** Tauri 2 loads the same `apps/web` build; the Rust side only locates the runtime,
probes health and optionally starts it.
**Rejected.** A separate desktop UI: two UIs to keep in sync, and the desktop would drift from the
browser.
**Consequence.** One UI, two shells; the desktop can point at a remote runtime.

## D13. File store as the default backend

**Decision.** `StoreBackend::File` - one JSON envelope per key, JSONL event logs, atomic renames.
**Rejected.** SQLite: it needs a C toolchain, which is not guaranteed on a fresh Windows machine.
`redb` is available behind a feature for embedded-KV users.
**Consequence.** Default installs need nothing but Rust; the trait keeps the SQLite option open.

## D14. No model output is trusted

**Decision.** The plan JSON is parsed defensively (unknown kinds fall back to `think`, a parse
failure degrades to a direct answer), and capability input/output are validated against JSON
Schema before and after execution.
**Rejected.** Assuming the model returns the requested shape.
**Consequence.** A malformed plan produces a worse answer, never a crashed run.
## D15. Localization: one typed dictionary, English as the source of truth

**Decision.** `apps/web/src/i18n.tsx` holds two flat tables (`en`, `zh`) with namespaced keys
(`nav.*`, `common.*`, `state.*`, `chat.*`, ...). The English table defines the `MessageKey` union,
so a typo is a compile error. Lookup falls back to English and then to the key itself, and runtime
identifiers (lifecycle states, severities) fall back to the raw value - a new backend state can
never render as a blank or as a missing-translation artefact. The active locale is persisted per
browser and seeded from `navigator.language`; `format.ts` reads it through a module-level setter so
dates and numbers follow the language without threading a locale through every call site.

**Rejected.** An i18n library (react-i18next and friends): the console has roughly 250 strings in
two languages, no plurals engine needs, no message extraction pipeline, and no server rendering.
A dependency would have cost more than the whole dictionary. Also rejected: keying the dictionary
by English source text, which silently turns copy edits into missing translations.

**Consequence.** Adding a language is one table plus one entry in `LOCALES`; adding a string is one
line in two tables. Untranslated keys are visible in code review because the `zh` table is typed as
`Partial<Record<MessageKey, string>>` and the fallback is deliberate, not accidental.

## D16. Theming: CSS custom properties, one attribute, no theme objects in JS

**Decision.** Every colour is a CSS custom property defined twice in `styles.css`: once in `:root`
(dark) and once under `[data-theme='light']`. `theme.tsx` only decides which attribute value is on
`<html>` - `light`, `dark`, or `system` resolved through `prefers-color-scheme` with a live
listener - and persists the choice. The 37 colours that had been written inline in the stylesheet
were replaced by semantic tokens (`--bg-chip`, `--border-warn-strong`, `--text-accent`, ...), so no
rule outside the two palette blocks contains a literal colour.

**Rejected.** A CSS-in-JS or utility-framework theme object: it would move colour decisions into
components, which is exactly the coupling the palette blocks exist to prevent. Also rejected:
preprocessor variables, which cannot be switched at runtime without a rebuild.

**Consequence.** A component changes appearance without knowing that themes exist, the two palettes
cannot drift (every token has a counterpart in both), and a third theme is one more block. React
Flow's `--xy-*` variables are mapped onto the same tokens so graph views follow the switch too.

---

## D17. Discovery defaults to a shared directory, not to the network

**Decision.** Nodes find each other through a per-user directory of small JSON advertisements
(`%LOCALAPPDATA%\agora-agent-os\nodes`, `$XDG_RUNTIME_DIR/agora-agent-os/nodes`, ...), written
atomically, refreshed on a heartbeat, expired by TTL, and read by `LocalFileDiscovery`. It is on by
default. The libp2p/mDNS backend implements the same `NodeDiscovery` trait and is chosen by the
composition root.

**Why.** The case that matters first is two checkouts of this repository running on one machine as
two processes owned by the same user. A directory is a rendezvous for exactly that case: no
multicast, no firewall exception, no bootstrap list, no configuration at all - and it is
inspectable with `dir`, which is what an operator needs when a node does not show up. mDNS is the
right answer for "a node on another machine", and it costs a libp2p dependency and a network
surface that a single-machine setup should not have to pay for.

**Rejected.** mDNS-only: it is the higher-friction default (multicast is often blocked, and it
answers a question the user did not ask). A central registry service: it reintroduces exactly the
control-plane-on-the-hot-path coupling the architecture avoids. UDP broadcast: same reach as the
directory, none of the inspectability.

**Correction.** The first implementation resolved a node's identity to its *name* when
`AGENTOS_NODE_ID` was unset. Since every node defaults to the same name, two checkouts wrote one
advertisement, each skipped it as its own, and zero-configuration discovery - the entire point -
did not work. Identity is now generated on first start and persisted in `<data_dir>/node.id`:
unique per instance, stable across restarts, and independent of the label. Names are for humans;
the console appends a port when two nodes share one.

**Consequence.** Discovery is a hint, never a fact: a stale advertisement expires, a corrupt file is
skipped, and a hostile node id cannot escape the directory (it is hashed). Nothing on the request
path depends on it - `/v1/nodes` reads a cached view that a background heartbeat maintains, and
join/leave are ordinary events on the bus. Two running instances must still not share a data
directory; discovery shares knowledge, not state.

## D18 — 配置文档允许缺省（可部分给出）

给配置结构体新增字段，过去等同于对所有部署的破坏性变更：磁盘上的配置文件不再能反序列化，运行时以
"missing field ..." 拒绝启动。仓库里随源码发布的示例配置**已经这样失效了**——它缺 `approval_timeout_ms`，
运维照抄示例会得到一个起不来的运行时。

现在每个配置结构体都为其字段派生默认值，缺失的字段回落到默认；**存在但类型错误的值仍然被直接拒绝**——
这正是关键之处：运行时容忍为旧版本写的文档，不容忍胡说。两条测试钉住这件事：随仓库发布的示例必须能解析
**并且**通过校验；部分文档必须能加载，而写错的值必须不能。

## D19 — 身份：有凭证的节点只认凭证，公开节点才允许自报

**决策。** 一个节点只有两种身份制度，边界是一个问题：`ApiConfig::has_authenticated_identities()`
（有静态令牌或有 principal 表）。有凭证时，身份**只**来自 bearer token，无法识别的 token 直接拒绝；
`X-Agora-User` / `?user=` 在有凭证的节点上**故意忽略**——若采纳，principal 表就成了摆设，任何人报个
名字即可绕过 token。没有凭证的节点是公开节点，由 `api.asserted_identity` 决定自报名的含义：
`off`（忽略，仍是 operator）、`optional`（默认：给了就用，没给就是 operator）、`required`（没给就 401）。

自报名走 `X-Agora-User`（可带 `X-Agora-Node`），WebSocket 握手用 `?user=` / `?node=`（浏览器不能设头）。
字符集限定为字母数字与 `._-`、上限 64、保留字 `operator`；自报拿到的角色是 `creator` 而非 `admin`。
`GET /v1/meta` 在任何身份之前就报告模式（`identity.authenticated` / `identity.asserted` / `separation`），
`/v1/auth/whoami` 回报 `source`（`token` / `asserted` / `operator`）。

**为何。** 归属（工作区归创建者、按工作区分权）在公开节点上需要一个「人」，而公开节点上没有任何可验证的
东西。两条路：要么给节点加账号体系（能验证，但加入 = 注册 + 凭据存储 + 找回），要么承认自报只是**署名**。
多数部署（本机、内网、P2P 信任域）只需要后者，把它们都逼上账号体系比问题本身更贵。因此默认 `optional`：
不改变任何现有部署（没有客户端会发这个头，缺省仍是 operator），而想要真正多人的节点显式开 `required`。

**拒绝了什么。** 只做账号体系：把一个「先能用」的运行时卡在注册页后面，且要引入凭据存储、口令散列与找回流程，
都在第一个真实用户之前。只在有表时允许自报：那会让 token 变成建议，越权只需换个头。把自报写成 admin：
一个没人核实的名字若能通过 `decide()` 的 admin 旁路，隔离就只剩表演。

**代价（必须显式说明）。** 自报**不是认证**：`optional` 下「不发头」仍等于 operator，所以公开节点要真正隔离
必须开 `required`。`required` 下任何人可报任意名字，因此它只适合信任域或配合别的准入（反代鉴权、
`AGENTOS_PRINCIPALS`）使用。控制台在连接页显式展示当前模式与解析结果（`source`），
好让「为什么两份会话互相看得见」在页面上就有答案，而不是靠猜。

**后续。** 真正的账号体系（注册 / 登录 / 凭据）不在此决定内：它落在一个新节点入口上，与自报共存——
有凭证时自报仍旧被忽略，这条边界不因账号体系引入而改变。

## D20 — 工作区是归属、共享与目录隔离的单位

**决策。** 引入一等实体 **工作区**（`WorkspaceRecord`，id `ws_*`）。会话属于工作区而不是直接属于人：
`SessionRecord.workspace_id`、`create_session_in(workspace, ...)`，且会话记录的 `owner` 与工作区的
`owner` 保持一致（谁拥有工作区，谁就拥有其中所有会话）。权限判定改为**经工作区解析**：
`effective_role(record, workspace, principal)` —— 会话有工作区时，角色取工作区（`workspace_role`），
会话自身的 grants 不再参与；没有工作区的旧记录沿用原来的会话级 owner/grants 读法，所以升级不会让已有
会话答错。目录隔离：一个节点一个根（`policy.workspace_root`），每个工作区一个目录
`<root>/<ws_id>/`，由 `WorkspaceRegistry` 按 `CallerContext.workspace` 在**每次能力调用**时解析成
`Workspace` jail；mesh、策略门、审计看到的是同一个目录，不可能各说一套。目录名用 **id 不是名字**，
改名不移动文件，两个工作区也不会因重名撞目录。

**为何。** 「共享一份完整工作能力」如果不能一次决定，就会退化成"每个会话都点一遍授权"，而漏掉的那个
会话就是共享边界的漏洞。把归属与授权抬到工作区，共享是一个决定，隔离是一个目录：会话归属、成员、能力收窄
都挂在工作区上，文件系统也按同一单位切开，权限与目录边界重合，审计才说得清"他能碰什么"。

**拒绝了什么。** 把 `workspace_root` 直接当每个工作区的目录（会用一个根承载所有工作区，等于没有隔离）；
把会话 grant 继续当主授权（两个授权来源必然漂移——工作区删了成员，会话还在放行）；`Some(id)` 之外再用
可读 slug 当目录名（改名即搬文件，且可能指向旁人的目录）。也没有做「旧会话自动收编进默认工作区」：那会在
升级时批量改写记录并改变文件可见位置，风险大于收益，留作显式迁移（见下）。

**代价 / 已知过渡态（必须明说）。**
1. **没有工作区的旧会话**（`workspace_id = None`）仍把节点根 `policy.workspace_root` 当作 jail，因此理论上
   能顺着 `ws_*/…` 读到某个工作区目录内的文件。这是升级期的一处洞，只在「会话早于工作区」时存在；
   网关一旦总是分配工作区（第 3 阶段）就只剩历史记录，随后用显式迁移补齐。
2. **gRPC 远程能力调用**（`KernelCapabilityHandler`）的线上契约还没有 `workspace_id` 字段，因此该路径
   `workspace = None`，落到同一个根。第 3 阶段给 proto 加字段后收口；目前它是唯一不受工作区约束的入口。
3. **能力收窄仍在会话级**（`SessionCapabilities` 按 session 存）。工作区级的能力收窄（"共享完整工作能力"的
   另一半）随网关阶段一起搬到工作区，本阶段不动，避免半接线的字段。

**验证。** 单测：`workspace_role` 的拥有/授权/节点限定/最佳授权取胜、目录名只认 id；
`WorkspaceRegistry` 的 A/B 各自 jail、相对路径越界被拒。端到端（`tests/tests/acceptance.rs`）：
`a_workspace_owns_its_sessions_and_shares_them_with_one_decision`（一条工作区授权 = 其内所有会话生效，
含之后新建的会话）、`one_workspace_cannot_reach_another_workspace_files`（同一路径在 B 里 `not_found`，
`../` 越界 `policy_denied`）。

**后续（网关阶段，2026-10-05）。** 上述代价逐条收口：
1. 网关每次建会话都必须落到一个工作区——没点名就用目标用户的**默认工作区**（首次自动建，`metadata.default=true`），
   所以「每个会话都有工作区」从这一层起就是不变式，只剩升级前写入、`workspace_id` 为空的历史记录仍走根目录。
   分支（`branch`）继承源会话的工作区；从归档恢复（`restore_archive`）落到恢复者/原主的默认工作区。
2. gRPC 的 `InvokeRequest` 增加 `workspace_id`（field 9，向后兼容），`KernelCapabilityHandler` 据此设定
   `CallerContext.workspace`，远程能力调用与本地同样受 jail 约束。仍有缺口：`CapabilityTransport::call`
   这一层的签名根本不携带调用者（连 session 都没有），经它发起的能力调用仍落根目录——线上字段已备好，等这条
   缝被拓宽。
3. 能力收窄移到工作区：`WorkspaceRecord.capabilities` + `SessionCapabilitySource::for_scope(session, workspace)`，
   网关新增 `PUT /v1/workspaces/{id}/capabilities`，会话级路由写的是工作区。会话记录上的旧 `capabilities` 仅对
   无工作区的历史记录仍然生效。
4. 会话级授权/申请路由改为**委派**工作区：`/v1/sessions/{id}/access`、`.../access-requests`、`.../capabilities`
   照旧可用，响应带 `scope: "workspace"` 与真正变更的 `workspace` 记录；`decide` 通过 `grant_workspace` 落地，
   所以审批与手写授权产出同一种东西。跨工作区收件箱 `/v1/access-requests` 同时汇总工作区与历史会话请求。
5. 守卫的 ACL 改为两个域：`RequiredAccess::Session`（经 `decide_in` 读工作区）与 `RequiredAccess::Workspace`
   （`decide_workspace`）。工作区动作复用同一张角色表——读=Read、在工作区建会话=Chat、改名/收窄/授权=Grant——
   不为工作区再写第二张表，正是「同一角色在两处漂移」的源头。

验证：`cargo test -p agentos-tests --test api` 覆盖工作区 HTTP 全链（建/列/授权/审批/降权/收窄/改名，
40 passed）；`one_workspace_cannot_reach_another_workspace_files` 与
`a_workspace_owns_its_sessions_and_shares_them_with_one_decision` 在 acceptance 层钉住目录隔离与一条授权生效。

**后续（控制台阶段，2026-10-05）。** 新增 `工作区` 视图（列表/创建/切换、成员授权与撤销、能力收窄编辑），
会话列表按选中工作区分组并显示 `工作区` 列，新建会话默认落到选中的工作区（未选则「我的默认工作区」），
访问申请页改为按「工作区或会话」区分目标：审批走工作区路由，申请人的列表也按工作区展示。
能力编辑器现在一个组件两种作用域（`sessionId` / `workspaceId`），并显式标明「这收窄的是整个工作区」。
验证方式与仓库既有做法一致：真实浏览器（headless Chrome over CDP）跑一遍
`scripts/cdp-workspaces.mjs`——连接 → 建工作区 → 在其中建会话 → 列表里该会话的「工作区」列显示该工作区，
且无浏览器侧异常。

**后续（归档与验证，2026-10-05）。** 归档从「按 owner 分目录」改为「按工作区分目录」：包落在
`<archive_dir>/<ws_id>/<ses>-<ts>.zip`，`manifest.json` 增加 `workspace_id`（旧包缺失即 `None`），
`list_packages` 本来就只读两层目录，因此列表/查找/预览/删除无需改动，旧包也不会被搬动。
恢复（`restore_archive`）优先放回原工作区（该工作区在本节点仍存在时），否则落到恢复者/原主的默认工作区——
指向一个只存在于来源节点的 id 会让会话无主。这样「这个工作区的数据」在存储、工作区目录、归档根三处对齐。
`archiving_writes_a_package_and_restoring_brings_it_back` 新增两条断言钉住：包的父目录名等于会话的 `workspace_id`，
且清单里的 `workspace_id` 与之一致。
